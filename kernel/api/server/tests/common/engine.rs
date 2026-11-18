//! The tests' engine (item 3.2, phase 1): it speaks the host protocol of
//! `moruna-hosted-engine.md` 4.3 as `moruna serve` does, as far as the node's `api` hears it:
//! `hello` when it starts, `heartbeat`, event records as NUL-marked frames (encoded by
//! `moruna-event`, as the engine's are), `report` and `exit`; and it keeps what it is sent
//! (`spec`, `cancel`) and the documents it is asked to check. The test says what the engine does
//! and when. The crate's unit tests and its end-to-end tests both use it.

use moruna_api::Refusal;
use moruna_api_server::{Checked, EngineHost, HostError};
use moruna_event::{Encoder, Event, LogStream, Stamp};
use std::{
    cell::RefCell,
    collections::VecDeque,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};

/// A run ID as the engine mints one: 32 lowercase hexadecimal digits.
pub const RUN: &str = "0123456789abcdef0123456789abcdef";
/// The engine process's own ID, as its records carry it.
const START: &str = "fedcba9876543210fedcba9876543210";

/// What the engine has been asked, and what it has said that the server has not yet read.
#[derive(Default)]
pub struct State {
    /// The documents handed to the check, in order, and the secrets' names with each.
    pub checked: Vec<Vec<u8>>,
    pub checked_names: Vec<Vec<String>>,
    /// The verdicts the next checks give, in order; a check with none finds the document sound.
    pub verdicts: VecDeque<Result<Vec<Refusal>, HostError>>,
    /// The functions the next checks answer the job uses (its `needs.functions`), in order; a
    /// check with none answers none.
    pub functions: VecDeque<Vec<String>>,
    /// While set, a check under way gives no verdict.
    pub hold_checks: bool,
    checking: bool,
    /// The `spec` line of each engine started.
    pub started: Vec<String>,
    /// What the next start answers, when it is not to start.
    pub refuse_start: Option<HostError>,
    /// The lines sent to the engine after its `spec`.
    pub sent: Vec<String>,
    /// What the engine has sent that the server has not read.
    pub outbox: VecDeque<u8>,
    /// The engine's channel has ended.
    pub ended: bool,
    run: String,
    seq: u64,
}

/// The engine, as the test holds it and as the server does: clones share one state.
#[derive(Clone, Default)]
pub struct Engine(pub Rc<RefCell<State>>);

impl Engine {
    pub fn new() -> Self {
        let engine = Self::default();
        engine.0.borrow_mut().run = RUN.to_string();
        engine
    }

    /// The run ID the engine's next records carry.
    pub fn run_as(&self, run: &str) {
        self.0.borrow_mut().run = run.to_string();
    }

    /// The engine sends this line.
    pub fn say(&self, line: &str) {
        let mut state = self.0.borrow_mut();
        state.outbox.extend(line.as_bytes());
        state.outbox.push_back(b'\n');
    }

    /// The engine sends these bytes as they are.
    pub fn bytes(&self, bytes: &[u8]) {
        self.0.borrow_mut().outbox.extend(bytes);
    }

    /// The engine tells `event`, as a framed record numbered after its last.
    pub fn event(&self, event: &Event) {
        let mut state = self.0.borrow_mut();
        state.seq += 1;
        let stamp = Stamp {
            run: Some(&state.run),
            plan: Some("plan-1"),
            attempt: 1,
            start: START,
        };
        let mut encoder = Encoder::new();
        let record = encoder
            .encode(
                &stamp,
                state.seq,
                1_794_931_200_000_000_000,
                state.seq * 10,
                event,
            )
            .expect("the event is a record")
            .to_vec();
        state.outbox.push_back(0);
        state.outbox.extend(record);
    }

    /// The first event of a run, which tells its ID.
    pub fn attempt_started(&self) {
        self.event(&Event::AttemptStarted {
            resumed: false,
            plan: Some("plan-1".to_string()),
        });
    }

