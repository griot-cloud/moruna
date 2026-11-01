//! PY-T30 to PY-T33 and PY-T35 (SDD 12 PY-I13): an event as a record and back, the layout of the
//! record, the headers, the ids and the bytes of a hostile peer.

use moruna_event::{
    CancelDisposition, Counters, ENGINE, Element, Encoder, Event, EventMsg, FailureCode,
    FailureScope, Kind, LogReach, LogStream, Phase, SUBJECT_BASE, SkipReason, Stamp, StepOutcome,
    TaskOutcome, WaitReason, decode, read_records,
};
use moruna_record::{Decoded, decode as decode_record, encode};

const START: &str = "0123456789abcdef0123456789abcdef";
const RUN: &str = "fedcba9876543210fedcba9876543210";

fn stamp(plan: Option<&'static str>) -> Stamp<'static> {
    Stamp {
        run: Some(RUN),
        plan,
        attempt: 3,
        start: START,
    }
}

fn s(text: &str) -> String {
    text.to_string()
}

/// Every kind of event, each with its optional fields present and absent, and every value of
/// every enum.
fn every_event() -> Vec<Event> {
    let mut events = Vec::new();
    for resumed in [false, true] {
        for plan in [None, Some(s("p0123"))] {
            events.push(Event::AttemptStarted { resumed, plan });
        }
    }
    events.push(Event::StepStarted {
        step: s("s1"),
        plan: s("p0123"),
        group: 7,
    });
    for outcome in [
        StepOutcome::Failed,
        StepOutcome::Cancelled,
        StepOutcome::Stopped,
        StepOutcome::Skipped,
        StepOutcome::Succeeded,
    ] {
        for duration_us in [None, Some(0), Some(u64::MAX)] {
            events.push(Event::StepEnded {
                step: s("s1"),
                plan: s("p0123"),
                outcome,
                duration_us,
            });
        }
    }
    for element in [Element::Source, Element::Kernel, Element::Sink] {
        for step in [None, Some(s("s1"))] {
            events.push(Event::TaskRegistered {
                task: s("orders"),
                step,
                element,
                stage: u16::MAX,
                upstream: vec![s("a"), s("b")],
                members: vec![],
            });
        }
    }
    events.push(Event::TaskRegistered {
        task: s("fused"),
        step: Some(s("s2")),
        element: Element::Kernel,
        stage: 2,
        upstream: vec![],
        members: vec![s("m1"), s("m2")],
    });
    for reason in [
        WaitReason::EnginePreparing,
        WaitReason::AwaitingInput,
        WaitReason::PortCall,
    ] {
        for port in [None, Some(s("model"))] {
            events.push(Event::TaskWaiting {
                task: s("t"),
                step: Some(s("s")),
                reason,
                blocking: vec![s("up")],
                port,
            });
        }
    }
    events.push(Event::TaskReady {
        task: s("t"),
        step: None,
    });
    for phase in [
        Phase::Calibrating,
        Phase::Reading,
        Phase::Applying,
        Phase::Writing,
        Phase::Draining,
        Phase::Committing,
    ] {
        events.push(Event::TaskStarted {
            task: s("t"),
            step: Some(s("s")),
            phase,
        });
        events.push(Event::TaskPhase {
            task: s("t"),
            step: None,
            phase,
        });
    }
    for outcome in [
        TaskOutcome::Succeeded,
        TaskOutcome::Stopped,
        TaskOutcome::Cancelled,
    ] {
        for counters in [
            None,
            Some(Counters::default()),
            Some(Counters {
                morsels: 1,
                rows_in: 2,
                rows_out: 3,
                bytes_in: 4,
                bytes_out: 5,
                skipped: 6,
                errors: u64::MAX,
            }),
        ] {
            for (duration_us, cause) in [(None, None), (Some(12), Some(s("boom")))] {
                events.push(Event::TaskCompleted {
                    task: s("t"),
                    step: Some(s("s")),
                    outcome,
                    duration_us,
                    counters,
                    cause,
                });
            }
        }
    }
    for code in [
        FailureCode::SpecRefused,
        FailureCode::KernelError,
        FailureCode::BudgetExceeded,
        FailureCode::MemoryRefused,
        FailureCode::SourceError,
        FailureCode::SinkError,
        FailureCode::AllocRefused,
        FailureCode::IoError,
        FailureCode::StagingError,
        FailureCode::ConvertError,
        FailureCode::ConfigRefused,
        FailureCode::PlanRefused,
        FailureCode::ResumeRefused,
        FailureCode::Unsupported,
        FailureCode::Cancelled,
        FailureCode::Internal,
    ] {
        events.push(Event::TaskFailed {
            task: Some(s("t")),
            step: Some(s("s")),
            code,
            message: s("a sentence, café €"),
            scope: FailureScope::Task,
            retryable: true,
        });
    }
    events.push(Event::TaskFailed {
        task: None,
        step: None,
        code: FailureCode::IoError,
        message: String::new(),
        scope: FailureScope::Attempt,
        retryable: false,
    });
    for reason in [
        SkipReason::FailedTask,
        SkipReason::AttemptFailed,
        SkipReason::Cancelled,
    ] {
        for blocking in [None, Some(s("other"))] {
            events.push(Event::TaskSkipped {
                task: s("t"),
                step: Some(s("s")),
                reason,
                blocking,
            });
        }
    }
    events.push(Event::CancelAcknowledged);
    for disposition in [
        CancelDisposition::Cancelled,
        CancelDisposition::Completed,
        CancelDisposition::Failed,
    ] {
        events.push(Event::CancelEnded { disposition });
    }
    events.push(Event::Logs { producers: vec![] });
    events.push(Event::Logs {
        producers: vec![
            LogReach {
                task: Some(s("k1")),
                records: 3,
                bytes: 40,
                truncated: 1,
                dropped: 2,
                dropped_bytes: 7,
                drained: true,
            },
            LogReach::default(),
        ],
    });
    for stream in [
        LogStream::Stdout,
        LogStream::Stderr,
        LogStream::Structured,
        LogStream::Platform,
    ] {
        for (task, level) in [(None, None), (Some(s("k1")), Some(s("warning")))] {
            events.push(Event::Log {
                task,
                stream,
                level,
                bytes: 2,
                text: s("hi\nthere"),
                partial: true,
                continued: true,
                truncated: 9,
            });
        }
    }
    events
}

/// PY-T30: every kind encodes to a record and decodes to the same fields.
#[test]
fn py_t30_an_event_is_a_record_and_back() {
    let mut encoder = Encoder::new();
    let events = every_event();
    let kinds: std::collections::HashSet<Kind> = events.iter().map(Event::kind).collect();
    assert_eq!(kinds.len(), Kind::ALL.len(), "every kind is tried");
    for (i, event) in events.iter().enumerate() {
        for plan in [None, Some("p0123")] {
            let seq = i as u64 + 1;
            let ts = 1_700_000_000_000_000_000 + seq as i64;
            let t_us = 1_000 * seq;
            let record = encoder
                .encode(&stamp(plan), seq, ts, t_us, event)
                .expect("a record")
                .to_vec();
            let read = decode(&record).unwrap_or_else(|e| panic!("{event:?}: {e}"));
            assert_eq!(&read.body, event);
            assert_eq!(read.run.as_deref(), Some(RUN));
            assert_eq!(read.plan.as_deref(), plan);
            assert_eq!(read.attempt, 3);
            assert_eq!(read.start, START);
            assert_eq!(read.seq, seq);
            assert_eq!(read.ts_ns, ts);
            assert_eq!(read.t_us, t_us);
        }
    }
}

/// PY-T31: the bytes are a `moruna-record` record.
#[test]
fn py_t31_the_record_layout_is_the_records() {
    let mut encoder = Encoder::new();
    let event = Event::TaskStarted {
        task: s("orders"),
        step: Some(s("s9")),
        phase: Phase::Reading,
    };
    let record = encoder
        .encode(&stamp(Some("p0123")), 42, 99, 7, &event)
        .expect("a record")
        .to_vec();
    let Decoded::Record(read, used) = decode_record(&record) else {
        panic!("not a record");
    };
    assert_eq!(used, record.len());
    assert_eq!(
        u32::from_le_bytes(record[0..4].try_into().unwrap()) as usize,
        record.len() - 4
    );
    assert_eq!(read.seq, 42);
    assert_eq!(read.ts, 99);
    assert_eq!(read.subject, SUBJECT_BASE + Kind::TaskStarted as u32);
    assert_eq!(read.subject, Kind::TaskStarted.subject());
    assert_eq!(read.msg_id, format!("3-{START}-42"));
    assert_eq!(
        read.headers,
        vec![
            ("run", RUN),
            ("attempt", "3"),
            ("plan", "p0123"),
            ("step", "s9"),
            ("task", "orders")
        ]
    );
    assert_eq!(&read.body[4..8], b"MEV1");
    assert_eq!(Kind::of_subject(read.subject), Some(Kind::TaskStarted));
    assert_eq!(Kind::of_subject(0), None);
    assert_eq!(Kind::of_subject(SUBJECT_BASE), None);
    assert_eq!(Kind::of_subject(SUBJECT_BASE + 16), None);

    // The encoder reuses its record buffer: once it has written the largest event, writing
    // smaller ones does not move it.
    let big = Event::Log {
        task: None,
        stream: LogStream::Stdout,
        level: None,
        bytes: 20_000,
        text: "x".repeat(20_000),
        partial: false,
        continued: false,
        truncated: 0,
    };
    let first = encoder
        .encode(&stamp(None), 1, 0, 0, &big)
        .expect("a record")
        .as_ptr();
    for e in every_event() {
        let at = encoder
            .encode(&stamp(None), 2, 0, 0, &e)
            .expect("a record")
            .as_ptr();
        assert_eq!(at, first);
    }
}

/// The headers a record carries are the ones that apply to its event.
#[test]
fn py_t32_headers_say_whose() {
    let mut encoder = Encoder::new();
    for event in every_event() {
        for plan in [None, Some("p0123")] {
            let record = encoder
                .encode(&stamp(plan), 1, 0, 0, &event)
                .expect("a record")
                .to_vec();
            let Decoded::Record(read, _) = decode_record(&record) else {
                panic!("not a record");
            };
            let has = |key: &str| {
                read.headers
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| *v)
            };
            assert_eq!(has("run"), Some(RUN));
            assert_eq!(has("attempt"), Some("3"));
            assert_eq!(has("plan"), plan);
            assert_eq!(has("task"), event.task(), "{event:?}");
            assert_eq!(has("step"), event.step(), "{event:?}");
            let keys: Vec<&str> = read.headers.iter().map(|(k, _)| *k).collect();
            let mut sorted = keys.clone();
            sorted.sort_by_key(|k| {
                ["run", "attempt", "plan", "step", "task"]
                    .iter()
                    .position(|o| o == k)
            });
            assert_eq!(keys, sorted, "the headers keep one order");
        }
    }
    // A task's events carry step and task when the run was planned.
    let task_event = Event::TaskReady {
        task: s("t"),
        step: Some(s("s")),
    };
    let record = encoder
        .encode(&stamp(Some("p")), 1, 0, 0, &task_event)
        .expect("a record")
        .to_vec();
    let Decoded::Record(read, _) = decode_record(&record) else {
        panic!();
    };
    assert!(read.headers.contains(&("step", "s")) && read.headers.contains(&("task", "t")));
    // A run with no id says so by saying nothing.
    let unknown = Stamp {
        run: None,
        ..stamp(None)
    };
    let record = encoder
        .encode(&unknown, 1, 0, 0, &Event::CancelAcknowledged)
        .expect("a record")
        .to_vec();
    assert_eq!(decode(&record).expect("an event").run, None);
}

