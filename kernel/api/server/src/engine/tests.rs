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
    assert_eq!(none.check(b"{}"), Err(HostError::Unavailable));
    assert_eq!(none.checked(), None);
    assert_eq!(none.start(b"{}\n"), Err(HostError::Unavailable));
    assert_eq!(none.send(b"{}\n"), Err(HostError::Unavailable));
    assert_eq!(none.receive(&mut [0; 8]), Err(HostError::WouldBlock));
}
