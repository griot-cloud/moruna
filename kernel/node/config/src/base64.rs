//! Standard base64 (RFC 4648, section 4), read strictly: the `+/` alphabet, `=` padding where the
//! length needs it, the unused bits of the last character zero, and nothing else. Node config's
//! keys, Azure's user-data and the bodies of PEM blocks are all written this way.

use alloc::{string::String, vec::Vec};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A character's six bits, or `None` when it is not one of the alphabet's.
fn value(character: u8) -> Option<u32> {
    let bits = match character {
        b'A'..=b'Z' => character - b'A',
        b'a'..=b'z' => character - b'a' + 26,
        b'0'..=b'9' => character - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(bits))
}

/// The bytes `text` encodes, or `None` when it is not canonical standard base64.
pub fn decode(text: &[u8]) -> Option<Vec<u8>> {
    let (groups, rest) = text.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(groups.len() * 3);
    for (index, group) in groups.iter().enumerate() {
        let padding = match (group[2], group[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        if padding != 0 && index + 1 != groups.len() {
            return None;
        }
        let first = value(group[0])?;
        let second = value(group[1])?;
        let third = if padding == 2 { 0 } else { value(group[2])? };
        let fourth = if padding == 0 { value(group[3])? } else { 0 };
        // The bits a padded group does not use are zero in the one canonical encoding.
        if (padding == 2 && second & 0x0f != 0) || (padding == 1 && third & 0x03 != 0) {
            return None;
        }
        let word = (first << 18) | (second << 12) | (third << 6) | fourth;
        bytes.push((word >> 16) as u8);
        if padding < 2 {
            bytes.push((word >> 8) as u8);
        }
        if padding == 0 {
            bytes.push(word as u8);
        }
    }
    Some(bytes)
}

/// `bytes` as canonical standard base64, padded.
pub fn encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        let character = |shift: u32| char::from(ALPHABET[((word >> shift) & 0x3f) as usize]);
        text.push(character(18));
        text.push(character(12));
        text.push(if chunk.len() > 1 { character(6) } else { '=' });
        text.push(if chunk.len() > 2 { character(0) } else { '=' });
    }
    text
}
