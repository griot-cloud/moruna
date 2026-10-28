//! DR-T14 (SDD 25, k): holding exactly the resources the device manager gives the
//! driver of `block0`, less its DMA right (control's `sandbox`), this process tries
//! what a driver must not do and is refused each: another device's configuration
//! window and BAR (`console0`'s), the configuration windows either side of its own, a
//! window wider than its own, an MSI slot past its own, and DMA through any resource
//! it holds.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use devices_protocol::control;
use moruna_svc::{
    Channel, Console, DeviceMemory, Error, Handle, Interrupt, Memory, Pin, deadline_in, println,
    start,
};

moruna_svc::main!(run);

const SECONDS: u64 = 1_000_000_000;

fn denied<T>(what: &str, result: Result<T, Error>) -> Result<(), String> {
    match result {
        Err(Error::AccessDenied) => Ok(()),
        Err(e) => Err(format!("{what}: {}, not access denied", e.name())),
        Ok(_) => Err(format!("{what} was allowed")),
    }
}

fn probe(ctl: &mut control::Client) -> Result<usize, String> {
    let sandbox: Vec<Handle> = ctl
        .sandbox(String::from("block0"))
        .map_err(|e| format!("sandbox: {e:?}"))?;
    let (own_config, own_bars) = ctl
        .windows(String::from("block0"))
        .map_err(|e| format!("windows: {e:?}"))?;
    let (other_config, other_bars) = ctl
        .windows(String::from("console0"))
        .map_err(|e| format!("windows: {e:?}"))?;
    let [config, bars @ .., msi] = sandbox.as_slice() else {
        return Err(String::from(
            "the sandbox is not a config window, BARs and MSI",
        ));
    };
    if bars.len() != own_bars.len() || bars.is_empty() {
        return Err(format!(
            "{} BAR resources for {} BARs",
            bars.len(),
            own_bars.len()
        ));
    }
    let mut refused = 0;
    let mut try_one = |what: &str, r: Result<(), Error>| -> Result<(), String> {
        denied(what, r)?;
        refused += 1;
        Ok(())
    };
    try_one(
        "console0's configuration window",
        DeviceMemory::create(config, other_config, 4096).map(drop),
    )?;
    try_one(
        "the configuration window after its own",
        DeviceMemory::create(config, own_config + 4096, 4096).map(drop),
    )?;
    try_one(
        "the configuration window before its own",
        DeviceMemory::create(config, own_config - 4096, 4096).map(drop),
    )?;
    try_one(
        "a configuration window wider than its own",
        config.narrow(own_config, 8192).map(drop),
    )?;
    for bar in bars {
        for (_, base, _) in &other_bars {
            try_one(
                "a BAR of console0",
                DeviceMemory::create(bar, base & !0xfff, 4096).map(drop),
            )?;
        }
    }
    let slots = msi
        .resource_info()
        .map_err(|e| format!("resource_info: {}", e.name()))?;
    try_one(
        "an MSI slot past its own",
        Interrupt::create_msi(msi, slots.base + slots.len).map(drop),
    )?;
    let memory = Memory::create_dma(4096).map_err(|e| format!("DMA memory: {}", e.name()))?;
    let mut page = [0u64; 1];
    for (what, resource) in [
        ("the MSI resource", msi),
        ("the configuration window", config),
    ]
    .into_iter()
    .chain(bars.iter().map(|b| ("a BAR window", b)))
    {
        try_one(
            &format!("DMA through {what}"),
            Pin::create(resource, &memory.0, true, &mut page).map(drop),
        )?;
    }
    Ok(refused)
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(c) = start::take("control") else {
        println!(out, "KB3 driver isolation: FAIL: no control");
        return 1;
    };
    let mut ctl = match control::Client::connect(Channel(c), deadline_in(300 * SECONDS)) {
        Ok(c) => c,
        Err(e) => {
            println!(out, "KB3 driver isolation: FAIL: control: {:?}", e);
            return 1;
        }
    };
    ctl.core().set_timeout(300 * SECONDS);
    match probe(&mut ctl) {
        Ok(n) => {
            println!(
                out,
                "KB3 driver isolation: ok: holding block0's driver resources it was refused {} times: console0's configuration window and BARs, the windows either side of its own, a wider one, an MSI slot past its own, and DMA through every resource it holds, none of which is a DMA right",
                n
            );
            0
        }
        Err(e) => {
            println!(out, "KB3 driver isolation: FAIL: {}", e);
            1
        }
    }
}