/// PY-T33: `msg_id` is `<attempt>-<start>-<seq>`, unique among an attempt's events, the same for the same
/// event told twice and different in another attempt; a record that disagrees with itself is
/// refused.
#[test]
fn py_t33_message_ids_are_idempotent() {
    let mut encoder = Encoder::new();
    let event = Event::CancelAcknowledged;
    let mut seen = std::collections::HashSet::new();
    for seq in 1..=500u64 {
        let record = encoder
            .encode(&stamp(None), seq, 0, 0, &event)
            .expect("a record")
            .to_vec();
        let read = decode(&record).expect("an event");
        assert_eq!(read.msg_id, format!("3-{START}-{seq}"));
        assert!(seen.insert(read.msg_id));
    }
    let again = encoder
        .encode(&stamp(None), 7, 0, 0, &event)
        .expect("a record")
        .to_vec();
    let twice = encoder
        .encode(&stamp(None), 7, 0, 0, &event)
        .expect("a record")
        .to_vec();
    assert_eq!(again, twice);
    let resumed = Stamp {
        attempt: 4,
        ..stamp(None)
    };
    let other = encoder
        .encode(&resumed, 7, 0, 0, &event)
        .expect("a record")
        .to_vec();
    assert_ne!(
        decode(&again).unwrap().msg_id,
        decode(&other).unwrap().msg_id
    );

    // A record of output is `<start>/<producer>/<seq>`.
    let log = |task: Option<&str>| Event::Log {
        task: task.map(str::to_string),
        stream: LogStream::Stdout,
        level: None,
        bytes: 1,
        text: s("x"),
        partial: false,
        continued: false,
        truncated: 0,
    };
    let record = encoder
        .encode(&stamp(None), 5, 0, 0, &log(Some("k1")))
        .expect("a record")
        .to_vec();
    assert_eq!(decode(&record).unwrap().msg_id, format!("3-{START}-k1-5"));
    let record = encoder
        .encode(&stamp(None), 6, 0, 0, &log(None))
        .expect("a record")
        .to_vec();
    assert_eq!(
        decode(&record).unwrap().msg_id,
        format!("3-{START}-{ENGINE}-6")
    );

    // The record's own fields must agree: a wrong id, a wrong subject.
    let good = encoder
        .encode(&stamp(None), 9, 5, 5, &event)
        .expect("a record")
        .to_vec();
    let Decoded::Record(parts, _) = decode_record(&good) else {
        panic!();
    };
    let remade = |subject: u32, msg_id: &str, seq: u64| {
        encode(seq, parts.ts, subject, msg_id, &parts.headers, parts.body).unwrap()
    };
    let id = |attempt: u32, start: &str, seq: u64| format!("{attempt}-{start}-{seq}");
    assert!(decode(&remade(parts.subject, &id(3, START, 9), 9)).is_ok());
    assert!(decode(&remade(parts.subject, &id(3, START, 10), 9)).is_err());
    assert!(decode(&remade(parts.subject, &id(4, START, 9), 9)).is_err());
    assert!(decode(&remade(parts.subject, &id(3, RUN, 9), 9)).is_err());
    assert!(decode(&remade(parts.subject, "3-9", 9)).is_err());
    assert!(decode(&remade(parts.subject, &id(3, START, 9), 10)).is_err());
    assert!(decode(&remade(Kind::TaskReady.subject(), &id(3, START, 9), 9)).is_err());
    assert!(decode(&remade(1, &id(3, START, 9), 9)).is_err());
    // A header that is not a number of an attempt.
    let headerless = encode(
        9,
        0,
        parts.subject,
        &format!("3-{START}-9"),
        &[("run", RUN)],
        parts.body,
    )
    .unwrap();
    assert!(decode(&headerless).is_err());
    let bad_attempt = encode(
        9,
        0,
        parts.subject,
        "3-9",
        &[("attempt", "three")],
        parts.body,
    )
    .unwrap();
    assert!(decode(&bad_attempt).is_err());
    // Bytes after the record are not an event.
    let mut long = good.clone();
    long.push(0);
    assert!(decode(&long).is_err());
    assert!(decode(&good[..good.len() - 1]).is_err());
    assert!(decode(&[]).is_err());
}

