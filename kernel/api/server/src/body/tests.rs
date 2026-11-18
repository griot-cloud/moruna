//! A body written whole and a piece at a time, raw and escaped as `serde_json` escapes.

use super::*;
use alloc::{format, string::ToString, vec};

/// `body` written a piece at a time, `limit` bytes of output at most each time (plus an
/// escape's), each piece taken away as TLS would take it.
fn in_pieces(mut body: Body, limit: usize) -> Vec<u8> {
    let mut whole = Vec::new();
    let mut fills = 0;
    loop {
        let mut out = Vec::new();
        let done = body.fill(&mut out, limit);
        assert!(out.len() < limit + 6, "{} for {limit}", out.len());
        whole.extend_from_slice(&out);
        fills += 1;
        assert!(fills < 1_000_000);
        if done {
            return whole;
        }
    }
}

#[test]
fn escaped_text_is_what_serde_json_writes_inside_a_string() {
    let mut text = String::new();
    for byte in 0u8..0x80 {
        text.push(char::from(byte));
    }
    text.push_str("é漢🦀\u{7f}\u{2028}");
    let mut body = Body::default();
    let at = body.add(Text::Owned(text.clone()));
    body.write(at, Form::Escaped);
    let written = serde_json::to_string(&text).unwrap();
    let inside = &written[1..written.len() - 1];
    assert_eq!(body.bytes(), inside.as_bytes());
    assert_eq!(body.len(), inside.len());
    for limit in [1, 2, 5, 6, 7, 64] {
        assert_eq!(in_pieces(body.clone(), limit), inside.as_bytes(), "{limit}");
    }
}

#[test]
fn a_text_written_twice_is_held_once() {
    let shared: Arc<str> = Arc::from(r#"{"report":"a \"quoted\" line"}"#);
    let mut body = Body::default();
    body.push(Text::Static("{\"text\":\""));
    let at = body.add(Text::Shared(shared.clone()));
    body.write(at, Form::Escaped);
    body.push(Text::Static("\",\"json\":"));
    body.write(at, Form::Raw);
    body.push(Text::Static("}"));
    let whole = body.bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&whole).unwrap();
    assert_eq!(parsed["text"], shared.to_string());
    assert_eq!(parsed["json"]["report"], "a \"quoted\" line");
    assert_eq!(body.len(), whole.len());
    // The report is shared, not copied: the body and the job hold the one text.
    assert_eq!(Arc::strong_count(&shared), 2);
    for limit in [1, 3, 16, 1024] {
        assert_eq!(in_pieces(body.clone(), limit), whole, "{limit}");
    }
}

#[test]
fn a_body_of_one_text_gives_it_back_and_any_other_is_written_into_one() {
    let shared: Arc<str> = Arc::from("{}");
    assert_eq!(
        Body::of(Text::Shared(shared.clone())).into_text(),
        Text::Shared(shared)
    );
    let mut two = Body::of(Text::Static("{\"a\":"));
    two.push(Text::Owned("1}".to_string()));
    assert_eq!(
        two.clone().into_text(),
        Text::Owned("{\"a\":1}".to_string())
    );
    // Partly written, it still gives the whole.
    two.fill(&mut Vec::new(), 2);
    assert_eq!(two.into_text().as_str(), "{\"a\":1}");
    assert_eq!(Body::json(Vec::new()), Body::default());
    let mut out = Vec::new();
    assert!(Body::default().fill(&mut out, 1) && out.is_empty());
    assert_eq!(Body::json(b"[1]".to_vec()).bytes(), b"[1]");
    // Bytes that are not UTF-8 are written as their replacement.
    assert_eq!(
        Body::json(vec![b'"', 0xff, b'"']).bytes(),
        "\"\u{fffd}\"".as_bytes()
    );
}

#[test]
fn a_large_text_is_written_in_pieces_of_the_limit() {
    let text = format!("[{}]", "\"x\",".repeat(100_000).trim_end_matches(','));
    let body = Body::json(text.clone().into_bytes());
    let mut writing = body.clone();
    let mut out = Vec::new();
    assert!(!writing.fill(&mut out, 32 * 1024));
    assert_eq!(out.len(), 32 * 1024);
    // A full output takes nothing more.
    assert!(!writing.fill(&mut out, 32 * 1024));
    assert_eq!(out.len(), 32 * 1024);
    assert_eq!(in_pieces(body, 32 * 1024), text.as_bytes());
}
