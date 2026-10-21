//! What a kernel prints and logs, attributed to the stage that printed it (MH 4.3 `log`).
//!
//! Output is captured where it leaves the code that wrote it, before the output of concurrent
//! stages is merged: a worker enters a stage's context ([`enter`]) around every call it makes
//! into that stage's kernel, and a writer that knows where it is (the in-process Python streams,
//! a `.pex` child's pipes opened while the context held, a Rust kernel calling [`write`] or
//! [`log`]) hands its bytes to the run's [`OutputSink`] with that stage. Bytes written with no
//! context, or on a thread the kernel started itself, are the engine's: they are never guessed
//! onto a stage.
//!
//! Nothing a kernel writes is read as anything but its output: the sink records it, it never
//! changes a stage's state, and a kernel has no way to write the platform's own records.

use std::cell::RefCell;
use std::sync::Arc;

use crate::ids::StageId;

/// Which of its streams a kernel wrote to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stream {
    /// Its standard output.
    Stdout,
    /// Its standard error. Writing here says nothing about severity.
    Stderr,
    /// A logging framework's record, whose level the framework said.
    Structured,
}

/// A logging framework's level, when it said one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// Finer than debug.
    Trace,
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
    /// Critical: past error.
    Critical,
}

impl Level {
    /// Python's `logging` level number, mapped to the nearest level at or below it.
    pub fn of_python(levelno: i64) -> Level {
        match levelno {
            n if n >= 50 => Level::Critical,
            n if n >= 40 => Level::Error,
            n if n >= 30 => Level::Warning,
            n if n >= 20 => Level::Info,
            n if n >= 10 => Level::Debug,
            _ => Level::Trace,
        }
    }

    /// The level by its lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Critical => "critical",
        }
    }
}

/// Where a run's captured output goes. Every method returns at once: a sink that cannot keep up
/// drops what it cannot hold and counts it, and never makes the writer wait.
pub trait OutputSink: Send + Sync {
    /// Bytes of `stream`, as written: lines are found and kept whole by the sink. `stage` is
    /// `None` for output no stage can be named for.
    fn bytes(&self, stage: Option<StageId>, stream: Stream, bytes: &[u8]);

    /// One record of a logging framework: its level when it said one, and its whole text (a
    /// record that carries an exception carries it here, every line of it).
    fn record(&self, stage: Option<StageId>, level: Option<Level>, text: &str);

    /// A writer that will go on writing after the call that opened it returned (a `.pex`
    /// child's pipes) holds this until its last byte is handed over, so the run's output is not
    /// called complete while it still has some.
    fn hold(&self) -> Box<dyn Send>;
}

thread_local! {
    static CONTEXT: RefCell<Option<(Arc<dyn OutputSink>, StageId)>> = const { RefCell::new(None) };
}

/// The stage this thread is working for until the guard drops; the context it replaced comes
/// back then.
pub struct Entered {
    previous: Option<(Arc<dyn OutputSink>, StageId)>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CONTEXT.with(|c| *c.borrow_mut() = previous);
    }
}

/// Work for `stage`, whose output goes to `sink`, until the guard drops.
pub fn enter(sink: &Arc<dyn OutputSink>, stage: StageId) -> Entered {
    let previous = CONTEXT.with(|c| c.borrow_mut().replace((Arc::clone(sink), stage)));
    Entered { previous }
}

/// The sink and the stage this thread works for, when it works for one.
pub fn current() -> Option<(Arc<dyn OutputSink>, StageId)> {
    CONTEXT.with(|c| c.borrow().clone())
}

/// Write `bytes` to `stream` as the stage this thread works for. `false`, and nothing written,
/// when the thread works for none: the caller writes where it would have.
pub fn write(stream: Stream, bytes: &[u8]) -> bool {
    match current() {
        Some((sink, stage)) => {
            sink.bytes(Some(stage), stream, bytes);
            true
        }
        None => false,
    }
}

/// Log one record as the stage this thread works for: what a Rust kernel uses to have its
/// messages attributed and levelled. `false` when the thread works for none.
pub fn log(level: Level, text: &str) -> bool {
    match current() {
        Some((sink, stage)) => {
            sink.record(Some(stage), Some(level), text);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen(Mutex<Vec<(Option<StageId>, String)>>);

    impl OutputSink for Seen {
        fn bytes(&self, stage: Option<StageId>, _: Stream, bytes: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push((stage, String::from_utf8_lossy(bytes).into_owned()));
        }
        fn record(&self, stage: Option<StageId>, _: Option<Level>, text: &str) {
            self.0.lock().unwrap().push((stage, text.to_string()));
        }
        fn hold(&self) -> Box<dyn Send> {
            Box::new(())
        }
    }

    /// Output goes to the stage the thread works for, a nested context gives the outer one
    /// back, and a thread working for none writes nothing here.
    #[test]
    fn output_is_attributed_by_the_context_it_was_written_in() {
        let seen = Arc::new(Seen::default());
        let sink: Arc<dyn OutputSink> = seen.clone();
        assert!(!write(Stream::Stdout, b"before"));
        {
            let _one = enter(&sink, 1);
            assert!(write(Stream::Stdout, b"one"));
            {
                let _two = enter(&sink, 2);
                assert!(log(Level::Error, "two"));
            }
            assert!(write(Stream::Stderr, b"one again"));
            let other = std::thread::spawn(|| write(Stream::Stdout, b"elsewhere"));
            assert!(!other.join().unwrap());
        }
        assert!(current().is_none());
        assert_eq!(
            *seen.0.lock().unwrap(),
            vec![
                (Some(1), "one".to_string()),
                (Some(2), "two".to_string()),
                (Some(1), "one again".to_string()),
            ]
        );
    }

    #[test]
    fn python_levels_map_down() {
        assert_eq!(Level::of_python(10), Level::Debug);
        assert_eq!(Level::of_python(25), Level::Info);
        assert_eq!(Level::of_python(30), Level::Warning);
        assert_eq!(Level::of_python(55), Level::Critical);
        assert_eq!(Level::of_python(0), Level::Trace);
    }
}
