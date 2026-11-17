//! The jobs (system design, sections 3A and 3B): one at a time, each in a fresh engine the node
//! starts; the last 100 with their state, heartbeat, exit and report; and the node's buffer of
//! the last event and log records, which a stream of events replays from.
//!
//! The server is the engine's host: it sends `spec` and `cancel`, and reads `hello`,
//! `heartbeat`, the framed event records, `report` and `exit`. It never reads a job document:
//! the engine checks it, and the run's ID is the one the engine's first event carries.

use crate::{
    api::Answer,
    engine::{ENGINE_LINE_LIMIT, EngineHost, HostError, Message, Wire},
    json::{Secrets, compact, is_object},
    token::base64,
};
use alloc::{boxed::Box, collections::VecDeque, format, string::String, vec, vec::Vec};
use moruna_api::{
    ErrorCode, Heartbeat, Job, JobState, JobStatus, JobSummary, Jobs as JobList, Refusal,
    Submitted, Validation,
};
use serde::Deserialize;
use serde_json::value::RawValue;

/// How many jobs are kept: the last 100.
pub const JOBS_KEPT: usize = 100;
/// How many event and log records the node's buffer keeps, at most: the last 100,000.
pub const EVENTS_KEPT: usize = 100_000;
/// The most bytes the node's buffer of records holds, each record as its JSON line: when the
/// last [`EVENTS_KEPT`] records are larger, the oldest go first.
pub const EVENT_BUFFER_BYTES: usize = 12 * 1024 * 1024;
/// The most bytes of reports kept, over all the jobs kept: the oldest reports go first.
pub const REPORT_BYTES: usize = 2 * 1024 * 1024;
/// What one record costs the buffer beyond its line.
const RECORD_OVERHEAD: usize = 32;
/// The most bytes of events one stream is handed at a time.
const STREAM_BATCH: usize = 32 * 1024;
/// The most bytes read from the engine in one poll.
const POLL_BYTES: usize = 4 * 1024 * 1024;
/// The exit code of a refused document, and of a cancelled run (`moruna-hosted-engine.md` 4.2).
const SPEC_REFUSED: i32 = 2;
const CANCELLED: i32 = 130;

/// One job the node has taken.
#[derive(Debug)]
struct Entry {
    /// Its number among the jobs this server has started, which no later job reuses.
    ordinal: u64,
    id: String,
    state: JobState,
    submitted: u64,
    started: Option<u64>,
    ended: Option<u64>,
    heartbeat: Option<Heartbeat>,
    /// The latest heartbeat as the engine wrote it, and how many there have been.
    heartbeat_line: Option<Box<str>>,
    heartbeats: u64,
    exit_code: Option<i32>,
    /// The `exit` message as the engine wrote it.
    exit_line: Option<Box<str>>,
    /// The report and the plan of the `report` message, each as the engine wrote it.
    report: Option<Box<str>>,
    plan: Option<Box<str>>,
    /// Where its first record stands in the buffer, and how many records it has.
    first: u64,
    records: u64,
    /// The engine's lines passed over for being longer than [`ENGINE_LINE_LIMIT`].
    skipped: u64,
}

impl Entry {
    fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id.clone(),
            state: self.state,
            submitted: self.submitted,
            started: self.started,
            ended: self.ended,
        }
    }

    fn status(&self) -> JobStatus {
        JobStatus {
            id: self.id.clone(),
            state: self.state,
            submitted: self.submitted,
            started: self.started,
            ended: self.ended,
            heartbeat: self.heartbeat.clone(),
            exit_code: self.exit_code,
        }
    }
}

