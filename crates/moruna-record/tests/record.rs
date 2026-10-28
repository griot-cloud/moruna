//! The record's own properties (design 21 k, RC-T1 to RC-T5). Its agreement with the frozen
//! reference is `reference.rs`.

use moruna_record::{
    Corruption, Decoded, Error, FIXED, MAX_BODY, MAX_RECORD, check_fits, crc32c, decode, encode,
    encode_into, encoded_len,
};

#[test]
fn rc_t1_crc32c_matches_the_published_values() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c(b""), 0);
    assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    let ascending: Vec<u8> = (0..32).collect();
    assert_eq!(crc32c(&ascending), 0x46DD_794E);
}

#[test]
fn rc_t2_the_layout_is_byte_for_byte_as_documented() {
    let b = encode(
        0x0102_0304_0506_0708,
        -2,
        0xAABB_CCDD,
        "ab",
        &[("k", "vv")],
        &[9, 9, 9],
    )
    .expect("fits");
    assert_eq!(b.len(), 36 + 2 + (2 + 1 + 2 + 2) + 3);
    assert_eq!(b[0..4], 44u32.to_le_bytes(), "len counts what follows it");
    assert_eq!(
        b[4..8],
        crc32c(&b[8..]).to_le_bytes(),
        "crc covers [8, 4 + len)"
    );
    assert_eq!(b[8..16], [8, 7, 6, 5, 4, 3, 2, 1], "seq");
    assert_eq!(
        b[16..24],
        [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        "ts"
    );
    assert_eq!(b[24..28], [0xDD, 0xCC, 0xBB, 0xAA], "subject");
    assert_eq!(b[28..30], [2, 0], "msg_id_len");
    assert_eq!(b[30..32], [1, 0], "hdr_count");
    assert_eq!(b[32..36], [3, 0, 0, 0], "body_len");
    assert_eq!(&b[36..], b"ab\x01\x00k\x02\x00vv\x09\x09\x09");
}

#[test]
fn rc_t3_a_record_round_trips() {
    let b = encode(
        42,
        1_700_000_000_000_000_000,
        7,
        "id-1",
        &[("k", "v"), ("x", "")],
        b"hello",
    )
    .expect("fits");
    let Decoded::Record(r, n) = decode(&b) else {
        panic!("{:?}", decode(&b));
    };
    assert_eq!(n, b.len());
    assert_eq!(
        (r.seq, r.ts, r.subject, r.msg_id),
        (42, 1_700_000_000_000_000_000, 7, "id-1")
    );
    assert_eq!(r.headers, vec![("k", "v"), ("x", "")]);
    assert_eq!(r.body, b"hello");

    let empty = encode(1, 0, 0, "", &[], b"").expect("fits");
    assert_eq!(empty.len(), FIXED);
    assert!(matches!(decode(&empty), Decoded::Record(_, 36)));

    let mut two = encode(1, 0, 0, "", &[], b"a").expect("fits");
    two.extend(encode(2, 0, 0, "", &[], b"bb").expect("fits"));
    let Decoded::Record(first, n1) = decode(&two) else {
        panic!()
    };
    let Decoded::Record(second, _) = decode(&two[n1..]) else {
        panic!()
    };
    assert_eq!((first.seq, second.seq, second.body), (1, 2, &b"bb"[..]));
}

#[test]
fn rc_t4_what_does_not_fit_is_refused_and_writes_nothing() {
    let id = "i".repeat(65_536);
    assert_eq!(
        encoded_len(&id, &[], b""),
        Err(Error::MsgIdTooLong { len: 65_536 })
    );
    let body = vec![0u8; MAX_BODY + 1];
    assert_eq!(
        check_fits("", &[], &body),
        Err(Error::BodyTooLarge { len: MAX_BODY + 1 })
    );
    let many = vec![("", ""); 65_536];
    assert_eq!(
        check_fits("", &many, b""),
        Err(Error::TooManyHeaders { count: 65_536 })
    );
    let long = "v".repeat(65_536);
    assert_eq!(
        check_fits("", &[("a", "b"), ("k", &long)], b""),
        Err(Error::HeaderTooLong { index: 1 })
    );
    assert_eq!(
        check_fits("", &[(&long, "")], b""),
        Err(Error::HeaderTooLong { index: 0 })
    );
    let max = vec![0u8; MAX_BODY];
    let id = "i".repeat(MAX_RECORD - MAX_BODY - FIXED);
    assert_eq!(encoded_len(&id, &[], &max), Ok(MAX_RECORD));
    let id = "i".repeat(MAX_RECORD - MAX_BODY - FIXED + 1);
    assert_eq!(
        check_fits(&id, &[], &max),
        Err(Error::RecordTooLarge {
            size: MAX_RECORD + 1
        })
    );
    let big = "x".repeat(65_535);
    assert!(matches!(
        check_fits(&big, &[(&big, &big)], &max),
        Err(Error::RecordTooLarge { .. })
    ));

    // A buffer that is too small is refused and left as it was.
    let mut buf = [0x55u8; 40];
    assert_eq!(
        encode_into(&mut buf, 1, 2, 3, "abcdef", &[], b""),
        Err(Error::BufferTooSmall { need: 42, have: 40 })
    );
    assert_eq!(buf, [0x55u8; 40]);
    assert_eq!(
        encode(1, 2, 3, &id, &[], &max),
        Err(Error::RecordTooLarge {
            size: MAX_RECORD + 1
        })
    );
    // An exact one is taken.
    let mut exact = [0u8; 42];
    assert_eq!(encode_into(&mut exact, 1, 2, 3, "abcdef", &[], b""), Ok(42));
    assert!(matches!(decode(&exact), Decoded::Record(..)));
}

#[test]
fn rc_t5_a_subject_is_an_opaque_number() {
    for subject in [0, 1, 2, 3, 0x8000_0000, u32::MAX] {
        let b = encode(1, 1, subject, "", &[], b"x").expect("fits");
        let Decoded::Record(r, _) = decode(&b) else {
            panic!("subject {subject}");
        };
        assert_eq!(r.subject, subject);
    }
}

#[test]
fn rc_t4_errors_say_what_is_wrong_and_are_std_errors() {
    fn is_error<E: std::error::Error>(_: &E) {}
    let all = [
        Error::BodyTooLarge { len: 1 },
        Error::MsgIdTooLong { len: 2 },
        Error::TooManyHeaders { count: 3 },
        Error::HeaderTooLong { index: 4 },
        Error::RecordTooLarge { size: 5 },
        Error::BufferTooSmall { need: 6, have: 7 },
        Error::Misaligned,
        Error::RegionTooSmall { len: 8, need: 9 },
        Error::BadMagic,
        Error::BadHeader("why"),
        Error::ConsumerTaken,
        Error::CorruptSlot { pos: 10 },
        Error::StaleExtentUnknown { pos: 11 },
    ];
    for e in all {
        is_error(&e);
        assert!(!e.to_string().is_empty(), "{e:?}");
    }
    assert!(Error::BadHeader("why").to_string().contains("why"));
    let c = [
        Corruption::Length(1),
        Corruption::Crc {
            stored: 1,
            actual: 2,
        },
        Corruption::MsgIdPastEnd,
        Corruption::MsgIdNotUtf8,
        Corruption::HeaderPastEnd,
        Corruption::HeaderKeyPastEnd,
        Corruption::HeaderValuePastEnd,
        Corruption::HeaderNotUtf8,
        Corruption::Body {
            body_len: 1,
            at: 2,
            total: 3,
        },
    ];
    for c in c {
        is_error(&c);
        assert!(!c.to_string().is_empty(), "{c:?}");
    }
}
