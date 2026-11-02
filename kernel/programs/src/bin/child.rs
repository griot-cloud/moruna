//! KK-T11: the process the supervisor starts from an image file.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::console;
use moruna_sys::{println, start};

moruna_sys::main!(run);

fn run() -> i64 {
    let out = console();
    println!(
        out,
        "KB1 spawn: ok: {} runs, started by the supervisor with {} handle",
        start::process_name(),
        start::count()
    );
    0
}
