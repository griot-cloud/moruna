//! The DER writer's pieces against values worked out by hand.

use super::*;
use alloc::vec;

#[test]
fn lengths_take_the_short_and_the_long_form() {
    assert_eq!(tlv(OCTET_STRING, &[]), [0x04, 0x00]);
    assert_eq!(tlv(OCTET_STRING, &[7; 0x7f])[..2], [0x04, 0x7f]);
    assert_eq!(tlv(OCTET_STRING, &[7; 0x80])[..3], [0x04, 0x81, 0x80]);
    assert_eq!(
        tlv(OCTET_STRING, &[7; 0x1234])[..4],
        [0x04, 0x82, 0x12, 0x34]
    );
    assert_eq!(tlv(OCTET_STRING, &[7; 0x1234]).len(), 4 + 0x1234);
}

#[test]
fn integers_are_minimal_and_positive() {
    assert_eq!(unsigned(&[2]), [0x02, 0x01, 0x02]);
    assert_eq!(unsigned(&[0, 0, 0x7f]), [0x02, 0x01, 0x7f]);
    assert_eq!(unsigned(&[0x80]), [0x02, 0x02, 0x00, 0x80]);
    assert_eq!(unsigned(&[0, 0x80, 1]), [0x02, 0x03, 0x00, 0x80, 0x01]);
    assert_eq!(unsigned(&[0, 0]), [0x02, 0x01, 0x00]);
    assert_eq!(unsigned(&[]), [0x02, 0x01, 0x00]);
}

#[test]
fn the_calendar_matches_known_dates() {
    assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
    // The design's example clock.
    assert_eq!(civil(1_794_931_200), (2026, 11, 17, 16, 0, 0));
    assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0));
    assert_eq!(civil(4_107_542_399), (2100, 2, 28, 23, 59, 59));
    assert_eq!(civil(4_107_542_400), (2100, 3, 1, 0, 0, 0));
    assert_eq!(civil(FIRST_GENERALIZED), (2050, 1, 1, 0, 0, 0));
    assert_eq!(civil(LAST_SECOND), (9999, 12, 31, 23, 59, 59));
    assert_eq!(civil(1_234_567_890), (2009, 2, 13, 23, 31, 30));
}

#[test]
fn times_switch_to_generalized_in_2050() {
    assert_eq!(time(1_794_931_200), tlv(UTC_TIME, b"261117160000Z"));
    assert_eq!(time(FIRST_GENERALIZED - 1), tlv(UTC_TIME, b"491231235959Z"));
    assert_eq!(
        time(FIRST_GENERALIZED),
        tlv(GENERALIZED_TIME, b"20500101000000Z")
    );
    assert_eq!(time(u64::MAX), tlv(GENERALIZED_TIME, b"99991231235959Z"));
}

#[test]
fn extensions_mark_criticality_only_when_critical() {
    assert_eq!(
        extension(KEY_USAGE, true, &[0x03, 0x02, 0x07, 0x80]),
        vec![
            0x30, 0x0e, 0x06, 0x03, 0x55, 0x1d, 0x0f, 0x01, 0x01, 0xff, 0x04, 0x04, 0x03, 0x02,
            0x07, 0x80
        ]
    );
    assert_eq!(
        extension(EXT_KEY_USAGE, false, &[0x30, 0x00]),
        vec![
            0x30, 0x09, 0x06, 0x03, 0x55, 0x1d, 0x25, 0x04, 0x02, 0x30, 0x00
        ]
    );
}