/// The engine in flight: from `spec` to `exit`.
#[derive(Debug)]
struct Flight {
    ordinal: u64,
    /// The submit that waits to hear whether the engine takes the document.
    ticket: Option<u64>,
    /// What the job says is secret, kept to redact what the engine says until it exits.
    secrets: Secrets,
    submitted: u64,
    /// Whether the engine has told the run's ID, so that the job is in the list.
    listed: bool,
    /// A heartbeat and a report that came before the run's ID, and the lines passed over.
    heartbeat: Option<Box<str>>,
    report: Option<(Box<str>, Option<Box<str>>)>,
    skipped: u64,
}

/// A document waiting for the engine's check.
#[derive(Debug)]
struct Check {
    ticket: u64,
    document: Vec<u8>,
    /// The names of the job's secrets, which its document may refer to.
    names: Vec<String>,
    secrets: Secrets,
}

/// A job as its body gives it: what the engine is sent, and what it is checked as.
struct Parsed {
    /// The `spec` member of the host protocol's `spec` message: the version 1 document as one
    /// line of JSON, or the version 2 document's base64 as a JSON string.
    spec: String,
    /// The `secrets` member: the job's secrets by name, as JSON, when it has any.
    named: Option<String>,
    /// Their names.
    names: Vec<String>,
    secrets: Secrets,
}

impl Parsed {
    /// The job in `body`, or why it is not one.
    fn of(body: &[u8]) -> Result<Self, &'static str> {
        let job: Job<&RawValue> =
            serde_json::from_slice(body).map_err(|_| "the body is not a job: v1 or v2")?;
        let secrets = Secrets::new(job.secrets.iter())?;
        let names = job.secrets.keys().cloned().collect();
        let named = (!job.secrets.is_empty())
            .then(|| serde_json::to_string(&job.secrets))
            .transpose()
            .map_err(|_| "the secrets cannot be written")?;
        let spec = match (job.v1, job.v2) {
            (Some(document), None) if is_object(document.get()) => compact(document.get()),
            (Some(_), None) => return Err("v1 is a job document, a JSON object"),
            (None, Some(document)) => {
                if base64(document.as_bytes()).is_none_or(|bytes| bytes.is_empty()) {
                    return Err("v2 is a job document in standard base64");
                }
                format!("\"{document}\"")
            }
            _ => return Err("a job is exactly one of v1 and v2"),
        };
        Ok(Self {
            spec,
            named,
            names,
            secrets,
        })
    }

    /// The host protocol's `spec` message: the document, and the secrets beside it.
    fn message(&self) -> String {
        match &self.named {
            Some(named) => format!(
                "{{\"type\":\"spec\",\"spec\":{},\"secrets\":{named}}}\n",
                self.spec
            ),
            None => format!("{{\"type\":\"spec\",\"spec\":{}}}\n", self.spec),
        }
    }

    /// The document as the engine's check reads it: version 1's JSON, or version 2's bytes.
    fn document(&self) -> Vec<u8> {
        match self.spec.strip_prefix('"') {
            Some(rest) => base64(rest.trim_end_matches('"').as_bytes()).unwrap_or_default(),
            None => self.spec.clone().into_bytes(),
        }
    }
}

/// The `type` of a line of the host protocol.
#[derive(Deserialize)]
struct Typed {
    #[serde(rename = "type")]
    kind: String,
}

/// The `report` message.
#[derive(Deserialize)]
struct ReportLine<'a> {
    #[serde(borrow)]
    report: &'a RawValue,
    #[serde(borrow, default)]
    plan: Option<&'a RawValue>,
}

/// The `exit` message.
#[derive(Deserialize)]
struct ExitLine {
    code: i32,
    #[serde(default)]
    diagnostic: Option<String>,
}

/// Where a stream of a job's events has got to.
#[derive(Debug)]
pub(crate) struct Stream {
    ordinal: u64,
    /// The buffer position of the next record to send.
    next: u64,
    /// How many of the job's heartbeats had been sent or passed when last looked.
    heartbeats: u64,
    /// How many of the engine's skipped lines the stream has been told of.
    skipped: u64,
}