/// Two processes of one attempt (a host that ran the attempt twice) tell disjoint ids for every
/// event, whatever the numbers, because the process's own id is in `msg_id`; so does a record of
/// output of the same producer and number.
#[test]
fn two_processes_of_one_attempt_tell_disjoint_ids() {
    let other_start = "ffffffffffffffffffffffffffffffff";
    let mut encoder = Encoder::new();
    let mut ids = std::collections::HashSet::new();
    let log = |task: Option<&str>| Event::Log {
        task: task.map(str::to_string),
        stream: LogStream::Stdout,
        level: None,
        bytes: 1,
        text: s("x"),
        partial: false,
        continued: false,
        truncated: 0,
    };
    for start in [START, other_start] {
        let stamp = Stamp {
            start,
            ..stamp(None)
        };
        for seq in 1..=200u64 {
            for event in [Event::CancelAcknowledged, log(Some("k1")), log(None)] {
                let record = encoder
                    .encode(&stamp, seq, 0, 0, &event)
                    .expect("a record")
                    .to_vec();
                let read = decode(&record).expect("an event");
                assert!(
                    ids.insert(read.msg_id.clone()),
                    "{} is told twice",
                    read.msg_id
                );
            }
        }
    }
    assert_eq!(ids.len(), 2 * 200 * 3);
}

