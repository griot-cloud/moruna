//! Actual resident private housekeeping client. Timer waits perform useful scheduled
//! maintenance; no sleep/keepalive or tenant-facing endpoint impersonates this process.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
#[cfg(not(target_os = "none"))]
compile_error!("storage housekeeping requires the actual native kernel target");
extern crate alloc;
use alloc::{format, string::String};
use moruna_storage_service::{maintenance_config, protocol::maintenance};
use moruna_svc::{
    CallError, Channel, Console, DEADLINE_INFINITE, ObjectType, Rights, Signals, WaitItem, now,
    println, start, wait,
};
moruna_svc::main!(run);
fn serve(out: &Console) -> Result<(), String> {
    let account = start::take("account").ok_or("actual housekeeping Account missing")?;
    let (kind, rights) = account
        .info()
        .map_err(|error| format!("housekeeping Account info: {}", error.name()))?;
    if kind != ObjectType::Account || !rights.contains(Rights::READ | Rights::WRITE) {
        return Err("actual carved housekeeping Account required".into());
    }
    let policy = maintenance_config::load(&account)
        .map_err(|cause| format!("maintenance policy refused: {cause:?}"))?
        .ok_or("explicit maintenance policy required")?;
    let channel = Channel(
        start::take("storage.maintenance").ok_or("private maintenance capability required")?,
    );
    let deadline = now()
        .checked_add(policy.interval_ns)
        .ok_or("maintenance deadline overflow")?;
    let mut client = maintenance::Client::connect(channel, deadline)
        .map_err(|cause| format!("maintenance handshake: {cause:?}"))?;
    client.core().set_timeout(policy.interval_ns);
    let peer = client
        .core()
        .channel()
        .0
        .duplicate(Rights::WAIT)
        .map_err(|cause| format!("maintenance wait capability: {}", cause.name()))?;
    let timer = moruna_sys::Timer::create()
        .map_err(|cause| format!("housekeeping Timer: {}", cause.name()))?;
    loop {
        let deadline = now()
            .checked_add(policy.interval_ns)
            .ok_or("maintenance interval overflow")?;
        timer
            .set(deadline)
            .map_err(|cause| format!("housekeeping Timer arm: {}", cause.name()))?;
        let mut items = [
            WaitItem::new(&timer.0, Signals::SIGNALED),
            WaitItem::new(&peer, Signals::PEER_CLOSED),
        ];
        wait(&mut items, DEADLINE_INFINITE)
            .map_err(|cause| format!("housekeeping wait: {}", cause.name()))?;
        if items[1].observed().contains(Signals::PEER_CLOSED) {
            return Err("private maintenance peer closed".into());
        }
        if !items[0].observed().contains(Signals::SIGNALED) {
            return Err("housekeeping woke without timer or peer signal".into());
        }
        match client.compact() {
            Ok(()) => println!(out, "storage maintenance completed"),
            Err(CallError::Remote(maintenance::Error::Busy)) => {
                println!(out, "storage maintenance deferred: owner busy")
            }
            Err(CallError::Remote(maintenance::Error::Capacity)) => {
                println!(out, "storage maintenance refused: explicit resource budget")
            }
            // A soft timeout is NOT a retry instruction: the server owns/drains any
            // accepted original IO. Stop this client instead of overlapping uncertainty.
            Err(cause) => return Err(format!("actual maintenance failed: {cause:?}")),
        }
    }
}
fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    match serve(&out) {
        Ok(()) => 0,
        Err(cause) => {
            println!(out, "storage housekeeping refused: {}", cause);
            1
        }
    }
}