/// The jobs' state.
#[derive(Debug, Default)]
pub(crate) struct Jobs {
    /// The last [`JOBS_KEPT`] jobs, newest first.
    list: VecDeque<Entry>,
    flight: Option<Flight>,
    wire: Wire,
    /// The node's buffer of records, each as its JSON line, oldest first; `front` is the
    /// position of the first, and positions only grow.
    lines: VecDeque<Box<str>>,
    front: u64,
    bytes: usize,
    report_bytes: usize,
    /// Documents waiting for the engine's check, in order; the first is being checked when
    /// `checking`.
    checks: VecDeque<Check>,
    checking: bool,
    /// The check under way was let go by the call that asked for it.
    unwanted: bool,
    tickets: u64,
    ordinals: u64,
    /// The answers that waiting calls can now have.
    resolved: Vec<(u64, Answer)>,
}

/// An answer with `value` as its body and this status.
fn json(status: u16, written: serde_json::Result<Vec<u8>>, secrets: &Secrets) -> Answer {
    let mut answer = Answer::ok(written);
    if answer.status == 200 {
        answer.status = status;
    }
    answer.body = secrets
        .scrub(String::from_utf8(answer.body).unwrap_or_default())
        .into_bytes();
    answer
}

/// The error `code` with `message`, its secrets taken out.
fn refusal(code: ErrorCode, message: &str, secrets: &Secrets) -> Answer {
    let mut answer = Answer::error(code, message);
    answer.body = secrets
        .scrub(String::from_utf8(answer.body).unwrap_or_default())
        .into_bytes();
    answer
}

/// What a host's failure is answered as.
fn failed(error: HostError, secrets: &Secrets) -> Answer {
    match error {
        HostError::Refused(why) => refusal(ErrorCode::Invalid, &why, secrets),
        HostError::Failed(why) => refusal(ErrorCode::Internal, &why, secrets),
        HostError::WouldBlock | HostError::Unavailable => refusal(
            ErrorCode::Busy,
            "the node cannot run the engine now",
            secrets,
        ),
    }
}

impl Jobs {
    /// Whether an engine is in flight: a job is starting or running.
    pub(crate) fn busy(&self) -> bool {
        self.flight.is_some()
    }

    fn ticket(&mut self) -> u64 {
        self.tickets += 1;
        self.tickets
    }

    /// The answers waiting calls can now have, each with its ticket.
    pub(crate) fn resolved(&mut self) -> Vec<(u64, Answer)> {
        core::mem::take(&mut self.resolved)
    }

    /// `POST /v1/jobs/validate`: queues the document in `body` for the engine's check, and
    /// gives the ticket its verdict will come under; or the answer, when the body is no job.
    pub(crate) fn validate(&mut self, body: &[u8]) -> Result<u64, Answer> {
        let parsed = Parsed::of(body).map_err(|why| Answer::error(ErrorCode::Invalid, why))?;
        let ticket = self.ticket();
        self.checks.push_back(Check {
            ticket,
            document: parsed.document(),
            names: parsed.names,
            secrets: parsed.secrets,
        });
        Ok(ticket)
    }

    /// `POST /v1/jobs`: starts a fresh engine with the job in `body`, and gives the ticket the
    /// engine's verdict will come under; or the answer, when no engine was started.
    pub(crate) fn submit(
        &mut self,
        host: &mut dyn EngineHost,
        body: &[u8],
        now: u64,
    ) -> Result<u64, Answer> {
        if self.busy() {
            return Err(Answer::error(ErrorCode::Busy, "a job is running"));
        }
        let parsed = Parsed::of(body).map_err(|why| Answer::error(ErrorCode::Invalid, why))?;
        let mut spec = parsed.message();
        let started = host.start(spec.as_bytes());
        // The message held the secrets in the clear: overwrite it before it is let go.
        spec.replace_range(.., &"\0".repeat(spec.len()));
        started.map_err(|error| failed(error, &parsed.secrets))?;
        let ticket = self.ticket();
        self.ordinals += 1;
        self.wire.clear();
        self.flight = Some(Flight {
            ordinal: self.ordinals,
            ticket: Some(ticket),
            secrets: parsed.secrets,
            submitted: now,
            listed: false,
            heartbeat: None,
            report: None,
            skipped: 0,
        });
        Ok(ticket)
    }

