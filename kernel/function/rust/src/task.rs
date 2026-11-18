//! A task driven one call at a time: what the C header's calls run on (system design 5B).
//!
//! [`crate::serve`] calls a function on each batch and releases the batch when the call returns.
//! The C header (`kernel/function/c/moruna_function.h`) turns that around: the function asks for
//! each batch, holds it until it releases it, and writes batches when it likes. [`Task`] is that,
//! on the same lane, records, errors and exit codes as `serve`; the C library
//! (`kernel/function/c`) is the Arrow C Data Interface over it. It is not part of the Rust SDK's
//! interface (5A): a Rust function calls `serve`.
//!
//! How it keeps the ring protocol, which `serve` keeps by its shape:
//!
//! - **A batch is given back** once nothing holds any part of it (its buffers are the record's
//!   memory, each holding the batch's lease, so the last one dropped says so), and in the order
//!   the batches were taken: a batch let go before an older one is given back with it. A batch
//!   let go is given back at the task's next [`Task::next_batch`] or [`Task::finish`], before either
//!   waits for anything, so a batch the function writes after letting go of its input still
//!   answers that input, as a record posted before a release answers the batch released.
//! - **A batch written answers the oldest batch held**, taken and not yet given back: a task
//!   answers its batches in turn (`moruna_plane::ring`). One written with none held (before the
//!   first batch, or after the engine's end, as an aggregate writes) answers no input, as
//!   [`crate::Function::end`]'s do.
//! - **A failure ends the task**, as in `serve`: its line goes to the log at once, every later
//!   call returns its code, no batch is given back after it, and [`Task::finish`] says the code.
//! - **[`Task::finish`] says [`code::DONE`]** only as `serve` does: every batch answered and given
//!   back, and the engine posting no more. A batch still held, or an end before the engine's, is
//!   a refusal.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::RecordBatch;

use crate::code;
use crate::error::FunctionError;
use crate::lane::{Lane, Mapped, NO_INPUT, Posted};
use crate::platform;
use crate::record::{self, Lease};

/// What a task that ended before the engine's end is told.
pub const BEFORE_THE_END: &str =
    "the function finished before moruna_next said the engine posts no more";

/// A batch taken and not yet given back.
struct Held {
    cookie: u64,
    /// Set once nothing holds any part of the batch.
    gone: Arc<AtomicBool>,
}

/// What every buffer of a batch holds on to, through its lease: the memory it lies in, and the
/// flag its drop sets, after the last part of the batch is dropped.
struct LetGo {
    gone: Arc<AtomicBool>,
    _keep: Option<Arc<dyn Mapped>>,
}

impl Drop for LetGo {
    fn drop(&mut self) {
        self.gone.store(true, Ordering::Release);
    }
}

/// A task's life on its rings, a call at a time.
pub struct Task {
    lane: Box<dyn Lane>,
    held: VecDeque<Held>,
    /// The code of the failure that ended the task.
    failed: Option<i64>,
    /// Batches given back.
    answered: u64,
}

impl Task {
    /// The task's plane and its rings, once: `Err` with the exit code when there are none, the
    /// line saying why in the log.
    pub fn open() -> Result<Task, i64> {
        match platform::attach() {
            Ok(lane) => Ok(Task::over(Box::new(lane))),
            Err(unattached) => {
                platform::log_line(&unattached.line);
                Err(unattached.code)
            }
        }
    }

    fn over(lane: Box<dyn Lane>) -> Task {
        Task {
            lane,
            held: VecDeque::new(),
            failed: None,
            answered: 0,
        }
    }

    /// The next batch, read in place, `None` once the engine posts no more; `Err` with the exit
    /// code once the task has failed. The batch is held until every part of it is dropped.
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch>, i64> {
        if let Some(code) = self.failed {
            return Err(code);
        }
        loop {
            self.give_back();
            match self.lane.batch() {
                Ok(Some(posted)) => return self.take(posted).map(Some),
                Ok(None) if self.lane.ended() => return Ok(None),
                Ok(None) => self.lane.wait_input(),
                Err(e) => return Err(self.fail(&FunctionError::rings(e))),
            }
        }
    }

