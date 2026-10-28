//! DR-T13 (SDD 25, k): a client of `vsock0`, found through the directory. It runs the
//! vsock protocol's conformance suite, connects to the host's port 5000 (where
//! `kernel/tools/vsock-peer.py` listens) and checks the answer, then listens on port
//! 5001, accepts the host's connection and answers it.
//!
//! A machine with no virtio-vsock (a runner with no `/dev/vhost-vsock` cannot give
//! QEMU one) has no `vsock0`; the test then says so in a line of its own, which the
//! boot check accepts only when the boot attached no vsock.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use devices_protocol::directory;
use moruna_svc::{CallError, Channel, Console, conformance, deadline_in, println, start};
use vsock_protocol::{HOST_CID, vsock};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;
const HOST_PORT: u32 = 5000;
const GUEST_PORT: u32 = 5001;

struct Lookup<'a>(&'a mut directory::Client);

impl conformance::Connector for Lookup<'_> {
    fn connect(&mut self) -> Result<Channel, conformance::Fail> {
        self.0
            .lookup(String::from("vsock0"))
            .map_err(|e| format!("looking up vsock0: {e:?}"))
    }
}

/// Reads from `stream` until a newline.
fn line(v: &mut vsock::Client, stream: u32) -> Result<Vec<u8>, String> {
    let mut got = Vec::new();
    while !got.ends_with(b"\n") {
        let bytes = v.recv(stream, 4096).map_err(|e| format!("recv: {e:?}"))?;
        if bytes.is_empty() {
            return Err(format!("the host closed after {} bytes", got.len()));
        }
        got.extend_from_slice(&bytes);
    }
    Ok(got)
}

fn exchange(v: &mut vsock::Client) -> Result<u64, String> {
    let cid = v.local_cid().map_err(|e| format!("local_cid: {e:?}"))?;
    let s = v
        .connect_to(HOST_CID, HOST_PORT)
        .map_err(|e| format!("connecting to the host's port {HOST_PORT}: {e:?}"))?;
    v.send(s, b"ping from the guest\n".to_vec())
        .map_err(|e| format!("send: {e:?}"))?;
    let answer = line(v, s)?;
    if answer != b"pong: ping from the guest\n" {
        return Err(format!(
            "the host answered {:?}",
            String::from_utf8_lossy(&answer)
        ));
    }
    v.close(s).map_err(|e| format!("close: {e:?}"))?;
    v.listen(GUEST_PORT).map_err(|e| format!("listen: {e:?}"))?;
    let (s, peer, _) = v.accept(GUEST_PORT).map_err(|e| format!("accept: {e:?}"))?;
    if peer != HOST_CID {
        return Err(format!("accepted cid {peer}, not the host"));
    }
    let hello = line(v, s)?;
    if hello != b"hello from the host\n" {
        return Err(format!(
            "the host sent {:?}",
            String::from_utf8_lossy(&hello)
        ));
    }
    v.send(s, b"hello back from the guest\n".to_vec())
        .map_err(|e| format!("send: {e:?}"))?;
    v.close(s).map_err(|e| format!("close: {e:?}"))?;
    Ok(cid)
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(d) = start::take("directory") else {
        println!(out, "KB3 vsock: FAIL: no directory");
        return 1;
    };
    let mut dir = match directory::Client::connect(Channel(d), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 vsock: FAIL: the directory: {:?}", e);
            return 1;
        }
    };
    dir.core().set_timeout(300 * SECONDS);
    let channel = match dir.lookup(String::from("vsock0")) {
        Ok(c) => c,
        Err(CallError::Remote(directory::Error::NotFound)) => {
            println!(
                out,
                "KB3 vsock: no device: this machine has no virtio-vsock, so there is no vsock0"
            );
            return 0;
        }
        Err(e) => {
            println!(out, "KB3 vsock: FAIL: looking up vsock0: {:?}", e);
            return 1;
        }
    };
    let mut failed = 0;
    let suite = vsock_protocol::suite();
    let summary = conformance::run(&suite, &mut Lookup(&mut dir), &mut |check| {
        if let Err(why) = check.result {
            println!(out, "KB3 conformance: FAIL: {}: {}", check.name, why);
        }
    });
    if summary.ok() {
        println!(
            out,
            "KB3 conformance {}: ok: {} checks passed against vsock0",
            suite.id.name,
            summary.passed
        );
    } else {
        failed += 1;
    }
    let mut v = match vsock::Client::connect(channel, deadline_in(60 * SECONDS)) {
        Ok(v) => v,
        Err(e) => {
            println!(out, "KB3 vsock: FAIL: hello to vsock0: {:?}", e);
            return 1;
        }
    };
    v.core().set_timeout(300 * SECONDS);
    match exchange(&mut v) {
        Ok(cid) => println!(
            out,
            "KB3 vsock: ok: as cid {} connected to the host's port {} and was answered, then accepted the host on port {} and answered it",
            cid,
            HOST_PORT,
            GUEST_PORT
        ),
        Err(e) => {
            println!(out, "KB3 vsock: FAIL: {}", e);
            failed += 1;
        }
    }
    failed
}
