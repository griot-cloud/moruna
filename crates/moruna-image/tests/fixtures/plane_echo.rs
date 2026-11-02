//! The program the image kind's tests run as a command that makes the data plane's calls itself
//! (SDD 19 with `plane`, SDD 29 d.1 on Linux): each batch of input slot 0 is put back unchanged
//! on output slot 1, the record copied from the batch's pages into the window's, and at the
//! input's end it exits with status 0.
//!
//! One optional argument makes it misbehave, each in the way a test needs:
//!
//! - `--garbage` puts a window of bytes that are no record, which the engine's check refuses;
//! - `--twice` puts every batch twice, more records than it read.

use std::process::exit;

fn die(what: &str, e: impl std::fmt::Debug) -> ! {
    eprintln!("plane_echo: {what}: {e:?}");
    exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    use moruna_plane_linux::task::Plane;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let plane = match Plane::take() {
        Ok(Some(plane)) => plane,
        Ok(None) => die("the plane", "taken twice"),
        Err(e) => die("the plane on descriptor 3", e),
    };
    let mut input = plane.input(0).unwrap_or_else(|| die("input 0", "taken"));
    let mut output = plane.output(1).unwrap_or_else(|| die("output 1", "taken"));
    let times = if flag("--twice") { 2 } else { 1 };
    loop {
        let batch = match input.get() {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => die("batch_get", e),
        };
        let record = batch.bytes();
        for _ in 0..times {
            let mut window = match output.window() {
                Ok(Some(w)) => w,
                Ok(None) => die("the window", "none"),
                Err(e) => die("batch_get on the output", e),
            };
            let len = if flag("--garbage") {
                window.bytes()[..4096].fill(0x5a);
                4096
            } else {
                window.bytes()[..record.len()].copy_from_slice(record);
                record.len() as u64
            };
            if let Err((e, _)) = window.put(0, len) {
                die("batch_put", e);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    die("the data plane", "a plane program runs on Linux");
}
