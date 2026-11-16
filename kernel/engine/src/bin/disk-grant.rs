//! A data disk's hand-over (`architecture/data-os.md` §3, decision 2): the program the device
//! manager starts as the "driver" of each data disk, so the engine can drive the disk itself.
//! It holds the function's handles the device manager gave it (`config`, each `barN`, `msi`,
//! `dma`) and gives them, all at once and once only, to the first connection to its `listen`
//! slot: the one the engine's supervisor makes with `grant disk.in.NAME lookup SERVICEn` and
//! grants the engine. The message's bytes name the handles in order (`engine::disk_names`).
//! Every later connection is closed unanswered; the program ends when its listener does.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};

use moruna_svc::{
    Channel, Console, DEADLINE_INFINITE, Handle, Signals, frame, println, start, wait_one,
};

moruna_svc::main!(run);

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    let Some(listen) = start::take("listen").map(Channel) else {
        println!(out, "disk-grant: FAIL: no `listen` slot");
        return 1;
    };
    let mut bars = Vec::new();
    let mut handles = Vec::new();
    let take = |name: &str| start::take(name);
    let (Some(config), Some(msi), Some(dma)) = (take("config"), take("msi"), take("dma")) else {
        println!(
            out,
            "disk-grant: FAIL: the function's handles are not all here"
        );
        return 1;
    };
    handles.push(config);
    handles.push(msi);
    handles.push(dma);
    for index in 0..6 {
        if let Some(bar) = start::take(&alloc::format!("bar{index}")) {
            bars.push(index);
            handles.push(bar);
        }
    }
    let names: String = engine::disk_names(&bars);
    let mut held: Option<Vec<Handle>> = Some(handles);
    loop {
        match wait_one(
            &listen.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            DEADLINE_INFINITE,
        ) {
            Ok(seen) if seen.contains(Signals::READABLE) => {}
            _ => return 0,
        }
        let mut bytes = [0u8; frame::HEADER];
        let mut got = [None];
        let Ok((_, n)) = listen.read(&mut bytes, &mut got) else {
            continue;
        };
        let Some(connection) = got[0].take().filter(|_| n == 1).map(Channel) else {
            continue;
        };
        if bytes[0] != frame::Kind::Connect as u8 {
            continue;
        }
        let Some(handles) = held.take() else {
            // The disk is the first client's: a later one is closed unanswered.
            continue;
        };
        let mut moving: Vec<Option<Handle>> = handles.into_iter().map(Some).collect();
        match connection.write_with(names.as_bytes(), &mut moving) {
            Ok(()) => println!(out, "disk-grant: handed over ({})", names),
            Err(e) => {
                println!(out, "disk-grant: FAIL: handing over: {}", e.name());
                return 1;
            }
        }
    }
}
