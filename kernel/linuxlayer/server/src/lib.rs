// SPDX-License-Identifier: MPL-2.0

//! The drop-in `linux` server's decisions (`main.rs` has the core's calls): what a program's
//! process is called, which requests are refused, and the `Ended` a task's end makes from
//! the personality's report and its output.
//!
//! The server speaks linux-protocol, as `kernel/linux/server` did, so its clients
//! (linux-test, the plane test, the engine, `moruna-micro`) are unchanged; each program now
//! runs on a Linux personality of its own (`linuxlayer_launch`).

#![no_std]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use linux_protocol::{Ended, OUTPUT_MAX, REQUEST_MAX, how, linux, log};
use linuxlayer_launch::report::{self, Report};
use moruna_record::{record, ring::Ring};

/// A program's process name: `lx-` and its file name, lower case, `[a-z0-9._-]` (as
/// `kernel/linux/server` named them, so the boot's lines about them hold).
pub fn process_name(path: &[u8]) -> String {
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let mut name: String = base
        .iter()
        .take(29)
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => *b as char,
            b'A'..=b'Z' => b.to_ascii_lowercase() as char,
            _ => '-',
        })
        .collect();
    if name.is_empty() {
        name.push_str("program");
    }
    format!("lx-{name}")
}

/// Whether a request's path, arguments and environment may run: an absolute path, no zero
/// byte, at most `REQUEST_MAX` bytes in all.
pub fn request_ok(path: &[u8], argv: &[Vec<u8>], env: &[Vec<u8>]) -> bool {
    let size = path.len()
        + argv.iter().map(|a| a.len() + 1).sum::<usize>()
        + env.iter().map(|e| e.len() + 1).sum::<usize>();
    size <= REQUEST_MAX
        && path.first() == Some(&b'/')
        && !path.contains(&0)
        && argv.iter().chain(env).all(|b| !b.contains(&0))
}

/// A program's standard output and error, as `Ended::output` holds them: the first
/// `OUTPUT_MAX` bytes, and a count of the rest.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub dropped: u64,
}

impl Output {
    /// Adds a line, which ends with a newline.
    pub fn line(&mut self, line: &[u8]) {
        for &b in line.iter().chain(b"\n") {
            if self.bytes.len() < OUTPUT_MAX {
                self.bytes.push(b);
            } else {
                self.dropped += 1;
            }
        }
    }
}

/// The lines in a log ring the personality wrote to: each record's stream (`stdout` or
/// `stderr`) and body, in the order written.
pub fn ring_lines(region: &mut [u8]) -> Vec<(String, Vec<u8>)> {
    let Ok(ring) = Ring::attach(region) else {
        return Vec::new();
    };
    let Ok(mut consumer) = ring.consumer() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    while let Some(bytes) = consumer.pop() {
        if let record::Decoded::Record(r, _) = record::decode(bytes) {
            let stream = r
                .headers
                .iter()
                .find(|(k, _)| *k == log::STREAM)
                .map_or(log::STDOUT, |(_, v)| v);
            lines.push((String::from(stream), r.body.to_vec()));
        }
    }
    lines
}

/// What a request whose program could not start answers: the protocol's error for the
/// Linux error the personality reported.
pub fn not_started(errno: i64) -> linux::Error {
    const ENOENT: i64 = 2;
    const ENOEXEC: i64 = 8;
    const EACCES: i64 = 13;
    const ENOTDIR: i64 = 20;
    match errno {
        ENOENT | ENOTDIR => linux::Error::NotFound,
        ENOEXEC | EACCES => linux::Error::NotExecutable,
        _ => linux::Error::NoProcess,
    }
}

/// The `Ended` of a task: the personality's report (none when its process was killed or
/// faulted before it wrote one) and the output gathered.
pub fn ended(report: Option<Report>, output: Output) -> Result<Ended, linux::Error> {
    let Some(report) = report else {
        return Ok(Ended {
            how: how::FAULTED,
            code: -1,
            output: linux_protocol::Bytes(output.bytes),
            dropped: output.dropped,
            refused: Vec::new(),
            served: 0,
        });
    };
    let how = match report.how {
        report::EXITED => how::EXITED,
        report::SIGNALED => how::SIGNALED,
        report::NOT_STARTED => return Err(not_started(report.code)),
        _ => how::FAULTED,
    };
    Ok(Ended {
        how,
        code: report.code,
        output: linux_protocol::Bytes(output.bytes),
        dropped: output.dropped,
        refused: report.refused,
        served: report.served,
    })
}

/// The console line for a program's output line, as `kernel/linux/server` echoed them.
pub fn echo(name: &str, line: &[u8]) -> String {
    format!("linux {name}: {}", String::from_utf8_lossy(line))
}

#[cfg(test)]
mod test;
