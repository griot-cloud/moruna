//! Bounded actual Python output carried with its original native command reply.
use crate::native_wire::Log;
use moruna_kernel::{Level, OutputSink, Stream};
use std::sync::Mutex;
const MAX_BYTES: usize = 8192;
#[derive(Default)]
struct State {
    records: Vec<Log>,
    bytes: usize,
    failed: bool,
}
/// One command's bounded output; overflow refuses the command, never hides loss.
#[derive(Default)]
pub struct Capture(Mutex<State>);
impl Capture {
    fn push(&self, stream: u8, level: Option<u8>, bytes: &[u8]) {
        let Ok(mut state) = self.0.lock() else { return };
        // Count per-record framing too: empty writes cannot grow an unbounded Vec.
        let Some(total) = state.bytes.checked_add(bytes.len().saturating_add(64)) else {
            state.failed = true;
            return;
        };
        if total > MAX_BYTES {
            state.failed = true;
            return;
        }
        state.records.push(Log {
            stream,
            level,
            bytes: bytes.to_vec(),
        });
        state.bytes = total;
    }
    /// Take only actual observed records. A poisoned/overflowed capture is an error.
    pub fn finish(&self) -> Result<Vec<Log>, String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "native output capture poisoned")?;
        if state.failed {
            return Err("native output exceeds admitted command budget".into());
        }
        Ok(std::mem::take(&mut state.records))
    }
}
impl OutputSink for Capture {
    fn bytes(&self, _: Option<moruna_kernel::StageId>, stream: Stream, bytes: &[u8]) {
        self.push(
            match stream {
                Stream::Stdout => 1,
                Stream::Stderr => 2,
                Stream::Structured => 3,
            },
            None,
            bytes,
        )
    }
    fn record(&self, _: Option<moruna_kernel::StageId>, level: Option<Level>, text: &str) {
        self.push(
            3,
            level.map(|l| match l {
                Level::Trace => 0,
                Level::Debug => 1,
                Level::Info => 2,
                Level::Warning => 3,
                Level::Error => 4,
                Level::Critical => 5,
            }),
            text.as_bytes(),
        )
    }
    fn hold(&self) -> Box<dyn Send> {
        Box::new(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_output_context_preserves_stream_level_and_original_bytes() {
        let capture = std::sync::Arc::new(Capture::default());
        let sink: std::sync::Arc<dyn OutputSink> = capture.clone();
        {
            let _context = moruna_kernel::output::enter(&sink, 7);
            assert!(moruna_kernel::output::write(
                Stream::Stdout,
                b"actual\0bytes"
            ));
            assert!(moruna_kernel::output::write(Stream::Stderr, b"warning"));
            assert!(moruna_kernel::output::log(Level::Warning, "actual record"));
        }
        let records = capture.finish().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].stream, 1);
        assert_eq!(records[0].bytes, b"actual\0bytes");
        assert_eq!(records[1].stream, 2);
        assert_eq!(records[2].stream, 3);
        assert_eq!(records[2].level, Some(3));
        assert_eq!(records[2].bytes, b"actual record");
    }
    #[test]
    fn overflow_is_explicit_and_even_empty_writes_are_bounded() {
        let capture = Capture::default();
        for _ in 0..=MAX_BYTES / 64 {
            capture.bytes(None, Stream::Stdout, b"");
        }
        assert!(capture.finish().unwrap_err().contains("budget"));
    }
}