    /// A call that waited under `ticket` has gone: its answer is not wanted.
    pub(crate) fn abandon(&mut self, ticket: u64) {
        self.resolved.retain(|(resolved, _)| *resolved != ticket);
        match self.checks.iter().position(|check| check.ticket == ticket) {
            Some(0) if self.checking => self.unwanted = true,
            Some(at) => {
                self.checks.remove(at);
            }
            None => {}
        }
        if let Some(flight) = &mut self.flight
            && flight.ticket == Some(ticket)
        {
            flight.ticket = None;
        }
    }

    /// `GET /v1/jobs`.
    pub(crate) fn list(&self) -> JobList {
        JobList {
            jobs: self.list.iter().map(Entry::summary).collect(),
        }
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.list.iter().find(|entry| entry.id == id)
    }

    /// `GET /v1/jobs/{id}`.
    pub(crate) fn status(&self, id: &str) -> Option<JobStatus> {
        self.entry(id).map(Entry::status)
    }

    /// `POST /v1/jobs/{id}/cancel`: sends the engine `cancel`.
    pub(crate) fn cancel(&mut self, host: &mut dyn EngineHost, id: &str) -> Answer {
        let Some(entry) = self.entry(id) else {
            return Answer::error(ErrorCode::NotFound, "no job has this ID");
        };
        if entry.state.ended() {
            return Answer::error(ErrorCode::Conflict, "the job has ended");
        }
        match host.send(b"{\"type\":\"cancel\"}\n") {
            Ok(()) => Answer::ok(serde_json::to_vec(&entry.status())),
            Err(error) => failed(error, &Secrets::default()),
        }
    }

    /// `GET /v1/jobs/{id}/report`: the report as the engine wrote it.
    pub(crate) fn report(&self, id: &str) -> Answer {
        match self.entry(id) {
            None => Answer::error(ErrorCode::NotFound, "no job has this ID"),
            Some(entry) if !entry.state.ended() => {
                Answer::error(ErrorCode::Conflict, "the job has not ended")
            }
            Some(entry) => match &entry.report {
                Some(report) => Answer::ok(Ok(report.as_bytes().to_vec())),
                None => Answer::error(ErrorCode::NotFound, "the node holds no report of this job"),
            },
        }
    }

    /// `GET /v1/jobs/{id}/events`: a stream of the job's events from its record `from` (its
    /// first when `None`), as far back as the buffer still holds them.
    pub(crate) fn stream(&self, id: &str, from: Option<u64>) -> Option<Stream> {
        let entry = self.entry(id)?;
        Some(Stream {
            ordinal: entry.ordinal,
            next: entry
                .first
                .saturating_add(from.unwrap_or(1).saturating_sub(1)),
            heartbeats: entry.heartbeats,
            skipped: 0,
        })
    }