    /// Holds `posted` and reads its batch.
    fn take(&mut self, posted: Posted) -> Result<RecordBatch, i64> {
        let gone = Arc::new(AtomicBool::new(false));
        self.held.push_back(Held {
            cookie: posted.cookie,
            gone: gone.clone(),
        });
        let let_go: Arc<dyn Mapped> = Arc::new(LetGo {
            gone,
            _keep: posted.keep.clone(),
        });
        let lease = Lease::new(Some(let_go));
        record::read(&posted, &lease).map_err(|e| self.fail(&e))
    }

    /// Gives back, oldest first, every batch let go of that is not behind one still held.
    fn give_back(&mut self) {
        if self.failed.is_some() {
            return;
        }
        while self
            .held
            .front()
            .is_some_and(|h| h.gone.load(Ordering::Acquire))
        {
            self.held.pop_front();
            self.lane.release();
            self.answered += 1;
        }
    }

    /// Writes `batch` into windows as the answer to the oldest batch held, or to none when none
    /// is held; `Err` with the exit code once the task has failed, this write's failure
    /// included.
    pub fn emit(&mut self, batch: &RecordBatch) -> Result<(), i64> {
        if let Some(code) = self.failed {
            return Err(code);
        }
        let cookie = self.held.front().map_or(NO_INPUT, |h| h.cookie);
        match record::write(&mut *self.lane, batch, cookie) {
            Ok(_) => Ok(()),
            Err(e) => Err(self.fail(&e)),
        }
    }

    /// Ends the task with `e`: its line in the log, after the task's name, and its code, the
    /// code of the first failure when the task had already failed. The code the task ends with.
    pub fn fail(&mut self, e: &FunctionError) -> i64 {
        let line = format!("{}: {}", self.lane.name(), e.message());
        self.lane.log(&line);
        *self.failed.get_or_insert(e.code())
    }

    /// The code of the failure that ended the task, if one did.
    pub fn failed(&self) -> Option<i64> {
        self.failed
    }

    /// A line in the task's log, as given; a line with newlines is several.
    pub fn log(&mut self, line: &str) {
        self.lane.log(line);
    }

    /// Ends the task: says its exit code on the rings and returns it, for `main` to return.
    pub fn finish(mut self) -> i64 {
        self.give_back();
        if self.failed.is_none() {
            if !self.held.is_empty() {
                let n = self.held.len();
                self.fail(&FunctionError::new(format!(
                    "the function finished holding {n} batches it took from moruna_next: a \
                     batch is given back to the engine once its release is called"
                )));
            } else if !self.lane.ended() {
                self.fail(&FunctionError::new(BEFORE_THE_END));
            }
        }
        let code = match self.failed {
            Some(code) => code,
            None => {
                let line = format!("{}: answered {} batches", self.lane.name(), self.answered);
                self.lane.log(&line);
                code::DONE
            }
        };
        self.lane.exit(code)
    }
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("name", &self.lane.name())
            .field("held", &self.held.len())
            .field("failed", &self.failed)
            .field("answered", &self.answered)
            .finish()
    }
}

#[cfg(all(test, not(any(target_os = "moruna", moruna_function_sys))))]
mod tests {
    use super::*;
    use crate::host::Plane;
    use crate::lane::Window;
    use arrow_array::{ArrayRef, Int64Array};
    use std::process::ExitCode;

    fn ints(from: i64, n: i64) -> RecordBatch {
        let a: ArrayRef = Arc::new(Int64Array::from_iter_values(from..from + n));
        RecordBatch::try_from_iter([("a", a)]).unwrap()
    }

    fn exit(code: i64) -> ExitCode {
        ExitCode::from(u8::try_from(code).unwrap())
    }

