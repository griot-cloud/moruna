//! Native acceptance only: actual object RPC, charged Memory and the NVMe owner.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{format, string::String, vec};
use moruna_storage_format::{
    Object,
    auth::{Admission, Proof, Session, SessionRole},
    wire::{self, Operation, Outcome, Refusal, Request, Response},
};
use moruna_storage_service::protocol::{admission, object};
use moruna_svc::{Account, Channel, Console, Memory, Rights, deadline_in, map, println, start};
moruna_svc::main!(run);
const WAIT: u64 = 30_000_000_000;
const BYTES: &[u8] = b"moruna-native-durable-nvme-proof\n";
fn why(e: impl core::fmt::Debug) -> String {
    format!("{e:?}")
}
fn memory(account: &Account, dma: bool, bytes: &[u8]) -> Result<Memory, String> {
    let memory = if dma {
        Memory::create_dma_in(wire::MAX_HEADER as u64, &account.0)
    } else {
        Memory::create_in(wire::MAX_HEADER as u64, &account.0)
    }
    .map_err(why)?;
    memory
        .map(0, wire::MAX_HEADER as u64, map::WRITE)
        .and_then(|view| view.write(0, bytes))
        .map_err(why)?;
    Ok(memory)
}
fn loan(memory: &Memory, write: bool) -> Result<Memory, String> {
    let rights = Rights::READ | Rights::MAP | Rights::TRANSFER;
    memory
        .0
        .duplicate(if write {
            rights | Rights::WRITE
        } else {
            rights
        })
        .map(Memory)
        .map_err(why)
}
fn read(memory: &Memory, length: u32) -> Result<alloc::vec::Vec<u8>, String> {
    if length as usize > wire::MAX_HEADER {
        return Err("oversized reply".into());
    }
    let mut bytes = vec![0; length as usize];
    memory
        .map(0, wire::MAX_HEADER as u64, map::READ)
        .and_then(|view| view.read(0, &mut bytes))
        .map_err(why)?;
    Ok(bytes)
}
fn bind(
    binder: &mut admission::Client,
    account: &Account,
    granted: &Admission,
) -> Result<object::Client, String> {
    let mut bytes = vec![0; wire::MAX_HEADER];
    let used = granted.encode_into(&mut bytes).map_err(why)?;
    let control = memory(account, false, &bytes[..used])?;
    let rights = Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER;
    let channel = binder
        .bind(
            loan(&control, false)?,
            used as u32,
            account.0.duplicate(rights).map_err(why)?,
        )
        .map_err(why)?;
    let mut client = object::Client::connect(channel, deadline_in(WAIT)).map_err(why)?;
    client.core().set_timeout(WAIT);
    let reply = memory(account, false, &[])?;
    let (used, identity) = client
        .session(loan(&reply, true)?, wire::MAX_HEADER as u32)
        .map_err(why)?;
    if !account.matches(&identity).map_err(why)? {
        return Err("foreign bound Account".into());
    }
    let facts = Session::decode(&read(&reply, used)?).map_err(why)?;
    if facts.admission != *granted {
        return Err("wrong bound scope/role".into());
    }
    Ok(client)
}
fn execute(
    client: &mut object::Client,
    account: &Account,
    request: Request<'_>,
    payload: Option<&Memory>,
    writable: bool,
) -> Result<Response, String> {
    let mut bytes = vec![0; wire::MAX_HEADER];
    let used = request.encode_into(&mut bytes).map_err(why)?;
    let id = request.id;
    let control = memory(account, false, &bytes[..used])?;
    let reply = memory(account, false, &[])?;
    let (used, count) = client
        .execute(
            loan(&control, false)?,
            used as u32,
            payload.map(|m| loan(m, writable)).transpose()?,
            0,
            loan(&reply, true)?,
            wire::MAX_HEADER as u32,
        )
        .map_err(why)?;
    let response = Response::decode(&read(&reply, used)?).map_err(why)?;
    if response.id != id
        || count
            != match &response.outcome {
                Outcome::Read { length, .. } => *length,
                _ => 0,
            }
    {
        return Err("response correlation/count mismatch".into());
    }
    Ok(response)
}
fn proof(out: &Console) -> Result<(), String> {
    let account = Account(start::take("account").ok_or("no actual job account")?);
    let bindings = Memory(start::take("storage.binding").ok_or("no actual Admission asset")?);
    let prefix = bindings.map(0, 4096, map::READ).map_err(why)?;
    let mut header = [0; wire::PREFIX_BYTES];
    prefix.read(0, &mut header).map_err(why)?;
    drop(prefix);
    let used = wire::header_bytes(&header).map_err(why)?;
    let capacity = used.div_ceil(4096) * 4096;
    let mut bytes = vec![0; capacity];
    bindings
        .map(0, capacity as u64, map::READ)
        .and_then(|view| view.read(0, &mut bytes))
        .map_err(why)?;
    if bytes[used..].iter().any(|b| *b != 0) {
        return Err("nonzero admission padding".into());
    }
    let admitted = Admission::decode(&bytes[..used]).map_err(why)?;
    if admitted.role != SessionRole::SingleNodeHome {
        return Err("not actual raw home role".into());
    }
    let mut binder = admission::Client::connect(
        Channel(start::take("storage.admission").ok_or("no private binder")?),
        deadline_in(WAIT),
    )
    .map_err(why)?;
    binder.core().set_timeout(WAIT);
    let mut client = bind(&mut binder, &account, &admitted)?;
    let object = Object {
        workspace: admitted.scope.workspace.clone(),
        bucket: "jobs".into(),
        key: "native-io-proof".into(),
    };
    let request = |id, operation, object: Object| Request {
        id,
        scope: admitted.scope.clone(),
        object,
        proof: Proof::NativeSession,
        expected_role: Some(SessionRole::SingleNodeHome),
        operation,
    };
    let source = memory(&account, true, BYTES)?;
    let response = execute(
        &mut client,
        &account,
        request(
            1,
            Operation::Create {
                length: BYTES.len() as u64,
            },
            object.clone(),
        ),
        Some(&source),
        false,
    )?;
    let written = match response.outcome {
        Outcome::Written(metadata)
            if metadata.object == object && metadata.bytes == BYTES.len() as u64 =>
        {
            println!(
                out,
                "KE3 native storage: created: durable write acknowledged"
            );
            Some(metadata)
        }
        Outcome::Refused(Refusal::Conflict) => {
            println!(
                out,
                "KE3 native storage: existing: actual previously published key"
            );
            None
        }
        other => return Err(format!("create failed: {other:?}")),
    };
    let mut denied = object.clone();
    denied.key = "not-granted".into();
    if !matches!(
        execute(
            &mut client,
            &account,
            request(2, Operation::Current, denied),
            None,
            false
        )?
        .outcome,
        Outcome::Refused(Refusal::Denied)
    ) {
        return Err("foreign object was not denied".into());
    }
    drop(client);
    let mut reopened = bind(&mut binder, &account, &admitted)?;
    let metadata = match execute(
        &mut reopened,
        &account,
        request(3, Operation::Current, object.clone()),
        None,
        false,
    )?
    .outcome
    {
        Outcome::Head(metadata)
            if metadata.object == object && metadata.bytes == BYTES.len() as u64 =>
        {
            metadata
        }
        other => return Err(format!("reopened metadata: {other:?}")),
    };
    if written
        .as_ref()
        .is_some_and(|committed| committed != &metadata)
    {
        return Err("reopen changed acknowledged immutable version".into());
    }
    let destination = memory(&account, true, &[])?;
    let response = execute(
        &mut reopened,
        &account,
        request(
            4,
            Operation::Read {
                version: metadata.version.clone(),
                offset: 0,
                length: BYTES.len() as u64,
            },
            object,
        ),
        Some(&destination),
        true,
    )?;
    if !matches!(response.outcome,Outcome::Read{metadata: actual,offset:0,length} if actual==metadata && length==BYTES.len() as u64)
        || read(&destination, BYTES.len() as u32)? != BYTES
    {
        return Err("reopened durable bytes mismatch".into());
    }
    // Reuse the sole canonical response codec, with one constant receipt id
    // across boots. No private metadata format or substitute digest is defined.
    let mut receipt = vec![0; wire::MAX_HEADER];
    let used = Response {
        id: 0x4b4533,
        outcome: Outcome::Head(metadata),
    }
    .encode_into(&mut receipt)
    .map_err(why)?;
    if used > 2048 {
        return Err("fixture metadata receipt exceeds serial bound".into());
    }
    let mut hex = String::with_capacity(used * 2);
    for byte in &receipt[..used] {
        use core::fmt::Write;
        core::write!(&mut hex, "{byte:02x}").map_err(why)?;
    }
    println!(out, "KE3 native storage receipt: {}", hex);
    println!(
        out,
        "KE3 native storage: ok: exact Account session, durable bytes, denial and reopened pinned read verified"
    );
    Ok(())
}
fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    match proof(&out) {
        Ok(()) => 0,
        Err(error) => {
            println!(out, "KE3 native storage: FAIL: {}", error);
            1
        }
    }
}