/// Records back to back are read in order, and the first thing that is not one stops the read,
/// naming where.
#[test]
fn records_back_to_back() {
    let mut encoder = Encoder::new();
    let mut bytes = Vec::new();
    for seq in 1..=5u64 {
        bytes.extend_from_slice(
            encoder
                .encode(&stamp(None), seq, 0, seq, &Event::CancelAcknowledged)
                .expect("a record"),
        );
    }
    let events = read_records(&bytes).expect("five events");
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert_eq!(read_records(&[]).unwrap(), Vec::<EventMsg>::new());
    let cut = read_records(&bytes[..bytes.len() - 3]).unwrap_err();
    assert!(cut.contains("cut short"), "{cut}");
    let mut broken = bytes.clone();
    let at = bytes.len() / 5 * 2 + 12;
    broken[at] ^= 0xff;
    let error = read_records(&broken).unwrap_err();
    assert!(
        error.contains("not a record") || error.contains("byte"),
        "{error}"
    );
}

/// The JSON rendering keeps the names of version 6, with `v` 7.
#[test]
fn the_json_rendering_has_the_old_names() {
    let mut encoder = Encoder::new();
    let record = encoder
        .encode(
            &stamp(Some("p")),
            7,
            0,
            1500,
            &Event::TaskStarted {
                task: s("orders"),
                step: Some(s("s0")),
                phase: Phase::Reading,
            },
        )
        .expect("a record")
        .to_vec();
    assert_eq!(
        decode(&record).unwrap().json_line(),
        format!(
            "{{\"type\":\"event\",\"v\":7,\"attempt\":3,\"start\":\"{START}\",\"seq\":7,\
             \"t_us\":1500,\"kind\":\"task_started\",\"task\":\"orders\",\"step\":\"s0\",\
             \"phase\":\"reading\"}}\n"
        )
    );
    let record = encoder
        .encode(
            &stamp(None),
            4,
            0,
            77,
            &Event::Log {
                task: Some(s("k1")),
                stream: LogStream::Stderr,
                level: None,
                bytes: 2,
                text: s("hi"),
                partial: false,
                continued: true,
                truncated: 0,
            },
        )
        .expect("a record")
        .to_vec();
    assert_eq!(
        decode(&record).unwrap().json_line(),
        format!(
            "{{\"type\":\"event\",\"v\":7,\"attempt\":3,\"start\":\"{START}\",\"seq\":4,\
             \"t_us\":77,\"kind\":\"log\",\"id\":\"{START}/k1/4\",\"task\":\"k1\",\
             \"stream\":\"stderr\",\"level\":null,\"bytes\":2,\"text\":\"hi\",\
             \"partial\":false,\"continued\":true,\"truncated\":0}}\n"
        )
    );
}

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// PY-T35: corrupted and invented bytes are refused, never a panic.
#[test]
fn py_t35_hostile_record_bytes() {
    let mut encoder = Encoder::new();
    let valid: Vec<Vec<u8>> = every_event()
        .iter()
        .enumerate()
        .map(|(i, e)| {
            encoder
                .encode(&stamp(Some("p0123")), i as u64 + 1, 5, 5, e)
                .expect("a record")
                .to_vec()
        })
        .collect();
    let mut rng = SplitMix(0x4D45_5654_4B41_3842);
    let mut refused_under_the_crc = 0;
    let mut reached_the_verifier = 0;
    for round in 0..20_000 {
        let original = &valid[round % valid.len()];
        let mut bytes = original.clone();
        let mut under_crc = false;
        match rng.below(8) {
            0 => {
                let at = rng.below(bytes.len());
                bytes[at] ^= 1 << rng.below(8);
                under_crc = at >= 8;
            }
            1 => bytes.truncate(rng.below(bytes.len())),
            2 => {
                let at = rng.below(bytes.len());
                let was = bytes[at];
                bytes[at] = rng.next() as u8;
                under_crc = at >= 8 && bytes[at] != was;
            }
            3 => {
                let at = rng.below(bytes.len() + 1);
                let extra: Vec<u8> = (0..1 + rng.below(16)).map(|_| rng.next() as u8).collect();
                bytes.splice(at..at, extra);
            }
            4 => {
                let len = (rng.next() as u32).to_le_bytes();
                bytes[0..4].copy_from_slice(&len);
            }
            5 => {
                let n = rng.below(2048);
                bytes = (0..n).map(|_| rng.next() as u8).collect();
            }
            6 => {
                // The body corrupted and the crc made good again, so the verifier is what
                // refuses it (or reads what is still an event).
                let Decoded::Record(parts, _) = decode_record(original) else {
                    panic!();
                };
                let mut body = parts.body.to_vec();
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(body.len());
                    body[at] = rng.next() as u8;
                }
                bytes = encode(
                    parts.seq,
                    parts.ts,
                    parts.subject,
                    parts.msg_id,
                    &parts.headers,
                    &body,
                )
                .unwrap();
                reached_the_verifier += 1;
            }
            _ => {
                // A header or the id replaced with noise, the crc made good again.
                let Decoded::Record(parts, _) = decode_record(original) else {
                    panic!();
                };
                let noise: String = (0..rng.below(24))
                    .map(|_| char::from(b'a' + (rng.next() % 26) as u8))
                    .collect();
                let headers = [("attempt", noise.as_str()), ("run", noise.as_str())];
                bytes = encode(
                    parts.seq,
                    parts.ts,
                    parts.subject,
                    &noise,
                    &headers,
                    parts.body,
                )
                .unwrap();
            }
        }
        let result = decode(&bytes);
        if under_crc {
            assert!(
                result.is_err(),
                "a changed byte under the crc was read as an event"
            );
            refused_under_the_crc += 1;
        }
        // And the record's own reader agrees it never panics on the same bytes.
        let _ = decode_record(&bytes);
        let _ = read_records(&bytes);
    }
    assert!(refused_under_the_crc > 1_000);
    assert!(reached_the_verifier > 1_000);
}

