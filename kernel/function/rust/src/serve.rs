//! [`serve`]: a task's whole life on its rings.

use std::process::ExitCode;
use std::sync::Arc;

use arrow_array::RecordBatch;

use crate::code;
use crate::emit::Emit;
use crate::error::FunctionError;
use crate::lane::{Lane, Posted};
use crate::platform;
use crate::record::{self, Lease};

/// Runs `f` on every batch the engine posts, until it posts no more, and returns the task's exit
/// code for `main` to return (it is also said on the task's rings, where the engine reads it).
///
/// Each batch is read in place and handed to `f` with an [`Emit`] for its output; the batch is
/// released once `f` returns and nothing of it is held. The task ends at the first failure, with
/// the failure's line in its log and its code ([`crate::code`]):
///
/// - `f` returns an error, or [`Emit::batch`] failed: the error's message and code;
/// - `f` kept any part of its batch past its call: [`code::REFUSED`], naming the function;
/// - the input record fails the engine's record check: [`code::REFUSED`];
/// - the rings broke: [`code::RINGS`].
///
/// A batch that ended the task is never released. `serve` attaches to the task's plane once; a
/// second call in the same task finds none ([`code::NO_PLANE`]).
pub fn serve<F>(f: F) -> ExitCode
where
    F: FnMut(RecordBatch, &mut Emit<'_>) -> Result<(), FunctionError>,
{
    let code = match platform::attach() {
        Ok(mut lane) => run(&mut lane, f),
        Err(unattached) => {
            platform::log_line(&unattached.line);
            unattached.code
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
}

/// Says `what` in the task's log, after its name.
fn say(lane: &mut dyn Lane, what: &str) {
    let line = format!("{}: {what}", lane.name());
    lane.log(&line);
}

/// Ends the task with `e`: its line, and its code said on the rings.
fn end(lane: &mut dyn Lane, e: &FunctionError) -> i64 {
    say(lane, e.message());
    lane.exit(e.code())
}

/// The task's loop over its rings: the exit code it said.
pub(crate) fn run<F>(lane: &mut dyn Lane, mut f: F) -> i64
where
    F: FnMut(RecordBatch, &mut Emit<'_>) -> Result<(), FunctionError>,
{
    let mut batches = 0u64;
    loop {
        let posted = match lane.batch() {
            Ok(Some(p)) => p,
            Ok(None) if lane.ended() => break,
            Ok(None) => {
                lane.wait_input();
                continue;
            }
            Err(e) => return end(lane, &FunctionError::rings(e)),
        };
        if let Err(e) = answer(lane, posted, &mut f) {
            return end(lane, &e);
        }
        lane.release();
        batches += 1;
    }
    say(lane, &format!("answered {batches} batches"));
    lane.exit(code::DONE)
}

/// What a function that kept its batch is told.
pub(crate) const KEPT: &str = "the function kept its input batch (or a column or buffer of it) \
    after its call returned: a batch is read in place and is the function's only during its \
    call, so a function copies what it keeps";

/// Calls `f` on the batch `posted` holds; `Ok` when the batch can be released.
fn answer<F>(lane: &mut dyn Lane, posted: Posted, f: &mut F) -> Result<(), FunctionError>
where
    F: FnMut(RecordBatch, &mut Emit<'_>) -> Result<(), FunctionError>,
{
    let lease = Lease::new(posted.keep.clone());
    let batch = record::read(&posted, &lease)?;
    let mut out = Emit::new(lane, posted.cookie);
    let returned = f(batch, &mut out);
    let emitted = out.finish();
    returned?;
    emitted?;
    if Arc::strong_count(&lease) > 1 {
        return Err(FunctionError::new(KEPT));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane::Window;
    use arrow_array::{ArrayRef, Int64Array};
    use moruna_ipc_check::record::{Need, framing_need};
    use std::collections::VecDeque;
    use std::ptr::NonNull;

    /// Page-aligned bytes.
    struct Pages {
        words: Vec<u64>,
        start: usize,
        len: usize,
    }

    impl Pages {
        fn new(len: usize) -> Pages {
            let mut words = vec![0u64; len.div_ceil(8) + 512];
            let base = words.as_mut_ptr() as usize;
            let start = (base.div_ceil(4096) * 4096 - base) / 8;
            Pages { words, start, len }
        }

        fn ptr(&mut self) -> NonNull<u8> {
            NonNull::new(self.words[self.start..].as_mut_ptr() as *mut u8).unwrap()
        }

        fn bytes(&mut self) -> &mut [u8] {
            let len = self.len;
            unsafe { std::slice::from_raw_parts_mut(self.ptr().as_ptr(), len) }
        }
    }

    /// A lane over plain memory, with each of its failures on demand.
    #[derive(Default)]
    struct Fake {
        inputs: VecDeque<Pages>,
        held: Vec<Pages>,
        windows: Vec<Pages>,
        window_bytes: u64,
        free: VecDeque<u64>,
        posted: Vec<(u64, u64, u64)>,
        released: u64,
        taken: u64,
        said: Option<i64>,
        log: Vec<String>,
        waits: u32,
        bad_batch: bool,
        bad_window: bool,
        no_window: bool,
        full: bool,
    }

    impl Fake {
        fn new(inputs: &[RecordBatch], windows: u64, window_bytes: u64) -> Fake {
            let records: Vec<Vec<u8>> = inputs
                .iter()
                .map(|b| record::encode(b).unwrap().to_vec().unwrap())
                .collect();
            Fake::records(&records, windows, window_bytes)
        }

        fn records(records: &[Vec<u8>], windows: u64, window_bytes: u64) -> Fake {
            let inputs = records
                .iter()
                .map(|r| {
                    let mut p = Pages::new(r.len());
                    p.bytes().copy_from_slice(r);
                    p
                })
                .collect();
            Fake {
                inputs,
                windows: (0..windows)
                    .map(|_| Pages::new(window_bytes as usize))
                    .collect(),
                free: (0..windows).collect(),
                window_bytes,
                ..Fake::default()
            }
        }
    }

    impl Lane for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn batch(&mut self) -> Result<Option<Posted>, String> {
            if self.bad_batch {
                return Err(String::from("an input past the region"));
            }
            let Some(mut p) = self.inputs.pop_front() else {
                return Ok(None);
            };
            let posted = Posted {
                cookie: self.taken,
                addr: p.ptr(),
                len: p.len,
                keep: None,
            };
            self.taken += 1;
            self.held.push(p);
            Ok(Some(posted))
        }

        fn ended(&mut self) -> bool {
            // A first look finds nothing, as when the engine has not posted yet.
            self.waits > 0
        }

        fn wait_input(&mut self) {
            self.waits += 1;
        }

        fn window_bytes(&self) -> u64 {
            self.window_bytes
        }

        fn window(&mut self) -> Result<Option<Window>, String> {
            if self.bad_window {
                return Err(String::from("the engine gave a window that is not one"));
            }
            let Some(w) = self.free.pop_front() else {
                return Ok(None);
            };
            let len = self.window_bytes as usize;
            Ok(Some(Window {
                number: w,
                addr: self.windows[w as usize].ptr(),
                len,
            }))
        }

        fn wait_window(&mut self) -> bool {
            if self.no_window {
                return false;
            }
            // The engine takes every record and gives its window back.
            for (w, _, _) in &self.posted {
                if !self.free.contains(w) {
                    self.free.push_back(*w);
                }
            }
            true
        }

        fn post(&mut self, window: u64, len: u64, cookie: u64) -> bool {
            if self.full {
                return false;
            }
            self.posted.push((window, len, cookie));
            true
        }

        fn release(&mut self) {
            self.released += 1;
        }

        fn exit(&mut self, code: i64) -> i64 {
            self.said = Some(code);
            code
        }

        fn log(&mut self, line: &str) {
            self.log.push(line.to_string());
        }
    }

    fn ints(n: i64) -> RecordBatch {
        let a: ArrayRef = Arc::new(Int64Array::from_iter_values(0..n));
        RecordBatch::try_from_iter([("a", a)]).unwrap()
    }

    fn echo(b: RecordBatch, out: &mut Emit<'_>) -> Result<(), FunctionError> {
        out.batch(&b)
    }

    #[test]
    fn every_batch_is_answered_released_and_counted() {
        let mut lane = Fake::new(&[ints(3), ints(0), ints(5)], 2, 1 << 16);
        assert_eq!(run(&mut lane, echo), code::DONE);
        assert_eq!((lane.released, lane.said), (3, Some(0)));
        let cookies: Vec<u64> = lane.posted.iter().map(|p| p.2).collect();
        assert_eq!(cookies, [0, 1, 2]);
        assert_eq!(lane.log, ["fake: answered 3 batches"]);
        assert_eq!(lane.waits, 1, "it waited once, then saw the end");
    }

    #[test]
    fn a_batch_bigger_than_a_window_is_cut_into_records_in_order() {
        // Enough windows that none is reused, so every record can be read back after.
        let mut lane = Fake::new(&[ints(5000)], 32, 3 * 4096);
        assert_eq!(run(&mut lane, echo), code::DONE);
        assert!(lane.posted.len() > 2, "{:?}", lane.posted);
        assert!(lane.posted.iter().all(|p| p.1 <= 3 * 4096 && p.2 == 0));
        // The records, read back in order, are the rows in order.
        let mut next = 0i64;
        for (w, len, _) in lane.posted.clone() {
            let posted = Posted {
                cookie: 0,
                addr: lane.windows[w as usize].ptr(),
                len: len as usize,
                keep: None,
            };
            let b = record::read(&posted, &Lease::new(None)).unwrap();
            let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
            for v in a.values() {
                assert_eq!(*v, next);
                next += 1;
            }
        }
        assert_eq!((next, lane.released), (5000, 1));
    }

    #[test]
    fn a_row_bigger_than_a_window_ends_the_task() {
        let mut lane = Fake::new(&[ints(1)], 1, 4096);
        assert_eq!(run(&mut lane, echo), code::WINDOW_SMALL);
        assert_eq!(lane.released, 0, "never released");
        assert!(
            lane.log[0].starts_with("fake: the output record takes "),
            "{:?}",
            lane.log
        );
        // Two rows that do not fit, one row that does not either.
        let mut lane = Fake::new(&[ints(2)], 1, 4096);
        assert_eq!(run(&mut lane, echo), code::WINDOW_SMALL);
    }

    #[test]
    fn a_function_error_ends_the_task_with_its_message() {
        let mut lane = Fake::new(&[ints(1), ints(1)], 1, 1 << 16);
        let mut calls = 0;
        let code = run(&mut lane, |_b, _out| {
            calls += 1;
            Err(FunctionError::new("no column x"))
        });
        assert_eq!((code, calls, lane.released), (code::REFUSED, 1, 0));
        assert_eq!(lane.log, ["fake: no column x"]);
    }

    #[test]
    fn an_emit_error_ends_the_task_even_if_the_function_goes_on() {
        let mut lane = Fake::new(&[ints(1)], 1, 4096);
        let code = run(&mut lane, |b, out| {
            let first = out.batch(&b).unwrap_err();
            assert_eq!(out.batch(&b).unwrap_err(), first, "the same error again");
            assert!(format!("{out:?}").contains("failed: Some"));
            Ok(())
        });
        assert_eq!(code, code::WINDOW_SMALL);
    }

    #[test]
    fn a_kept_batch_ends_the_task_and_is_never_released() {
        let kept: std::cell::RefCell<Vec<ArrayRef>> = Default::default();
        let mut lane = Fake::new(&[ints(4), ints(4)], 1, 1 << 16);
        let code = run(&mut lane, |b, out| {
            out.batch(&b)?;
            kept.borrow_mut().push(b.column(0).clone());
            Ok(())
        });
        assert_eq!((code, lane.released), (code::REFUSED, 0));
        assert_eq!(lane.log, [format!("fake: {KEPT}")]);
        assert_eq!(
            lane.posted.len(),
            1,
            "the answer was posted, the batch kept"
        );
        // A copy is the function's own.
        let mut lane = Fake::new(&[ints(4)], 1, 1 << 16);
        let mut copies = Vec::new();
        let code = run(&mut lane, |b, _out| {
            let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
            copies.push(Int64Array::from(a.values().to_vec()));
            Ok(())
        });
        assert_eq!((code, lane.released, copies.len()), (code::DONE, 1, 1));
    }

    #[test]
    fn broken_rings_end_the_task() {
        let mut lane = Fake::new(&[ints(1)], 1, 1 << 16);
        lane.bad_batch = true;
        assert_eq!(run(&mut lane, echo), code::RINGS);
        assert_eq!(lane.log, ["fake: the rings: an input past the region"]);
        let mut lane = Fake::new(&[ints(1)], 1, 1 << 16);
        lane.bad_window = true;
        assert_eq!(run(&mut lane, echo), code::RINGS);
        let mut lane = Fake::new(&[ints(1)], 1, 1 << 16);
        lane.full = true;
        assert_eq!(run(&mut lane, echo), code::RINGS);
        assert_eq!(lane.log, ["fake: the rings: the output ring is full"]);
        let mut lane = Fake::new(&[ints(1)], 1, 1 << 16);
        lane.free.clear();
        lane.no_window = true;
        assert_eq!(run(&mut lane, echo), code::RINGS);
    }

    /// Several types, nulls, nesting and an offset into shared buffers.
    fn mixed(n: usize) -> RecordBatch {
        use arrow_array::types::Int32Type;
        use arrow_array::{BooleanArray, ListArray, StringArray, StructArray};
        let i: ArrayRef = Arc::new(Int64Array::from_iter(
            (0..n as i64).map(|v| (v % 3 != 0).then_some(v)),
        ));
        let s: ArrayRef = Arc::new(StringArray::from_iter(
            (0..n).map(|v| (v % 5 != 0).then(|| format!("s{v}"))),
        ));
        let b: ArrayRef = Arc::new(BooleanArray::from_iter((0..n).map(|v| Some(v % 2 == 0))));
        let l: ArrayRef = Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(
            (0..n).map(|v| Some((0..(v % 4) as i32).map(Some).collect::<Vec<_>>())),
        ));
        let st: ArrayRef = Arc::new(StructArray::from(vec![(
            Arc::new(arrow_schema::Field::new(
                "x",
                arrow_schema::DataType::Int64,
                true,
            )),
            i.clone(),
        )]));
        RecordBatch::try_from_iter([("i", i), ("s", s), ("b", b), ("l", l), ("st", st)])
            .unwrap()
            .slice(1, n - 2)
    }

    /// Every byte of the framing flipped three ways, and every one of the record's 8-byte words
    /// set to 0xff: the task refuses or answers, and never panics. The check runs before arrow-ipc
    /// reads anything (contracts CT-I13).
    #[test]
    fn a_damaged_record_is_refused_not_a_panic() {
        for batch in [ints(10), mixed(40)] {
            let good = record::encode(&batch).unwrap().to_vec().unwrap();
            let Ok(Need::Framing(end)) = framing_need(&good) else {
                panic!()
            };
            let mut damaged = Vec::new();
            for i in 0..end {
                for flip in [0x01u8, 0x80, 0xff] {
                    let mut r = good.clone();
                    r[i] ^= flip;
                    damaged.push(r);
                }
            }
            for i in (0..good.len()).step_by(8) {
                let mut r = good.clone();
                r[i..i + 8].fill(0xff);
                damaged.push(r);
            }
            for r in damaged {
                let mut lane = Fake::records(&[r], 1, 1 << 20);
                let code = run(&mut lane, echo);
                assert!(code == code::REFUSED || code == code::DONE, "{code}");
            }
        }
        let mut lane = Fake::new(&[ints(10)], 1, 1 << 16);
        let good = lane.inputs[0].bytes().to_vec();
        let Ok(Need::Framing(end)) = framing_need(&good) else {
            panic!()
        };
        for cut in [0, 7, 8, 100, end - 1, end, good.len() - 1] {
            let mut lane = Fake::new(&[ints(10)], 1, 1 << 16);
            lane.inputs[0].len = cut;
            assert_eq!(run(&mut lane, echo), code::REFUSED, "cut at {cut}");
            assert!(
                lane.log[0].starts_with("fake: the input record: "),
                "{:?}",
                lane.log
            );
        }
    }

    #[test]
    fn many_records_wait_for_windows() {
        let mut lane = Fake::new(&[ints(10)], 1, 1 << 16);
        let code = run(&mut lane, |b, out| {
            for _ in 0..5 {
                out.batch(&b)?;
            }
            Ok(())
        });
        assert_eq!((code, lane.posted.len()), (code::DONE, 5));
        assert!(
            lane.posted.iter().all(|p| p.0 == 0),
            "one window, given back each time"
        );
    }
}
