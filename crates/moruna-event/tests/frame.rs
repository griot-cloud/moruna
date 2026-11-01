//! The framed message that carries one event on the host protocol (MH 4.3 version 7).

use std::io::{BufReader, Cursor};

use moruna_event::frame::{FRAME, next_is_frame, read_frame, write_frame};
use moruna_event::{Encoder, Event, Stamp, decode};

fn record(seq: u64) -> Vec<u8> {
    let stamp = Stamp {
        run: Some("ab"),
        plan: None,
        attempt: 1,
        start: "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    };
    Encoder::new()
        .encode(&stamp, seq, 0, seq, &Event::CancelAcknowledged)
        .expect("a record")
        .to_vec()
}

/// A frame is its mark and the record, in one write, and reads back to the record.
#[test]
fn a_frame_is_its_mark_and_its_record() {
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    let one = record(1);
    let two = record(2);
    write_frame(&mut out, &mut scratch, &one).expect("written");
    write_frame(&mut out, &mut scratch, &two).expect("written");
    assert_eq!(out[0], FRAME);
    assert_eq!(&out[1..1 + one.len()], one.as_slice());
    let mut reader = BufReader::new(Cursor::new(out));
    assert_eq!(next_is_frame(&mut reader).unwrap(), Some(true));
    assert_eq!(read_frame(&mut reader).unwrap(), one);
    assert_eq!(next_is_frame(&mut reader).unwrap(), Some(true));
    let read = read_frame(&mut reader).unwrap();
    assert_eq!(decode(&read).unwrap().seq, 2);
    assert_eq!(next_is_frame(&mut reader).unwrap(), None);
}

/// A reader that is not at a frame, a length no record has and a frame cut short are errors, not
/// a wait for bytes that will not come and not a buffer of the peer's choosing.
#[test]
fn a_frame_that_is_not_one_is_refused() {
    let mut reader = BufReader::new(Cursor::new(b"{\"type\":\"exit\"}\n".to_vec()));
    assert_eq!(next_is_frame(&mut reader).unwrap(), Some(false));
    assert!(read_frame(&mut reader).is_err());
    for len in [0u32, 3, 31, u32::MAX, (1 << 20) + (64 << 10) + 1] {
        let mut bytes = vec![FRAME];
        bytes.extend_from_slice(&len.to_le_bytes());
        let mut reader = BufReader::new(Cursor::new(bytes));
        let error = read_frame(&mut reader).expect_err("a length no record has");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{len}");
    }
    let mut cut = vec![FRAME];
    cut.extend_from_slice(&100u32.to_le_bytes());
    cut.extend_from_slice(&[1u8; 10]);
    let mut reader = BufReader::new(Cursor::new(cut));
    assert!(read_frame(&mut reader).is_err());
    let mut reader = BufReader::new(Cursor::new(Vec::new()));
    assert!(read_frame(&mut reader).is_err());
}