// Exercise the exact private proof reducer without pulling in Arrow/Runtime.
#[path = "../../moruna-runtime/examples/kd3/sink_ack.rs"]
mod kd3_sink_ack;
mod kd3_sink_ack_tests {
    use super::kd3_sink_ack::SinkAck;
    use moruna_event::{Counters, Element, Event, EventMsg, TaskOutcome};
    const RUN: &str = "0123456789abcdef0123456789abcdef";
    fn event(body: Event) -> EventMsg {
        EventMsg {
            run: None,
            plan: None,
            attempt: 1,
            start: "fedcba9876543210fedcba9876543210".into(),
            seq: 90,
            ts_ns: 0,
            t_us: 0,
            msg_id: "test".into(),
            body,
        }
    }
    fn register() -> EventMsg {
        let mut e = event(Event::TaskRegistered {
            task: "sink".into(),
            step: Some("s2b35645301238ecf".into()),
            element: Element::Sink,
            stage: 3,
            upstream: vec!["kernel2".into()],
            members: vec![],
        });
        e.seq = 5;
        e
    }
    fn complete() -> EventMsg {
        event(Event::TaskCompleted {
            task: "sink".into(),
            step: Some("s2b35645301238ecf".into()),
            outcome: TaskOutcome::Succeeded,
            duration_us: Some(1),
            counters: Some(Counters {
                rows_in: 100000,
                rows_out: 100000,
                morsels: 10,
                ..Default::default()
            }),
            cause: None,
        })
    }
    fn observe(completion: EventMsg) -> SinkAck {
        let mut ack = SinkAck::default();
        ack.observe(&register(), Some(RUN), 100000);
        let mut started = event(Event::TaskStarted {
            task: "sink".into(),
            step: Some("s2b35645301238ecf".into()),
            phase: moruna_event::Phase::Writing,
        });
        started.seq = 24;
        ack.observe(&started, Some(RUN), 100000);
        ack.observe(&completion, Some(RUN), 100000);
        ack
    }
    #[test]
    fn actual_sink_success_matches_run_and_attempt() {
        assert!(observe(complete()).verify(Some(RUN)).is_ok());
        assert!(observe(complete()).verify(Some("another-run")).is_err());
        assert!(observe(complete()).verify(Some("")).is_err());
        let mut unbound = SinkAck::default();
        unbound.observe(&register(), None, 100000);
        let mut started = event(Event::TaskStarted {
            task: "sink".into(),
            step: Some("s2b35645301238ecf".into()),
            phase: moruna_event::Phase::Writing,
        });
        started.seq = 24;
        unbound.observe(&started, None, 100000);
        unbound.observe(&complete(), None, 100000);
        assert!(unbound.verify(Some(RUN)).is_err());
    }
    #[test]
    fn missing_wrong_task_or_wrong_identity_refuses() {
        assert!(SinkAck::default().verify(Some(RUN)).is_err());
        let mut e = complete();
        if let Event::TaskCompleted { task, .. } = &mut e.body {
            *task = "kernel2".into();
        }
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut e = complete();
        e.run = Some("another-run".into());
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut e = complete();
        e.attempt = 2;
        assert!(observe(e).verify(Some(RUN)).is_err());
    }
    #[test]
    fn short_error_skipped_or_empty_counters_refuse() {
        for change in 0..5 {
            let mut e = complete();
            if let Event::TaskCompleted {
                counters: Some(c), ..
            } = &mut e.body
            {
                match change {
                    0 => c.rows_in -= 1,
                    1 => c.rows_out -= 1,
                    2 => c.errors = 1,
                    3 => c.skipped = 1,
                    _ => c.morsels = 0,
                }
            }
            assert!(observe(e).verify(Some(RUN)).is_err());
        }
    }
    #[test]
    fn wrong_outcome_missing_counters_and_conflicting_duplicates_refuse() {
        let mut e = complete();
        if let Event::TaskCompleted { outcome, .. } = &mut e.body {
            *outcome = TaskOutcome::Stopped;
        }
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut e = complete();
        if let Event::TaskCompleted { counters, .. } = &mut e.body {
            *counters = None;
        }
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut ack = observe(complete());
        ack.observe(&complete(), Some(RUN), 100000);
        assert!(ack.verify(Some(RUN)).is_err());
        let mut e = complete();
        if let Event::TaskCompleted { outcome, .. } = &mut e.body {
            *outcome = TaskOutcome::Cancelled;
        }
        ack.observe(&e, Some(RUN), 100000);
        assert!(ack.verify(Some(RUN)).is_err());
    }

