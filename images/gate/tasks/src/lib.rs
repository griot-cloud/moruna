//! What every gate task does, in ring mode (stream P, `moruna_sys::plane::Rings`): map its
//! region and its rings object once, then answer each batch the engine posts with the job's
//! output, written into windows as records (`moruna_gate_record::parts`: one record when the
//! answer fits a window, several in order when it does not), and say its exit code on its rings
//! as it ends. It sleeps on its bell only when it has nothing to do.
//!
//! A batch the function refuses ends the task with a non-zero code and the reason in its log,
//! as a Python task that raised ended its stage on 0.6.6.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use core::fmt::Write;

use moruna_gate_record::parts::{Carry, Wrote, apply_part};
use moruna_gate_record::{Error, Job};
use moruna_sys::plane::{Log, Part, Plane, Rings, Step};

/// The task's exit codes.
pub mod code {
    pub const DONE: i64 = 0;
    pub const NO_PLANE: i64 = 2;
    pub const NO_RINGS: i64 = 3;
    pub const RINGS: i64 = 5;
    pub const REFUSED: i64 = 7;
    pub const WINDOW_SMALL: i64 = 8;
}

fn say(log: &mut Option<Log>, job: Job, what: &dyn core::fmt::Display) {
    if let Some(log) = log.as_mut() {
        let _ = writeln!(log, "gate-{}: {what}", job.name());
    }
}

/// Runs `job` over every batch of the task's rings; the exit code, said on the rings.
pub fn run(job: Job) -> i64 {
    let Some(plane) = Plane::take() else {
        return code::NO_PLANE;
    };
    let mut log = Log::take();
    let mut rings = match Rings::attach(&plane) {
        Ok(r) => r,
        Err(e) => {
            say(&mut log, job, &alloc::format!("no rings: {e:?}"));
            return code::NO_RINGS;
        }
    };
    let mut carry = Carry::new();
    let mut batches = 0u64;
    loop {
        let mut failure: Option<Error> = None;
        let step = rings.step_parts(|input, window, part| {
            match apply_part(job, input, window, part, &mut carry) {
                Ok(Wrote::Last(n)) => Some(Part::Last(n)),
                Ok(Wrote::More(n)) => Some(Part::More(n)),
                Err(e) => {
                    failure = Some(e);
                    None
                }
            }
        });
        match step {
            Ok(Step::Answered(_)) => batches += 1,
            Ok(Step::NoInput) => rings.wait_input(),
            Ok(Step::NoWindow) => rings.wait_window(),
            Ok(Step::Ended) => break,
            Err(e) => {
                let code = match &failure {
                    Some(Error::WindowTooSmall { .. }) => code::WINDOW_SMALL,
                    Some(_) => code::REFUSED,
                    None => code::RINGS,
                };
                match failure {
                    Some(f) => say(&mut log, job, &f),
                    None => say(&mut log, job, &alloc::format!("the rings: {e:?}")),
                }
                drop(log);
                rings.note();
                return rings.exit(code);
            }
        }
    }
    say(&mut log, job, &alloc::format!("answered {batches} batches"));
    drop(log);
    rings.note();
    rings.exit(code::DONE)
}