    #[test]
    fn batches_are_taken_answered_and_given_back_in_turn() {
        let outcome = Plane::new()
            .named("pull")
            .run([ints(0, 3), ints(3, 0), ints(3, 4)], || {
                let mut t = Task::open().unwrap();
                while let Some(b) = t.next_batch().unwrap() {
                    t.emit(&b).unwrap();
                }
                // The last call gave the last batch back before it saw the end.
                assert!(format!("{t:?}").contains("held: 0, failed: None, answered: 3"));
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE));
        assert_eq!(outcome.batches(), [ints(0, 3), ints(3, 0), ints(3, 4)]);
        assert!(outcome.answers.iter().all(|a| a.released));
        assert_eq!(outcome.log, ["pull: answered 3 batches"]);
    }

    #[test]
    fn a_batch_written_after_its_input_is_let_go_still_answers_it() {
        let outcome = Plane::new()
            .run([ints(0, 2), ints(2, 2)], || {
                let mut t = Task::open().unwrap();
                while let Some(b) = t.next_batch().unwrap() {
                    let copy = ints(b.num_rows() as i64 * 10, 1);
                    drop(b);
                    t.emit(&copy).unwrap();
                }
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE));
        let answers: Vec<Vec<RecordBatch>> =
            outcome.answers.iter().map(|a| a.batches.clone()).collect();
        assert_eq!(answers, [vec![ints(20, 1)], vec![ints(20, 1)]]);
    }

    #[test]
    fn several_held_are_answered_oldest_first_and_given_back_in_order() {
        let outcome = Plane::new()
            .run([ints(0, 1), ints(1, 1), ints(2, 1)], || {
                let mut t = Task::open().unwrap();
                let a = t.next_batch().unwrap().unwrap();
                let b = t.next_batch().unwrap().unwrap();
                t.emit(&b).unwrap();
                // The newer let go first: nothing is given back until the older is.
                drop(b);
                t.emit(&a).unwrap();
                let column = a.column(0).clone();
                drop(a);
                t.emit(&ints(9, 1)).unwrap();
                drop(column);
                let c = t.next_batch().unwrap().unwrap();
                t.emit(&c).unwrap();
                drop(c);
                assert!(t.next_batch().unwrap().is_none());
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE));
        let answers: Vec<Vec<RecordBatch>> =
            outcome.answers.iter().map(|a| a.batches.clone()).collect();
        assert_eq!(
            answers,
            [
                vec![ints(1, 1), ints(0, 1), ints(9, 1)],
                vec![],
                vec![ints(2, 1)]
            ]
        );
        assert!(outcome.answers.iter().all(|a| a.released));
    }

    #[test]
    fn a_batch_written_with_none_held_answers_no_input() {
        let outcome = Plane::new()
            .named("sum")
            .run([ints(0, 3), ints(3, 2)], || {
                let mut t = Task::open().unwrap();
                t.emit(&ints(-1, 1)).unwrap();
                let mut sum = 0;
                while let Some(b) = t.next_batch().unwrap() {
                    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                    sum += a.values().iter().sum::<i64>();
                }
                t.emit(&ints(sum, 1)).unwrap();
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE));
        assert!(
            outcome
                .answers
                .iter()
                .all(|a| a.released && a.records.is_empty())
        );
        assert_eq!(outcome.unheld.batches, [ints(-1, 1), ints(10, 1)]);
        assert!(!outcome.unheld.released);
        assert_eq!(outcome.log, ["sum: answered 2 batches"]);
    }

    #[test]
    fn a_batch_still_held_at_the_finish_is_a_refusal_and_never_given_back() {
        let outcome = Plane::new()
            .named("keeper")
            .run([ints(0, 1), ints(1, 1)], || {
                let mut t = Task::open().unwrap();
                let mut kept = Vec::new();
                while let Some(b) = t.next_batch().unwrap() {
                    t.emit(&b).unwrap();
                    kept.push(b);
                }
                let code = t.finish();
                // What it held is still readable after the finish.
                assert_eq!(kept[1], ints(1, 1));
                exit(code)
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED));
        assert!(outcome.answers.iter().all(|a| !a.released));
        assert_eq!(outcome.batches(), [ints(0, 1), ints(1, 1)]);
        assert!(
            outcome.log[0].starts_with("keeper: the function finished holding 2 batches"),
            "{:?}",
            outcome.log
        );
    }

