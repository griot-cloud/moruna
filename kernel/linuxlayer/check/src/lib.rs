// SPDX-License-Identifier: MPL-2.0

//! B2's booted check of a task's filesystems and descriptors (stream B2-fs's checks),
//! through a real task start: `main.rs` asks the `linux` server to run
//! `boot/programs/taskcheck.c` with `run_granted`, an account of [`LIMIT`] bytes and a
//! scratch account of [`SCRATCH`] bytes, and [`verdict`] judges what it answered.

#![no_std]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use linux_protocol::{Ended, how};

/// The program, in the boot's Linux image.
pub const PROGRAM: &str = "/kc1/bin/taskcheck";

/// The task's account.
pub const LIMIT: u64 = 64 << 20;

/// The task's scratch account: `/tmp`'s quota.
pub const SCRATCH: u64 = 1 << 20;

/// The checks the program reports, each on a line of its own.
pub const CHECKS: [&str; 4] = ["image", "proc", "tmp", "epoll"];

/// The line the program writes on descriptor 2.
pub const STDERR_LINE: &str = "taskcheck: descriptor 2 reached the server";

/// The program's arguments: its path, the account's limit in KiB, the scratch's bytes.
pub fn argv() -> [String; 3] {
    [
        String::from(PROGRAM),
        format!("{}", LIMIT >> 10),
        format!("{SCRATCH}"),
    ]
}

/// Whether the task passed: the line to print after `B2 task: ok: `, or why not.
pub fn verdict(ended: &Ended) -> Result<String, String> {
    let text = String::from_utf8_lossy(&ended.output.0);
    if ended.how != how::EXITED || ended.code != 0 {
        return Err(format!(
            "taskcheck ended {} with code {}, printing `{}`",
            ended.how,
            ended.code,
            text.trim_end()
        ));
    }
    let mut details = Vec::new();
    for check in CHECKS {
        let head = format!("taskcheck: {check}: ok: ");
        let Some(line) = text.lines().find(|l| l.starts_with(&head)) else {
            return Err(format!("no `{check}` line in `{}`", text.trim_end()));
        };
        details.push(format!("{check}: {}", &line[head.len()..]));
    }
    if !text.lines().any(|l| l == STDERR_LINE) {
        return Err(format!(
            "its descriptor 2 line did not arrive: `{}`",
            text.trim_end()
        ));
    }
    details.push(String::from(
        "descriptor 2 reached the server's log ring as a record",
    ));
    Ok(details.join("; "))
}

#[cfg(test)]
mod test;
