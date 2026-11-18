//! The engine's bytes read as the host protocol's messages.

use super::*;
use crate::fake::Engine;
use alloc::{vec, vec::Vec};
use moruna_event::Event;

fn all(wire: &mut Wire) -> Vec<Message> {
    let mut messages = Vec::new();
    while let Some(message) = wire.next() {
        messages.push(message);
    }
    messages
}

/// What the tests' engine sends for one event: the frame, mark included.
fn frame() -> Vec<u8> {
    let engine = Engine::new();
    engine.attempt_started();
    let state = engine.0.borrow();
    state.outbox.iter().copied().collect()
}

#[test]
fn lines_and_frames_come_apart_however_the_bytes_arrive() {
    let frame = frame();
    let mut stream = b"{\"type\":\"hello\"}\n".to_vec();
    stream.extend_from_slice(&frame);
    stream.extend_from_slice(b"\r\n  {\"type\":\"exit\",\"code\":0}\r\n");
    stream.extend_from_slice(&frame);
    let expected = vec![
        Message::Line(b"{\"type\":\"hello\"}".to_vec()),
        Message::Record(frame[1..].to_vec()),
        Message::Line(b"{\"type\":\"exit\",\"code\":0}".to_vec()),
        Message::Record(frame[1..].to_vec()),
    ];
    let mut whole = Wire::default();
    whole.push(&stream);
    assert_eq!(all(&mut whole), expected);
    let mut wire = Wire::default();
    let mut messages = Vec::new();
    for byte in &stream {
        wire.push(&[*byte]);
        messages.extend(all(&mut wire));
        wire.settle();
    }
    assert_eq!(messages, expected);
    // The record decodes as the event it is.
    let Message::Record(record) = &expected[1] else {
        unreachable!()
    };
    let event = moruna_event::decode(record).unwrap();
    assert!(matches!(event.body, Event::AttemptStarted { .. }));
}

#[test]
fn a_line_past_the_limit_is_passed_over_and_the_next_is_read() {
    let mut wire = Wire::default();
    wire.push(&vec![b'x'; ENGINE_LINE_LIMIT + 1]);
    assert_eq!(wire.next(), None);
    wire.push(&vec![b'x'; 1000]);
    assert_eq!(wire.next(), None);
    wire.settle();
    wire.push(b"tail\n{\"type\":\"exit\"}\n");
    assert_eq!(
        all(&mut wire),
        [Message::Line(b"{\"type\":\"exit\"}".to_vec())]
    );
    assert_eq!(wire.take_skipped(), 1);
    assert_eq!(wire.take_skipped(), 0);
    // A long line that arrives whole is passed over and counted too.
    let mut long = vec![b'z'; ENGINE_LINE_LIMIT + 1];
    long.extend_from_slice(b"\n{}\n");
    wire.push(&long);
    assert_eq!(all(&mut wire), [Message::Line(b"{}".to_vec())]);
    assert_eq!(wire.take_skipped(), 1);
    // A line at the limit is read.
    let mut line = vec![b'y'; ENGINE_LINE_LIMIT];
    line.push(b'\n');
    wire.push(&line);
    assert!(matches!(wire.next(), Some(Message::Line(line)) if line.len() == ENGINE_LINE_LIMIT));
}

#[test]
fn a_frame_no_record_has_garbles_the_stream() {
    for length in [0u32, 31, (MAX_RECORD + 1) as u32, u32::MAX] {
        let mut wire = Wire::default();
        let mut bytes = vec![FRAME];
        bytes.extend_from_slice(&length.to_le_bytes());
        wire.push(&bytes[..3]);
        assert_eq!(wire.next(), None);
        wire.push(&bytes[3..]);
        assert_eq!(wire.next(), Some(Message::Garbled), "{length}");
        wire.push(b"{\"type\":\"exit\"}\n");
        assert_eq!(wire.next(), None, "nothing after it is read");
        wire.clear();
        wire.push(b"{}\n");
        assert_eq!(wire.next(), Some(Message::Line(b"{}".to_vec())));
    }
}

