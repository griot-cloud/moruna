//! The record layout this crate must match, frozen 2026-10-28.
//!
//! The functions `ref_encode`, `ref_decode` and `ref_check_fits` below are a copy of the record
//! format as the event bus Moruna's records reach writes and reads it, with their bodies kept as
//! they were: only the names are Moruna's, and the checksum is a plain table built at run time
//! in this file instead of a crate, so the comparison is between two independent
//! implementations. `moruna-record`'s `encode`, `decode` and `check_fits` are asserted equal to
//! them (RC-T6 to RC-T11). Do not edit the reference to make a test pass.

use moruna_record::{
    Corruption, Decoded, FIXED, MAX_BODY, MAX_RECORD, check_fits, decode, encode, encode_into,
};

// ---- the frozen reference -------------------------------------------------------------------

mod crc32c {
    use std::sync::OnceLock;

    fn table() -> &'static [u32; 256] {
        static T: OnceLock<[u32; 256]> = OnceLock::new();
        T.get_or_init(|| {
            let mut t = [0u32; 256];
            for (i, slot) in t.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        (c >> 1) ^ 0x82F6_3B78
                    } else {
                        c >> 1
                    };
                }
                *slot = c;
            }
            t
        })
    }

    pub fn crc32c(data: &[u8]) -> u32 {
        let t = table();
        let mut c = !0u32;
        for &b in data {
            c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        !c
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRecord<'a> {
    pub seq: u64,
    pub ts: i64,
    pub subject: u32,
    pub msg_id: &'a str,
    pub headers: Vec<(&'a str, &'a str)>,
    pub body: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum RefDecoded<'a> {
    Record(RefRecord<'a>, usize),
    Incomplete,
    Corrupt(String),
}

/// Append one record to `out`; its encoded length.
pub fn ref_encode(
    out: &mut Vec<u8>,
    seq: u64,
    ts: i64,
    subject: u32,
    msg_id: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&ts.to_le_bytes());
    out.extend_from_slice(&subject.to_le_bytes());
    out.extend_from_slice(&(msg_id.len() as u16).to_le_bytes());
    out.extend_from_slice(&(headers.len() as u16).to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(msg_id.as_bytes());
    for (k, v) in headers {
        out.extend_from_slice(&(k.len() as u16).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u16).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out.extend_from_slice(body);
    let len = (out.len() - start - 4) as u32;
    let crc = crc32c::crc32c(&out[start + 8..]);
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
    out.len() - start
}

/// Whether a record with these parts fits the format's fields.
pub fn ref_check_fits(
    msg_id: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(), String> {
    if body.len() > MAX_BODY {
        return Err(format!(
            "a body of {} bytes; a record holds at most {MAX_BODY}",
            body.len()
        ));
    }
    if msg_id.len() > u16::MAX as usize {
        return Err(format!("a msg_id of {} bytes", msg_id.len()));
    }
    if headers.len() > u16::MAX as usize {
        return Err(format!("{} headers", headers.len()));
    }
    for (k, v) in headers {
        if k.len() > u16::MAX as usize || v.len() > u16::MAX as usize {
            return Err(format!("header {k:?} is longer than 65535 bytes"));
        }
    }
    let size: usize = FIXED
        + msg_id.len()
        + headers
            .iter()
            .map(|(k, v)| 4 + k.len() + v.len())
            .sum::<usize>()
        + body.len();
    if size > MAX_RECORD {
        return Err(format!("a record of {size} bytes; at most {MAX_RECORD}"));
    }
    Ok(())
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("two bytes"))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("eight bytes"))
}

/// Decode the record at the start of `buf`, checking its crc and its fields.
pub fn ref_decode(buf: &[u8]) -> RefDecoded<'_> {
    if buf.len() < 4 {
        return RefDecoded::Incomplete;
    }
    let len = u32_at(buf, 0) as usize;
    if !(FIXED - 4..=MAX_RECORD).contains(&len) {
        return RefDecoded::Corrupt(format!("a record length of {len}"));
    }
    let total = 4 + len;
    if buf.len() < total {
        return RefDecoded::Incomplete;
    }
    let crc = u32_at(buf, 4);
    let actual = crc32c::crc32c(&buf[8..total]);
    if crc != actual {
        return RefDecoded::Corrupt(format!("crc {crc:08x}, the bytes give {actual:08x}"));
    }
    let seq = u64_at(buf, 8);
    let ts = u64_at(buf, 16) as i64;
    let subject = u32_at(buf, 24);
    let id_len = u16_at(buf, 28) as usize;
    let hdr_count = u16_at(buf, 30) as usize;
    let body_len = u32_at(buf, 32) as usize;
    let mut at = FIXED;
    let field = |at: usize, n: usize| -> Option<&[u8]> {
        if at + n <= total {
            Some(&buf[at..at + n])
        } else {
            None
        }
    };
    let Some(id) = field(at, id_len) else {
        return RefDecoded::Corrupt("msg_id past the record's end".into());
    };
    let Ok(msg_id) = std::str::from_utf8(id) else {
        return RefDecoded::Corrupt("msg_id is not UTF-8".into());
    };
    at += id_len;
    let mut headers = Vec::with_capacity(hdr_count);
    for _ in 0..hdr_count {
        let Some(kl) = field(at, 2) else {
            return RefDecoded::Corrupt("a header past the record's end".into());
        };
        let kl = u16_at(kl, 0) as usize;
        let Some(k) = field(at + 2, kl) else {
            return RefDecoded::Corrupt("a header key past the record's end".into());
        };
        at += 2 + kl;
        let Some(vl) = field(at, 2) else {
            return RefDecoded::Corrupt("a header past the record's end".into());
        };
        let vl = u16_at(vl, 0) as usize;
        let Some(v) = field(at + 2, vl) else {
            return RefDecoded::Corrupt("a header value past the record's end".into());
        };
        at += 2 + vl;
        let (Ok(k), Ok(v)) = (std::str::from_utf8(k), std::str::from_utf8(v)) else {
            return RefDecoded::Corrupt("a header is not UTF-8".into());
        };
        headers.push((k, v));
    }
    if at + body_len != total {
        return RefDecoded::Corrupt(format!(
            "a body of {body_len} bytes at {at} in a record of {total}"
        ));
    }
    RefDecoded::Record(
        RefRecord {
            seq,
            ts,
            subject,
            msg_id,
            headers,
            body: &buf[at..total],
        },
        total,
    )
}

// ---- the comparison -------------------------------------------------------------------------

/// SplitMix64, the generator of every random case here.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// A string of at most `max` bytes, with multi-byte characters among the ASCII.
    fn string(&mut self, max: usize) -> String {
        let target = self.below(max as u64 + 1) as usize;
        let mut s = String::new();
        while s.len() < target {
            let c = match self.below(10) {
                0 => 'é',
                1 => '日',
                2 => '🦀',
                _ => (b'a' + self.below(26) as u8) as char,
            };
            if s.len() + c.len_utf8() > target {
                break;
            }
            s.push(c);
        }
        s
    }

    /// A u64 across its whole range, with its edges over-represented.
    fn wide(&mut self) -> u64 {
        match self.below(8) {
            0 => [0, 1, u64::MAX, u64::MAX - 1, 1 << 63, (1 << 63) - 1][self.below(6) as usize],
            _ => self.next(),
        }
    }
}

