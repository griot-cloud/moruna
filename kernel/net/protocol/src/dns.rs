//! The least of DNS (RFC 1035) a test's name server needs: read a query of one
//! question, and answer it with addresses. The pair suite's server answers the
//! network server's resolver with it, so a name resolves across the link.

use alloc::{string::String, vec::Vec};

use crate::Addr;

/// The record type of an IPv4 address.
pub const TYPE_A: u16 = 1;
/// The record type of an IPv6 address.
pub const TYPE_AAAA: u16 = 28;

/// A query: its id, its one question's name (lowercase, dotted) and type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    pub name: String,
    pub qtype: u16,
    /// The question as it arrived, name, type and class, for the answer to repeat.
    question: Vec<u8>,
}

/// Reads a query: a header with no flags but recursion desired, one question of class
/// IN, no answers. `None` for anything else.
pub fn parse_query(m: &[u8]) -> Option<Query> {
    let field =
        |at: usize| -> Option<u16> { Some(u16::from_be_bytes([*m.get(at)?, *m.get(at + 1)?])) };
    let (id, flags, qd, an, ns) = (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    // A query (QR 0), opcode 0; RD may be set.
    if flags & 0xf800 != 0 || qd != 1 || an != 0 || ns != 0 {
        return None;
    }
    let mut at = 12;
    let mut name = String::new();
    loop {
        let len = *m.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        // Compression has no place in a question, and labels are at most 63 bytes.
        if len > 63 || name.len() + len + 1 > 253 {
            return None;
        }
        let label = m.get(at..at + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for b in label {
            name.push(b.to_ascii_lowercase() as char);
        }
        at += len;
    }
    let qtype = field(at)?;
    if field(at + 2)? != 1 {
        return None;
    }
    Some(Query {
        id,
        name,
        qtype,
        question: m.get(12..at + 4)?.to_vec(),
    })
}

/// The answer to `q`: each address of its type, with a time to live of `ttl`; no
/// address of the type is an empty answer, and `known` false is "no such name".
pub fn answer(q: &Query, known: bool, addrs: &[Addr], ttl: u32) -> Vec<u8> {
    let records: Vec<&Addr> = addrs
        .iter()
        .filter(|a| (q.qtype == TYPE_A && a.is_v4()) || (q.qtype == TYPE_AAAA && !a.is_v4()))
        .collect();
    let mut m = Vec::new();
    m.extend_from_slice(&q.id.to_be_bytes());
    // A response, recursion desired and available, NXDOMAIN when not known.
    let rcode = if known { 0 } else { 3 };
    m.extend_from_slice(&(0x8180u16 | rcode).to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&(records.len() as u16).to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    m.extend_from_slice(&q.question);
    for a in records {
        // The name, as a pointer to the question's at offset 12.
        m.extend_from_slice(&[0xc0, 12]);
        m.extend_from_slice(&q.qtype.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        m.extend_from_slice(&ttl.to_be_bytes());
        match a.0 {
            core::net::IpAddr::V4(v) => {
                m.extend_from_slice(&4u16.to_be_bytes());
                m.extend_from_slice(&v.octets());
            }
            core::net::IpAddr::V6(v) => {
                m.extend_from_slice(&16u16.to_be_bytes());
                m.extend_from_slice(&v.octets());
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![];
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        m
    }

    #[test]
    fn ke2_a_query_is_read_and_answered() {
        let q = parse_query(&query(0x1234, "Pair.KE2.test", TYPE_A)).expect("query");
        assert_eq!(
            (q.id, q.name.as_str(), q.qtype),
            (0x1234, "pair.ke2.test", TYPE_A)
        );
        let six = Addr("fd4b:4532::2".parse().expect("v6"));
        let a = answer(&q, true, &[Addr::v4(10, 47, 0, 2), six], 60);
        assert_eq!(&a[0..2], &[0x12, 0x34]);
        assert_eq!(&a[6..8], &[0, 1], "one A record");
        assert_eq!(&a[a.len() - 4..], &[10, 47, 0, 2]);
        let q6 = parse_query(&query(7, "pair.ke2.test", TYPE_AAAA)).expect("query");
        let a6 = answer(&q6, true, &[Addr::v4(10, 47, 0, 2), six], 60);
        assert_eq!(&a6[a6.len() - 16..], &six_octets(six));
        let none = answer(&q, false, &[], 60);
        assert_eq!(none[3] & 0xf, 3);
    }

    fn six_octets(a: Addr) -> [u8; 16] {
        match a.0 {
            core::net::IpAddr::V6(v) => v.octets(),
            core::net::IpAddr::V4(_) => [0; 16],
        }
    }

    #[test]
    fn ke2_what_is_not_a_query_is_refused() {
        let good = query(1, "a.test", TYPE_A);
        assert!(parse_query(&good).is_some());
        for cut in 0..good.len() {
            assert!(parse_query(&good[..cut]).is_none(), "cut at {cut}");
        }
        let mut response = good.clone();
        response[2] |= 0x80;
        assert!(parse_query(&response).is_none());
        let mut two = good.clone();
        two[5] = 2;
        assert!(parse_query(&two).is_none());
        let mut pointer = good.clone();
        pointer[12] = 0xc0;
        assert!(parse_query(&pointer).is_none());
        let mut chaos = good.clone();
        let n = chaos.len();
        chaos[n - 1] = 3;
        assert!(parse_query(&chaos).is_none());
    }
}
