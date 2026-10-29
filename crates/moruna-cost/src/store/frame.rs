//! The record frame of the local store's files (SDD 24 e.5).
//!
//! ```text
//! offset  size  field
//! 0       4     magic   C0 5D E7 0B
//! 4       4     len     u32 LE, the body's bytes
//! 8       4     crc     crc32c of bytes [4, 8) and the body
//! 12      len   body    one observation, JSON
//! ```
//!
//! `moruna-record`'s layout (SDD 21) is on the `v1` line only; when it reaches `main` this frame
//! is replaced by it (KB4's carry-over). The magic is what lets a reader find the next record
//! after a torn one: 0xC0 never occurs in UTF-8, so never in a body.

/// The first four bytes of every record.
pub(crate) const MAGIC: [u8; 4] = [0xC0, 0x5D, 0xE7, 0x0B];
/// Bytes before the body.
pub(crate) const HEADER: usize = 12;

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

const fn table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 == 1 { (c >> 1) ^ POLY } else { c >> 1 };
            bit += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = table();

fn crc32c_update(mut c: u32, data: &[u8]) -> u32 {
    for &b in data {
        c = TABLE[usize::from((c as u8) ^ b)] ^ (c >> 8);
    }
    c
}

fn crc(len: &[u8], body: &[u8]) -> u32 {
    !crc32c_update(crc32c_update(!0u32, len), body)
}

/// One framed record, or `None` when the body is longer than a `u32` counts.
pub(crate) fn encode(body: &[u8]) -> Option<Vec<u8>> {
    let len = u32::try_from(body.len()).ok()?.to_le_bytes();
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len);
    out.extend_from_slice(&crc(&len, body).to_le_bytes());
    out.extend_from_slice(body);
    Some(out)
}

/// The records of a file's bytes.
pub(crate) struct Decoded<'a> {
    /// Each whole record's offset and body, in order.
    pub records: Vec<(usize, &'a [u8])>,
    /// Places where a record was cut short or does not check: a torn write.
    pub torn: u64,
}

/// Read every whole record. Where the bytes at a position are not a whole record that checks,
/// that is one torn record, and reading resumes at the next magic.
pub(crate) fn decode(bytes: &[u8]) -> Decoded<'_> {
    let mut records = Vec::new();
    let mut torn = 0;
    let mut at = 0;
    while at < bytes.len() {
        match whole(bytes, at) {
            Some((body, next)) => {
                records.push((at, body));
                at = next;
            }
            None => {
                torn += 1;
                at = match find_magic(bytes, at + 1) {
                    Some(next) => next,
                    None => bytes.len(),
                };
            }
        }
    }
    Decoded { records, torn }
}

fn whole(bytes: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let header = bytes.get(at..at + HEADER)?;
    if header[0..4] != MAGIC {
        return None;
    }
    let len_bytes = &header[4..8];
    let len = usize::try_from(u32::from_le_bytes(len_bytes.try_into().ok()?)).ok()?;
    let want = u32::from_le_bytes(header[8..12].try_into().ok()?);
    let end = at.checked_add(HEADER)?.checked_add(len)?;
    let body = bytes.get(at + HEADER..end)?;
    (crc(len_bytes, body) == want).then_some((body, end))
}

fn find_magic(bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_reference_value() {
        assert_eq!(!crc32c_update(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn whole_torn_and_resynchronised() {
        let a = encode(b"{\"a\":1}").expect("frames");
        let b = encode(b"{\"b\":2}").expect("frames");
        let mut file = a.clone();
        file.extend_from_slice(&b);
        let d = decode(&file);
        assert_eq!(d.torn, 0);
        assert_eq!(d.records.len(), 2);
        assert_eq!(d.records[1], (a.len(), &b"{\"b\":2}"[..]));

        // A tail cut anywhere is one torn record and costs nothing before it.
        for cut in 1..b.len() {
            let mut file = a.clone();
            file.extend_from_slice(&b[..cut]);
            let d = decode(&file);
            assert_eq!((d.records.len(), d.torn), (1, 1), "cut at {cut}");
        }
        // A torn record followed by a whole one: the whole one is found.
        let mut file = a.clone();
        file.extend_from_slice(&b[..b.len() - 3]);
        file.extend_from_slice(&a);
        let d = decode(&file);
        assert_eq!((d.records.len(), d.torn), (2, 1));
        // A body that does not check.
        let mut bad = a.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let d = decode(&bad);
        assert_eq!((d.records.len(), d.torn), (0, 1));
        // Garbage with no magic at all.
        let d = decode(b"garbage");
        assert_eq!((d.records.len(), d.torn), (0, 1));
    }
}
