//! What every gate task does: take the plane, then for each batch the engine maps into input
//! slot 0, write the job's output record into a window of output slot 1 and put it. A batch the
//! function refuses ends the task with the reason on its console and a non-zero code, as a
//! Python task that raised ended its stage on 0.6.6.
//!
//! This is the plane's API as it is on `feature/arrow-core-0.7` (`batch_get`, `batch_put`). On
//! stream P's rings the loop is `Rings::step(|input, window| apply(job, input, window).ok())`:
//! [`moruna_gate_record::apply`] already has that shape.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use core::fmt::Write;

use moruna_gate_record::{Error, Job, apply};
use moruna_sys::plane::Plane;

/// The task's exit codes.
pub mod code {
    pub const DONE: i64 = 0;
    pub const NO_PLANE: i64 = 2;
    pub const NO_INPUT: i64 = 3;
    pub const NO_OUTPUT: i64 = 4;
    pub const INPUT: i64 = 5;
    pub const WINDOW: i64 = 6;
    pub const REFUSED: i64 = 7;
    pub const WINDOW_SMALL: i64 = 8;
    pub const PUT: i64 = 9;
}

fn say(job: Job, what: &dyn core::fmt::Display) {
    if let Some(console) = moruna_sys::start::peek("console") {
        let mut line = alloc::string::String::new();
        let _ = writeln!(line, "gate-{}: {what}", job.name());
        let console = moruna_sys::Console(moruna_sys::Handle::from_raw(console));
        let _ = console.write_str(&line);
        core::mem::forget(console);
    }
}

/// Runs `job` over every batch of the task's plane; the exit code.
pub fn run(job: Job) -> i64 {
    let Some(plane) = Plane::take() else {
        return code::NO_PLANE;
    };
    let Some(mut input) = plane.input(0) else {
        return code::NO_INPUT;
    };
    let Some(mut output) = plane.output(1) else {
        return code::NO_OUTPUT;
    };
    loop {
        let batch = match input.get() {
            Ok(Some(b)) => b,
            Ok(None) => return code::DONE,
            Err(e) => {
                say(job, &alloc::format!("batch_get: {e:?}"));
                return code::INPUT;
            }
        };
        let mut window = match output.window() {
            Ok(Some(w)) => w,
            Ok(None) | Err(_) => {
                say(job, &"no window for the output");
                return code::WINDOW;
            }
        };
        let len = match apply(job, batch.bytes(), window.bytes()) {
            Ok(len) => len,
            Err(e @ Error::WindowTooSmall { .. }) => {
                say(job, &e);
                return code::WINDOW_SMALL;
            }
            Err(e) => {
                say(job, &e);
                return code::REFUSED;
            }
        };
        if let Err((e, _)) = window.put(0, len as u64) {
            say(job, &alloc::format!("batch_put: {e:?}"));
            return code::PUT;
        }
    }
}