    /// Writes to `out` what `stream` can be sent now, as Server-Sent Events: each record as
    /// `id`, `event: event` and its JSON line; a heartbeat that came since the last look; and,
    /// once the job has ended and its records are sent, its `report` and its `exit`. Says
    /// whether the stream has ended.
    pub(crate) fn fill(&self, stream: &mut Stream, out: &mut Vec<u8>) -> bool {
        let Some(entry) = self
            .list
            .iter()
            .find(|entry| entry.ordinal == stream.ordinal)
        else {
            return true;
        };
        let end = entry.first + entry.records;
        // Records the buffer has let go of are passed over: the IDs show the gap.
        stream.next = stream.next.max(self.front).max(entry.first);
        while stream.next < end && out.len() < STREAM_BATCH {
            if let Some(line) = self.lines.get((stream.next - self.front) as usize) {
                let id = stream.next - entry.first + 1;
                out.extend_from_slice(format!("id: {id}\nevent: event\ndata: ").as_bytes());
                out.extend_from_slice(line.as_bytes());
                out.extend_from_slice(b"\n\n");
            }
            stream.next += 1;
        }
        if stream.next < end {
            return false;
        }
        if entry.skipped > stream.skipped {
            stream.skipped = entry.skipped;
            event(
                out,
                "note",
                &format!(
                    "{{\"type\":\"note\",\"skipped_lines\":{0},\"message\":\"{0} of the engine's \
                     lines were longer than {ENGINE_LINE_LIMIT} bytes and were skipped\"}}",
                    entry.skipped
                ),
            );
        }
        if entry.heartbeats > stream.heartbeats {
            stream.heartbeats = entry.heartbeats;
            if let Some(line) = &entry.heartbeat_line {
                event(out, "heartbeat", line);
            }
        }
        if !entry.state.ended() {
            return false;
        }
        if let Some(report) = &entry.report {
            let plan = entry.plan.as_deref().unwrap_or("null");
            event(
                out,
                "report",
                &format!("{{\"type\":\"report\",\"report\":{report},\"plan\":{plan}}}"),
            );
        }
        if let Some(line) = &entry.exit_line {
            event(out, "exit", line);
        }
        true
    }

    /// Moves the checks and the engine in flight as far as they go now.
    pub(crate) fn pump(&mut self, host: &mut dyn EngineHost, now: u64) {
        self.pump_checks(host);
        if self.flight.is_none() {
            return;
        }
        let mut scratch = vec![0u8; 16 * 1024];
        let mut read = 0;
        while self.flight.is_some() && read < POLL_BYTES {
            match host.receive(&mut scratch) {
                Ok(0) => self.end(None, None, now),
                Ok(count) => {
                    read += count;
                    self.wire.push(&scratch[..count]);
                    while self.flight.is_some() {
                        let message = self.wire.next();
                        self.count_skipped();
                        match message {
                            Some(Message::Line(line)) => self.line(line, now),
                            Some(Message::Record(record)) => self.record(&record, now),
                            Some(Message::Garbled) => self.end(None, None, now),
                            None => break,
                        }
                    }
                }
                Err(HostError::WouldBlock) => break,
                Err(_) => self.end(None, None, now),
            }
        }
        self.wire.settle();
    }

    /// Hands the engine the next document to check, and takes the verdict of the one it has.
    fn pump_checks(&mut self, host: &mut dyn EngineHost) {
        if self.checking
            && let Some(verdict) = host.checked()
        {
            self.checking = false;
            if let Some(check) = self.checks.pop_front()
                && !core::mem::take(&mut self.unwanted)
            {
                let answer = match verdict {
                    Ok(refusals) => validation(refusals, &check.secrets),
                    Err(error) => failed(error, &check.secrets),
                };
                self.resolved.push((check.ticket, answer));
            }
        }
        while !self.checking {
            let Some(check) = self.checks.front() else {
                break;
            };
            match host.check(&check.document, &check.names) {
                Ok(()) => self.checking = true,
                Err(HostError::WouldBlock) => break,
                Err(error) => {
                    if let Some(check) = self.checks.pop_front() {
                        self.resolved
                            .push((check.ticket, failed(error, &check.secrets)));
                    }
                }
            }
        }
    }

    /// Counts the lines the wire has passed over for their length, for the job in flight.
    fn count_skipped(&mut self) {
        let skipped = self.wire.take_skipped();
        if skipped == 0 {
            return;
        }
        if let Some(entry) = self.current() {
            entry.skipped += skipped;
        } else if let Some(flight) = &mut self.flight {
            flight.skipped += skipped;
        }
    }