    /// A record of output: one line a task printed.
    pub fn log(&self, task: &str, text: &str) {
        self.event(&Event::Log {
            task: Some(task.to_string()),
            stream: LogStream::Stdout,
            level: None,
            bytes: text.len() as u64,
            text: text.to_string(),
            partial: false,
            continued: false,
            truncated: 0,
        });
    }

    /// A heartbeat with this many rows out.
    pub fn heartbeat(&self, rows: u64) {
        self.say(&format!(
            r#"{{"type":"heartbeat","t_ms":{},"committed_seq":null,"rows_out":{rows},"bytes_out":{},"active_workers":2,"ceiling_bytes":1073741824,"anon_bytes":4096,"bottleneck":"source","staging_bytes":0}}"#,
            rows * 10,
            rows * 8
        ));
    }

    /// The report, then the exit: how a run ends.
    pub fn finish(&self, report: &str, code: i32, diagnostic: Option<&str>) {
        self.say(&format!(
            r#"{{"type":"report","report":{report},"plan":{{"id":"plan-1"}}}}"#
        ));
        self.exit(code, diagnostic);
    }

    /// The last message, then the end of the channel.
    pub fn exit(&self, code: i32, diagnostic: Option<&str>) {
        let diagnostic = match diagnostic {
            Some(text) => format!("\"{text}\""),
            None => "null".to_string(),
        };
        self.say(&format!(
            r#"{{"type":"exit","code":{code},"diagnostic":{diagnostic}}}"#
        ));
        self.0.borrow_mut().ended = true;
    }

    /// The `spec` the last engine was started with, parsed.
    pub fn spec(&self) -> serde_json::Value {
        let state = self.0.borrow();
        serde_json::from_str(state.started.last().expect("an engine was started")).unwrap()
    }
}

impl EngineHost for Engine {
    fn check(&mut self, document: &[u8], secrets: &[String]) -> Result<(), HostError> {
        let mut state = self.0.borrow_mut();
        assert!(!state.checking, "one check at a time");
        state.checked.push(document.to_vec());
        state.checked_names.push(secrets.to_vec());
        state.checking = true;
        Ok(())
    }

    fn checked(&mut self) -> Option<Result<Checked, HostError>> {
        let mut state = self.0.borrow_mut();
        if !state.checking || state.hold_checks {
            return None;
        }
        state.checking = false;
        let functions = state.functions.pop_front().unwrap_or_default();
        let verdict = state.verdicts.pop_front().unwrap_or(Ok(Vec::new()));
        Some(verdict.map(|refusals| Checked {
            refusals,
            functions,
        }))
    }

    fn start(&mut self, spec: &[u8]) -> Result<(), HostError> {
        let mut state = self.0.borrow_mut();
        if let Some(refusal) = state.refuse_start.take() {
            return Err(refusal);
        }
        assert!(
            spec.ends_with(b"\n") && !spec[..spec.len() - 1].contains(&b'\n'),
            "one line"
        );
        state
            .started
            .push(String::from_utf8(spec.to_vec()).unwrap());
        state.outbox.clear();
        state.ended = false;
        state.seq = 0;
        // As the engine does on connect, before it has its document.
        state.outbox.extend(
            br#"{"type":"hello","moruna_version":"0.7.0","spec_digest":null,"limits":null}"#,
        );
        state.outbox.push_back(b'\n');
        Ok(())
    }

    fn send(&mut self, line: &[u8]) -> Result<(), HostError> {
        let mut state = self.0.borrow_mut();
        state.sent.push(String::from_utf8(line.to_vec()).unwrap());
        Ok(())
    }

    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, HostError> {
        let mut state = self.0.borrow_mut();
        if state.outbox.is_empty() {
            return if state.ended {
                Ok(0)
            } else {
                Err(HostError::WouldBlock)
            };
        }
        let count = buf.len().min(state.outbox.len());
        for (slot, byte) in buf.iter_mut().zip(state.outbox.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }
}