    #[test]
    fn explicit_sink_failure_cannot_be_overwritten_by_later_success() {
        let mut ack = SinkAck::default();
        ack.observe(&register(), Some(RUN), 100000);
        ack.observe(
            &event(Event::TaskFailed {
                task: Some("sink".into()),
                step: Some("s2b35645301238ecf".into()),
                code: moruna_event::FailureCode::KernelError,
                message: "actual sink failure".into(),
                scope: moruna_event::FailureScope::Task,
                retryable: false,
            }),
            Some(RUN),
            100000,
        );
        ack.observe(&complete(), Some(RUN), 100000);
        assert!(ack.verify(Some(RUN)).is_err());
    }

    #[test]
    fn sink_producer_step_role_and_order_are_not_interchangeable() {
        let mut e = complete();
        e.start = "different-producer".into();
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut e = complete();
        if let Event::TaskCompleted { step, .. } = &mut e.body {
            *step = Some("other-step".into());
        }
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut e = complete();
        e.seq = 20;
        assert!(observe(e).verify(Some(RUN)).is_err());
        let mut ack = SinkAck::default();
        ack.observe(&complete(), Some(RUN), 100000);
        ack.observe(&register(), Some(RUN), 100000);
        assert!(ack.verify(Some(RUN)).is_err());
        let mut e = register();
        if let Event::TaskRegistered { element, .. } = &mut e.body {
            *element = Element::Kernel;
        }
        let mut ack = SinkAck::default();
        ack.observe(&e, Some(RUN), 100000);
        ack.observe(&complete(), Some(RUN), 100000);
        assert!(ack.verify(Some(RUN)).is_err());
    }
}