    /// The job in flight, once it is in the list.
    fn current(&mut self) -> Option<&mut Entry> {
        let flight = self.flight.as_ref().filter(|flight| flight.listed)?;
        let ordinal = flight.ordinal;
        self.list.iter_mut().find(|entry| entry.ordinal == ordinal)
    }

    /// One line of the host protocol from the engine.
    fn line(&mut self, line: Vec<u8>, now: u64) {
        let Some(flight) = &self.flight else {
            return;
        };
        let Ok(text) = String::from_utf8(line) else {
            return;
        };
        let text = flight.secrets.scrub(text);
        let Ok(typed) = serde_json::from_str::<Typed>(&text) else {
            return;
        };
        match typed.kind.as_str() {
            "heartbeat" => self.heartbeat(text.into_boxed_str(), now),
            "report" => {
                if let Ok(message) = serde_json::from_str::<ReportLine<'_>>(&text) {
                    let report = Box::from(message.report.get());
                    let plan = message.plan.map(|plan| Box::from(plan.get()));
                    self.keep_report(report, plan);
                }
            }
            "exit" => {
                if let Ok(exit) = serde_json::from_str::<ExitLine>(&text) {
                    let line = Some((exit.code, exit.diagnostic, text.into_boxed_str()));
                    self.end(line.as_ref().map(|l| l.0), line, now);
                }
            }
            // `hello`, `limits_changed` and the port messages change nothing the node keeps.
            _ => {}
        }
    }

    /// A heartbeat: the job's latest, and the sign that it runs.
    fn heartbeat(&mut self, line: Box<str>, now: u64) {
        let Some(entry) = self.current() else {
            if let Some(flight) = &mut self.flight {
                flight.heartbeat = Some(line);
            }
            return;
        };
        entry.heartbeat = serde_json::from_str(&line).ok();
        entry.heartbeat_line = Some(line);
        entry.heartbeats += 1;
        if entry.state == JobState::Starting {
            entry.state = JobState::Running;
            entry.started = Some(now);
        }
    }

    /// Keeps the job's report within what all the reports may take: the oldest go first, and a
    /// report larger than that is not kept.
    fn keep_report(&mut self, report: Box<str>, plan: Option<Box<str>>) {
        let Some(flight) = &mut self.flight else {
            return;
        };
        if !flight.listed {
            flight.report = Some((report, plan));
            return;
        }
        let ordinal = flight.ordinal;
        let size = report.len() + plan.as_ref().map_or(0, |plan| plan.len());
        if size > REPORT_BYTES {
            return;
        }
        for entry in self.list.iter_mut().rev() {
            if self.report_bytes + size <= REPORT_BYTES {
                break;
            }
            if entry.ordinal != ordinal
                && let Some(old) = entry.report.take()
            {
                self.report_bytes -= old.len() + entry.plan.take().map_or(0, |plan| plan.len());
            }
        }
        if let Some(entry) = self.list.iter_mut().find(|entry| entry.ordinal == ordinal) {
            self.report_bytes += size;
            entry.report = Some(report);
            entry.plan = plan;
        }
    }

    /// One event record from the engine: the first tells the run's ID, which makes the job; each
    /// is kept in the buffer as its JSON line.
    fn record(&mut self, record: &[u8], now: u64) {
        let Ok(event) = moruna_event::decode(record) else {
            return;
        };
        let Some(flight) = &mut self.flight else {
            return;
        };
        let Ok(line) = serde_json::to_string(&event.rendering()) else {
            return;
        };
        let line = flight.secrets.scrub(line);
        if !flight.listed {
            flight.listed = true;
            let id = event.run.unwrap_or(event.start);
            let (ordinal, submitted, skipped) = (flight.ordinal, flight.submitted, flight.skipped);
            let (heartbeat, report) = (flight.heartbeat.take(), flight.report.take());
            if let Some(ticket) = flight.ticket.take() {
                let submitted = Submitted { id: id.clone() };
                let answer = json(201, serde_json::to_vec(&submitted), &Secrets::default());
                self.resolved.push((ticket, answer));
            }
            self.list.push_front(Entry {
                ordinal,
                id,
                state: JobState::Starting,
                submitted,
                started: None,
                ended: None,
                heartbeat: None,
                heartbeat_line: None,
                heartbeats: 0,
                exit_code: None,
                exit_line: None,
                report: None,
                plan: None,
                first: self.front + self.lines.len() as u64,
                records: 0,
                skipped,
            });
            while self.list.len() > JOBS_KEPT {
                if let Some(old) = self.list.pop_back() {
                    self.report_bytes -= old.report.map_or(0, |report| report.len())
                        + old.plan.map_or(0, |plan| plan.len());
                }
            }
            if let Some(line) = heartbeat {
                self.heartbeat(line, now);
            }
            if let Some((report, plan)) = report {
                self.keep_report(report, plan);
            }
        }
        self.bytes += line.len() + RECORD_OVERHEAD;
        self.lines.push_back(line.into_boxed_str());
        if let Some(entry) = self.current() {
            entry.records += 1;
        }
        while self.lines.len() > EVENTS_KEPT || self.bytes > EVENT_BUFFER_BYTES {
            let Some(old) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= old.len() + RECORD_OVERHEAD;
            self.front += 1;
        }
    }

    /// The engine has ended: with its `exit` message (`code`, and the message's code,
    /// diagnostic and line), or without one. A job that had begun ends; a submit still waiting
    /// hears why its document did not become a job.
    fn end(&mut self, code: Option<i32>, exit: Option<(i32, Option<String>, Box<str>)>, now: u64) {
        let Some(mut flight) = self.flight.take() else {
            return;
        };
        self.wire.clear();
        if let Some(ticket) = flight.ticket.take() {
            let answer = match &exit {
                Some((SPEC_REFUSED, diagnostic, _)) => refusal(
                    ErrorCode::Invalid,
                    diagnostic
                        .as_deref()
                        .unwrap_or("the engine refused the document"),
                    &flight.secrets,
                ),
                Some((code, diagnostic, _)) => refusal(
                    ErrorCode::Internal,
                    &format!(
                        "the engine exited with code {code} before the job began: {}",
                        diagnostic.as_deref().unwrap_or("it did not say why")
                    ),
                    &flight.secrets,
                ),
                None => refusal(
                    ErrorCode::Internal,
                    "the engine ended before the job began",
                    &flight.secrets,
                ),
            };
            self.resolved.push((ticket, answer));
        }
        let ordinal = flight.ordinal;
        if let Some(entry) = self.list.iter_mut().find(|entry| entry.ordinal == ordinal) {
            entry.state = match code {
                Some(0) => JobState::Completed,
                Some(CANCELLED) => JobState::Cancelled,
                _ => JobState::Failed,
            };
            entry.ended = Some(now);
            entry.exit_code = code;
            entry.exit_line = exit.map(|(_, _, line)| line);
        }
    }

    /// How many bytes the buffer of records holds, and how many records.
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> (usize, usize) {
        (self.bytes, self.lines.len())
    }
}

/// One Server-Sent Event named `name` whose data is `line`.
fn event(out: &mut Vec<u8>, name: &str, line: &str) {
    out.extend_from_slice(format!("event: {name}\ndata: ").as_bytes());
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(b"\n\n");
}

/// The answer to a validate: the engine's verdict.
fn validation(refusals: Vec<Refusal>, secrets: &Secrets) -> Answer {
    let verdict = Validation {
        ok: refusals.is_empty(),
        refusals,
    };
    json(200, serde_json::to_vec(&verdict), secrets)
}

/// The longest line the engine's `report` may be, which bounds what a report takes before it
/// is kept: the engine's line limit.
const _: () = assert!(ENGINE_LINE_LIMIT <= REPORT_BYTES);

#[cfg(test)]
mod tests;