#[test]
fn a_node_with_no_engine_runs_no_jobs() {
    let mut none = NoEngine;
    assert_eq!(none.check(b"{}", &[]), Err(HostError::Unavailable));
    assert_eq!(none.checked(), None);
    assert_eq!(
        none.start(b"{}\n", &Verdict::default()),
        Err(HostError::Unavailable)
    );
    assert_eq!(none.send(b"{}\n"), Err(HostError::Unavailable));
    assert_eq!(none.receive(&mut [0; 8]), Err(HostError::WouldBlock));
}

/// The `check` message carries a version 1 document as its JSON and a version 2 document's
/// bytes as their base64, with the secrets' names and never their values.
#[test]
fn a_check_is_one_line_of_the_document_and_its_secrets_names() {
    let v1 = check_line(br#"{"moruna_spec":1}"#, &["s3.key".into(), "other".into()]);
    assert_eq!(
        v1,
        b"{\"type\":\"check\",\"spec\":{\"moruna_spec\":1},\"secrets\":[\"s3.key\",\"other\"]}\n"
    );
    let line: serde_json::Value = serde_json::from_slice(&v1).unwrap();
    assert_eq!(line["spec"]["moruna_spec"], 1);
    for (bytes, base64) in [
        (&b"MJB2"[..], "TUpCMg=="),
        (b"MJB2x", "TUpCMng="),
        (b"MJB2xy", "TUpCMnh5"),
        (b"\x00\xff\x10", "AP8Q"),
    ] {
        let line = check_line(bytes, &[]);
        let expected =
            alloc::format!("{{\"type\":\"check\",\"spec\":\"{base64}\",\"secrets\":[]}}\n");
        assert_eq!(String::from_utf8(line).unwrap(), expected);
        assert_eq!(
            crate::token::base64(base64.as_bytes()).unwrap(),
            bytes,
            "{base64}"
        );
    }
    assert_eq!(crate::token::to_base64(b""), "");
}

/// The `checked` line's refusals, budget and needs are the verdict; anything else is the
/// check's failure.
#[test]
fn the_checked_line_is_the_verdict() {
    let sound = verdict(
        br#"{"type":"checked","ok":true,"refusals":[],"memory_bytes":1073741824,"hosts":["s3.eu-west-1.amazonaws.com"],"volumes":[{"serial":"vol-in","write":false},{"serial":"vol-out","write":true},{"serial":"vol-s","write":true,"staging":true}],"functions":["taxi.clean"]}
"#,
    )
    .unwrap();
    assert_eq!(
        sound,
        Verdict {
            refusals: Vec::new(),
            memory_bytes: Some(1 << 30),
            needs: Needs {
                hosts: vec!["s3.eu-west-1.amazonaws.com".into()],
                volumes: vec![
                    Volume {
                        serial: "vol-in".into(),
                        write: false,
                        staging: false
                    },
                    Volume {
                        serial: "vol-out".into(),
                        write: true,
                        staging: false
                    },
                    Volume {
                        serial: "vol-s".into(),
                        write: true,
                        staging: true
                    }
                ],
                functions: vec!["taxi.clean".into()],
            },
        }
    );
    let refused = verdict(
        br#"{"type":"checked","ok":false,"refusals":[{"field":"sink.kind","message":"no such kind"}],"memory_bytes":null,"hosts":[],"volumes":[],"functions":[]}"#,
    )
    .unwrap();
    assert_eq!(refused.refusals.len(), 1);
    assert_eq!(refused.refusals[0].field, "sink.kind");
    assert_eq!(refused.memory_bytes, None);
    // The members a check of an unreadable document leaves empty may be absent.
    assert_eq!(
        verdict(br#"{"type":"checked","ok":true}"#).unwrap(),
        Verdict::default()
    );
    for line in [
        &b"not json"[..],
        br#"{"type":"hello","ok":true}"#,
        br#"{"type":"checked","ok":false,"refusals":[]}"#,
        br#"{"type":"checked"}"#,
    ] {
        assert!(
            matches!(verdict(line), Err(HostError::Failed(_))),
            "{}",
            String::from_utf8_lossy(line)
        );
    }
}