struct Sample {
    seq: u64,
    ts: i64,
    subject: u32,
    msg_id: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Sample {
    fn headers(&self) -> Vec<(&str, &str)> {
        self.headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn encode(&self) -> Vec<u8> {
        encode(
            self.seq,
            self.ts,
            self.subject,
            &self.msg_id,
            &self.headers(),
            &self.body,
        )
        .expect("a sample fits")
    }

    fn ref_encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        ref_encode(
            &mut out,
            self.seq,
            self.ts,
            self.subject,
            &self.msg_id,
            &self.headers,
            &self.body,
        );
        out
    }
}

fn sample(rng: &mut Rng, body: Vec<u8>) -> Sample {
    let headers = (0..rng.below(9))
        .map(|_| (rng.string(40), rng.string(40)))
        .collect();
    Sample {
        seq: rng.wide(),
        ts: rng.wide() as i64,
        subject: rng.wide() as u32,
        msg_id: rng.string(64),
        headers,
        body,
    }
}

/// 10,000 seeded records and one with a body of exactly MAX_BODY.
fn samples() -> Vec<Sample> {
    let mut rng = Rng(0x4D6F_7275_6E61_0001);
    let mut out = Vec::with_capacity(10_001);
    for _ in 0..10_000 {
        let n = match rng.below(100) {
            0..=69 => rng.below(257),
            70..=94 => 256 + rng.below(4096 - 256),
            _ => 4096 + rng.below(65_536 - 4096 + 1),
        } as usize;
        let body = rng.bytes(n);
        out.push(sample(&mut rng, body));
    }
    let body = rng.bytes(MAX_BODY);
    out.push(sample(&mut rng, body));
    out
}

fn same(mine: &Decoded<'_>, theirs: &RefDecoded<'_>) -> bool {
    match (mine, theirs) {
        (Decoded::Record(a, an), RefDecoded::Record(b, bn)) => {
            an == bn
                && a.seq == b.seq
                && a.ts == b.ts
                && a.subject == b.subject
                && a.msg_id == b.msg_id
                && a.headers == b.headers
                && a.body == b.body
        }
        (Decoded::Incomplete, RefDecoded::Incomplete) => true,
        (Decoded::Corrupt(a), RefDecoded::Corrupt(b)) => a.to_string() == *b,
        _ => false,
    }
}

#[test]
fn rc_t6_encode_is_byte_identical_to_the_reference() {
    let all = samples();
    assert_eq!(all.len(), 10_001);
    assert_eq!(all[10_000].body.len(), MAX_BODY);
    for (i, s) in all.iter().enumerate() {
        let hs = s.headers();
        let mine = s.encode();
        assert_eq!(mine, s.ref_encode(), "record {i}");
        // `encode_into` writes the same bytes into a caller's buffer, which may be larger.
        let mut buf = vec![0xAB; mine.len() + 5];
        let n =
            encode_into(&mut buf, s.seq, s.ts, s.subject, &s.msg_id, &hs, &s.body).expect("fits");
        assert_eq!(n, mine.len(), "record {i}");
        assert_eq!(&buf[..n], &mine[..], "record {i}");
        assert_eq!(buf[n..], [0xAB; 5], "record {i}: the tail is untouched");
        assert_eq!(
            moruna_record::encoded_len(&s.msg_id, &hs, &s.body),
            Ok(mine.len())
        );
    }
}

#[test]
fn rc_t7_decode_agrees_with_the_reference_and_with_the_input() {
    let all = samples();
    for (i, s) in all.iter().enumerate() {
        let enc = s.encode();
        let mine = decode(&enc);
        assert!(same(&mine, &ref_decode(&enc)), "record {i}");
        let Decoded::Record(r, n) = mine else {
            panic!("record {i} did not decode");
        };
        assert_eq!(n, enc.len());
        assert_eq!((r.seq, r.ts, r.subject), (s.seq, s.ts, s.subject));
        assert_eq!(r.msg_id, s.msg_id);
        assert_eq!(r.headers, s.headers());
        assert_eq!(r.body, &s.body[..]);
        if i % 10 == 0 {
            // Bytes after the record are not its business.
            let mut more = enc.clone();
            more.extend_from_slice(b"trailing bytes");
            assert!(
                same(&decode(&more), &ref_decode(&more)),
                "record {i} + tail"
            );
            assert!(matches!(decode(&more), Decoded::Record(_, k) if k == enc.len()));
        }
    }
}

/// The records the corruption and truncation tests walk byte by byte: 200 small ones.
fn small(all: &[Sample]) -> Vec<Vec<u8>> {
    all.iter()
        .map(Sample::encode)
        .filter(|e| e.len() <= 400)
        .take(200)
        .collect()
}

#[test]
fn rc_t8_every_single_byte_corruption_is_corrupt_in_both() {
    let all = samples();
    let records = small(&all);
    assert_eq!(records.len(), 200);
    let mut rng = Rng(0xC0FF_EE00_0000_0008);
    let mut flipped = 0usize;
    for (n, enc) in records.iter().enumerate() {
        for i in 0..enc.len() {
            let mut bad = enc.clone();
            bad[i] ^= (1 + rng.below(255)) as u8;
            let mine = decode(&bad);
            let theirs = ref_decode(&bad);
            assert!(
                same(&mine, &theirs),
                "record {n}, byte {i}: {mine:?} vs {theirs:?}"
            );
            match mine {
                Decoded::Record(r, _) => panic!("record {n}, byte {i} decoded as {r:?}"),
                // A longer `len` asks for bytes that are not there: the only way a corrupt
                // record reads as "need more".
                Decoded::Incomplete => assert!(i < 4, "record {n}, byte {i}"),
                Decoded::Corrupt(_) => {}
            }
            if i >= 4 {
                assert!(matches!(mine, Decoded::Corrupt(_)), "record {n}, byte {i}");
            }
            flipped += 1;
        }
    }
    assert!(flipped > 10_000);
}

#[test]
fn rc_t9_every_truncation_needs_more_bytes_in_both() {
    let all = samples();
    let mut records = small(&all);
    records.push(all[10_000].encode());
    for (n, enc) in records.iter().enumerate() {
        for cut in 0..enc.len() {
            let mine = decode(&enc[..cut]);
            assert_eq!(mine, Decoded::Incomplete, "record {n}, cut {cut}");
            assert_eq!(ref_decode(&enc[..cut]), RefDecoded::Incomplete);
        }
    }
}

/// A record whose fields are given as they are, with a `len` and a crc that are right for them.
fn raw(id_len: u16, hdr_count: u16, body_len: u32, tail: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b.extend_from_slice(&7u64.to_le_bytes());
    b.extend_from_slice(&(-7i64).to_le_bytes());
    b.extend_from_slice(&9u32.to_le_bytes());
    b.extend_from_slice(&id_len.to_le_bytes());
    b.extend_from_slice(&hdr_count.to_le_bytes());
    b.extend_from_slice(&body_len.to_le_bytes());
    b.extend_from_slice(tail);
    let len = (b.len() - 4) as u32;
    let crc = crc32c::crc32c(&b[8..]);
    b[0..4].copy_from_slice(&len.to_le_bytes());
    b[4..8].copy_from_slice(&crc.to_le_bytes());
    b
}

fn header(k: &[u8], v: &[u8]) -> Vec<u8> {
    let mut h = (k.len() as u16).to_le_bytes().to_vec();
    h.extend_from_slice(k);
    h.extend_from_slice(&(v.len() as u16).to_le_bytes());
    h.extend_from_slice(v);
    h
}

#[test]
fn rc_t10_malformed_records_are_refused_for_the_same_reasons() {
    let mut cases: Vec<(&str, Vec<u8>, Option<Corruption>)> = Vec::new();
    // The length field.
    for len in [0u32, 31, MAX_RECORD as u32 + 1, u32::MAX] {
        let mut b = len.to_le_bytes().to_vec();
        b.extend_from_slice(&[0; 40]);
        cases.push(("bad length", b, Some(Corruption::Length(len as usize))));
    }
    // A length that is valid and a buffer that is shorter.
    let mut short = (MAX_RECORD as u32).to_le_bytes().to_vec();
    short.extend_from_slice(&[0; 40]);
    cases.push(("long record, short buffer", short, None));
    // The crc.
    let mut bad_crc = raw(0, 0, 0, b"");
    bad_crc[4] ^= 1;
    cases.push(("crc", bad_crc, None));
    // Fields that do not add up, under a crc that is right.
    cases.push((
        "msg_id past the end",
        raw(50, 0, 0, b"abc"),
        Some(Corruption::MsgIdPastEnd),
    ));
    cases.push((
        "msg_id not UTF-8",
        raw(2, 0, 0, &[0xFF, 0xFE]),
        Some(Corruption::MsgIdNotUtf8),
    ));
    cases.push((
        "header missing",
        raw(0, 1, 0, b""),
        Some(Corruption::HeaderPastEnd),
    ));
    cases.push((
        "header key length cut",
        raw(0, 1, 0, &[1]),
        Some(Corruption::HeaderPastEnd),
    ));
    cases.push((
        "header key past the end",
        raw(0, 1, 0, &[9, 0, b'k']),
        Some(Corruption::HeaderKeyPastEnd),
    ));
    cases.push((
        "header value length missing",
        raw(0, 1, 0, &[1, 0, b'k']),
        Some(Corruption::HeaderPastEnd),
    ));
    cases.push((
        "header value past the end",
        raw(0, 1, 0, &[1, 0, b'k', 9, 0, b'v']),
        Some(Corruption::HeaderValuePastEnd),
    ));
    cases.push((
        "header key not UTF-8",
        raw(0, 1, 0, &header(&[0xC3], b"v")),
        Some(Corruption::HeaderNotUtf8),
    ));
    cases.push((
        "header value not UTF-8",
        raw(0, 1, 0, &header(b"k", &[0xE6, 0x97])),
        Some(Corruption::HeaderNotUtf8),
    ));
    cases.push((
        "body longer than the rest",
        raw(0, 0, 9, b"abc"),
        Some(Corruption::Body {
            body_len: 9,
            at: FIXED,
            total: FIXED + 3,
        }),
    ));
    cases.push((
        "body shorter than the rest",
        raw(0, 0, 1, b"abc"),
        Some(Corruption::Body {
            body_len: 1,
            at: FIXED,
            total: FIXED + 3,
        }),
    ));
    // And the same fields made right, which both accept.
    cases.push((
        "well formed",
        raw(
            2,
            1,
            3,
            &[b"id".as_slice(), &header(b"k", b"v"), b"xyz"].concat(),
        ),
        None,
    ));

    for (name, buf, expect) in &cases {
        let mine = decode(buf);
        let theirs = ref_decode(buf);
        assert!(same(&mine, &theirs), "{name}: {mine:?} vs {theirs:?}");
        if let Some(why) = expect {
            assert_eq!(mine, Decoded::Corrupt(*why), "{name}");
        }
    }
    let by_name = |n: &str| cases.iter().find(|c| c.0 == n).map(|c| decode(&c.1));
    assert_eq!(
        by_name("long record, short buffer"),
        Some(Decoded::Incomplete)
    );
    assert!(matches!(
        by_name("crc"),
        Some(Decoded::Corrupt(Corruption::Crc { .. }))
    ));
    assert!(matches!(by_name("well formed"), Some(Decoded::Record(..))));
}

/// A msg_id, headers and a body length.
type Case = (String, Vec<(String, String)>, usize);

#[test]
fn rc_t11_check_fits_agrees_with_the_reference() {
    let pair = |k: usize, v: usize| ("k".repeat(k), "v".repeat(v));
    let hs = |h: &[(String, String)]| -> Vec<(String, String)> { h.to_vec() };
    let mut cases: Vec<Case> = vec![
        (String::new(), vec![], 0),
        (String::new(), vec![], MAX_BODY),
        (String::new(), vec![], MAX_BODY + 1),
        ("i".repeat(65_535), vec![], 0),
        ("i".repeat(65_536), vec![], 0),
        (String::new(), vec![pair(65_535, 65_535)], 0),
        (String::new(), vec![pair(65_536, 0)], 0),
        (String::new(), vec![pair(0, 65_536)], 0),
        (String::new(), vec![pair(1, 1); 65_535], 0),
        (String::new(), vec![pair(0, 0); 65_536], 0),
        // Total exactly MAX_RECORD, and one byte more.
        ("i".repeat(MAX_RECORD - MAX_BODY - FIXED), vec![], MAX_BODY),
        (
            "i".repeat(MAX_RECORD - MAX_BODY - FIXED + 1),
            vec![],
            MAX_BODY,
        ),
        ("i".repeat(65_535), vec![pair(65_535, 65_535)], MAX_BODY),
    ];
    cases.push(("x".into(), hs(&[pair(3, 4), pair(0, 0)]), 100));
    for (i, (id, h, body)) in cases.iter().enumerate() {
        let body = vec![0u8; *body];
        let borrowed: Vec<(&str, &str)> = h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mine = check_fits(id, &borrowed, &body);
        let theirs = ref_check_fits(id, h, &body);
        assert_eq!(
            mine.is_ok(),
            theirs.is_ok(),
            "case {i}: {mine:?} vs {theirs:?}"
        );
        assert_eq!(
            moruna_record::encoded_len(id, &borrowed, &body).is_ok(),
            theirs.is_ok(),
            "case {i}"
        );
        if mine.is_ok() {
            // What fits encodes to what the reference encodes, and decodes.
            let mut want = Vec::new();
            ref_encode(&mut want, 1, 2, 3, id, h, &body);
            let got = encode(1, 2, 3, id, &borrowed, &body).expect("fits");
            assert_eq!(got, want, "case {i}");
            assert!(same(&decode(&got), &ref_decode(&want)), "case {i}");
        } else {
            assert!(encode(1, 2, 3, id, &borrowed, &body).is_err(), "case {i}");
        }
    }
}
