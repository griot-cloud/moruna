//! stdout and stderr (std's `sys::stdio`), as lines:
//!
//! - into the task's log, [`crate::plane::Log`], the ring the engine gives a native task as its
//!   start slot `log`: a record each, naming the stream `stderr`, as a Linux task's
//!   descriptor 2 does;
//! - with no log, onto the console, the start slot `console`, which the boot manifest or the
//!   supervisor gives a process: each line in one write, so lines of different processes do
//!   not interleave;
//! - with neither, nowhere: the output is dropped, as std's unsupported platforms drop it.
//!
//! Both streams write into the one place. stdin is empty, in std's file.

use moruna_abi::{RawHandle, limits};

use super::lock::Lock;
use super::os;
use crate::plane::Log;

/// The longest console line, its newline included: one `debug_write`.
const LINE: usize = limits::DEBUG_WRITE_MAX;

/// What has been written since the last newline, written whole at the next. A line longer than
/// [`LINE`] is cut there, as the log cuts its own.
struct Lines {
    pending: [u8; LINE],
    len: usize,
}

impl Lines {
    const fn new() -> Lines {
        Lines {
            pending: [0; LINE],
            len: 0,
        }
    }

    /// Takes `bytes`, giving `out` each line they end, with its newline.
    fn write(&mut self, bytes: &[u8], out: &mut impl FnMut(&[u8])) {
        for &b in bytes {
            if b == b'\n' {
                self.end(out);
            } else if self.len < LINE - 1 {
                self.pending[self.len] = b;
                self.len += 1;
            }
        }
    }

    /// Gives `out` the line the writes left without a newline, if any, with one.
    fn finish(&mut self, out: &mut impl FnMut(&[u8])) {
        if self.len > 0 {
            self.end(out);
        }
    }

    fn end(&mut self, out: &mut impl FnMut(&[u8])) {
        self.pending[self.len] = b'\n';
        out(&self.pending[..=self.len]);
        self.len = 0;
    }
}

enum Sink {
    /// Not looked for yet: the slots are looked at by the first write.
    Unopened,
    Log(Log),
    Console(RawHandle, Lines),
    /// Neither a log nor a console.
    None,
}

static SINK: Lock<Sink> = Lock::new(Sink::Unopened);

impl Sink {
    /// The task's log if it has one, else its console (looked at, not taken: it stays the
    /// program's own too), else nothing.
    fn open(&mut self) {
        if let Sink::Unopened = self {
            *self = match Log::take() {
                Some(log) => Sink::Log(log),
                None => match crate::start::peek("console") {
                    Some(console) => Sink::Console(console, Lines::new()),
                    None => Sink::None,
                },
            };
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.open();
        match self {
            Sink::Log(log) => log.write_bytes(bytes),
            Sink::Console(console, lines) => {
                let console = *console;
                lines.write(bytes, &mut |line| os::console_write(console, line));
            }
            Sink::Unopened | Sink::None => {}
        }
    }

    fn finish(&mut self) {
        match self {
            Sink::Log(log) => log.finish(),
            Sink::Console(console, lines) => {
                let console = *console;
                lines.finish(&mut |line| os::console_write(console, line));
            }
            Sink::Unopened | Sink::None => {}
        }
    }
}

/// Takes the task's log slot, or finds its console, now: the program's start does so before
/// `main` (system design 5B), so the C library's descriptors 1 and 2, which write through here,
/// find std's writer. Writing does so too, if nothing has.
pub fn open() {
    SINK.lock().open();
}

/// Writes `bytes`; every byte is taken, whether or not the line is kept.
pub fn write(bytes: &[u8]) -> usize {
    SINK.lock().write(bytes);
    bytes.len()
}

/// Writes the line the streams left without a newline, and, into a log, how many lines it
/// dropped.
pub fn finish() {
    SINK.lock().finish();
}

/// [`finish`], unless a thread holds the streams now: an abort does not wait on one.
pub fn finish_unless_busy() {
    if let Some(mut sink) = SINK.try_lock() {
        sink.finish();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    fn lines_of(writes: &[&[u8]], finish: bool) -> Vec<Vec<u8>> {
        let mut lines = Lines::new();
        let mut out = Vec::new();
        let mut take = |line: &[u8]| out.push(line.to_vec());
        for w in writes {
            lines.write(w, &mut take);
        }
        if finish {
            lines.finish(&mut take);
        }
        out
    }

    #[test]
    fn a_line_is_written_whole_at_its_newline() {
        assert_eq!(
            lines_of(&[b"std-arrow: sum ", b"is 55\nnext", b" line\n"], false),
            [b"std-arrow: sum is 55\n".to_vec(), b"next line\n".to_vec()]
        );
        assert_eq!(
            lines_of(&[b"a\n\nb\n"], false),
            [b"a\n".to_vec(), b"\n".to_vec(), b"b\n".to_vec()]
        );
    }

    #[test]
    fn the_last_line_without_a_newline_is_written_at_the_end() {
        assert_eq!(lines_of(&[b"done"], false), Vec::<Vec<u8>>::new());
        assert_eq!(lines_of(&[b"done"], true), [b"done\n".to_vec()]);
        assert_eq!(
            lines_of(&[b"done\n"], true),
            [b"done\n".to_vec()],
            "nothing left"
        );
    }

    #[test]
    fn a_long_line_is_cut_to_one_console_write() {
        let long = std::vec![b'x'; LINE + 50];
        let out = lines_of(&[&long, b"\nshort\n"], false);
        assert_eq!(out[0].len(), LINE);
        assert_eq!(out[0][LINE - 1], b'\n');
        assert_eq!(out[1], b"short\n");
    }

    #[test]
    fn a_task_with_neither_log_nor_console_takes_and_drops_its_output() {
        // A host process has no start page: no log slot, no console slot.
        open();
        let mut sink = Sink::Unopened;
        sink.write(b"nowhere\n");
        assert!(matches!(sink, Sink::None));
        sink.finish();
        assert_eq!(write(b"dropped\n"), 8);
        finish();
        finish_unless_busy();
    }
}