    #[test]
    fn a_finish_before_the_engines_end_is_a_refusal() {
        let outcome = Plane::new()
            .named("quitter")
            .run([ints(0, 1), ints(1, 1)], || {
                let mut t = Task::open().unwrap();
                let b = t.next_batch().unwrap().unwrap();
                t.emit(&b).unwrap();
                drop(b);
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED));
        assert_eq!(outcome.log, [format!("quitter: {BEFORE_THE_END}")]);
        assert!(outcome.answers[0].released, "given back at the finish");
        assert!(!outcome.answers[1].released);
    }

    #[test]
    fn a_failure_ends_the_task_and_nothing_is_given_back_after_it() {
        let outcome = Plane::new()
            .named("picky")
            .windows(1, 4096)
            .run([ints(0, 1), ints(1, 1)], || {
                let mut t = Task::open().unwrap();
                let b = t.next_batch().unwrap().unwrap();
                assert_eq!(t.emit(&b), Err(code::WINDOW_SMALL));
                drop(b);
                // A later failure is logged, and the first one's code kept.
                assert_eq!(
                    t.fail(&FunctionError::new("no column named x")),
                    code::WINDOW_SMALL
                );
                assert_eq!(t.next_batch().map(|b| b.is_some()), Err(code::WINDOW_SMALL));
                assert_eq!(t.failed(), Some(code::WINDOW_SMALL));
                t.log("after");
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::WINDOW_SMALL));
        assert!(outcome.answers.iter().all(|a| !a.released));
        assert_eq!(outcome.log.len(), 3, "{:?}", outcome.log);
        assert!(outcome.log[0].starts_with("picky: the output record takes "));
        assert_eq!(outcome.log[1..], ["picky: no column named x", "after"]);
    }

    #[test]
    fn a_damaged_input_ends_the_task_unreleased() {
        let outcome = Plane::new()
            .run([crate::host::Input::Record(vec![0xff; 4096])], || {
                let mut t = Task::open().unwrap();
                assert_eq!(t.next_batch().map(|b| b.is_some()), Err(code::REFUSED));
                exit(t.finish())
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::REFUSED));
        assert!(!outcome.answers[0].released);
        assert!(outcome.log[0].starts_with("function: the input record: "));
    }

    #[test]
    fn a_task_outside_a_plane_or_opened_twice_finds_none() {
        assert_eq!(Task::open().unwrap_err(), code::NO_PLANE);
        let outcome = Plane::new()
            .run([ints(0, 1)], || {
                let t = Task::open().unwrap();
                assert_eq!(Task::open().unwrap_err(), code::NO_PLANE);
                drop(t);
                ExitCode::SUCCESS
            })
            .unwrap();
        assert!(outcome.log[0].contains("no plane"), "{:?}", outcome.log);
    }

    /// A lane whose rings are broken.
    struct Broken;

    impl Lane for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        fn batch(&mut self) -> Result<Option<Posted>, String> {
            Err(String::from("an input past the region"))
        }
        fn ended(&mut self) -> bool {
            false
        }
        fn wait_input(&mut self) {}
        fn window_bytes(&self) -> u64 {
            0
        }
        fn window(&mut self) -> Result<Option<Window>, String> {
            Ok(None)
        }
        fn wait_window(&mut self) -> bool {
            false
        }
        fn post(&mut self, _window: u64, _len: u64, _cookie: u64) -> bool {
            false
        }
        fn release(&mut self) {}
        fn exit(&mut self, code: i64) -> i64 {
            code
        }
        fn log(&mut self, _line: &str) {}
    }

    #[test]
    fn broken_rings_end_the_task() {
        let mut t = Task::over(Box::new(Broken));
        assert_eq!(t.next_batch().map(|b| b.is_some()), Err(code::RINGS));
        assert_eq!(t.finish(), code::RINGS);
    }
}
