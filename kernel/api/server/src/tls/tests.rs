//! The pieces of the TLS layer the end-to-end tests cannot reach.

use super::*;

#[test]
fn the_wall_clock_is_what_the_server_last_set() {
    let wall = WallClock::default();
    wall.set(1_794_931_200);
    assert_eq!(wall.current_time().unwrap().as_secs(), 1_794_931_200);
}

#[test]
fn append_grows_to_the_size_asked_once_and_fails_cleanly() {
    let mut out = b"ab".to_vec();
    append(&mut out, 1, |tail| {
        if tail.len() < 3 {
            Err(Some(3))
        } else {
            tail[..3].copy_from_slice(b"cde");
            Ok(3)
        }
    })
    .unwrap();
    assert_eq!(out, b"abcde");
    assert_eq!(append(&mut out, 1, |_| Err(None)), Err(TlsError::Protocol));
    assert_eq!(
        append(&mut out, 4, |_| Err(Some(2))),
        Err(TlsError::Protocol),
        "a size no larger than offered"
    );
    assert_eq!(out, b"abcde", "nothing is left behind by a failure");
}
