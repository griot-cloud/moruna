//! JSON values as FlexBuffers and back (SDD 22 e.4): a task's settings and a contract's document
//! travel in the FlatBuffers document as FlexBuffers, and are read and checked as the JSON values
//! a kind's settings schema describes.

use serde_json::Value;

/// `value` as a FlexBuffers buffer.
pub(crate) fn encode(value: &Value) -> Result<Vec<u8>, String> {
    flexbuffers::to_vec(value).map_err(|e| format!("not encodable as FlexBuffers: {e}"))
}

/// The deepest a value read from FlexBuffers may nest.
const MAX_DEPTH: u32 = 64;

/// The most values one buffer may decode to: a buffer is anyone's, and offsets may point many
/// elements at one large vector, so the work is bounded by count, not by the buffer's length.
const MAX_VALUES: usize = 1 << 20;

/// A FlexBuffers buffer as the JSON value it holds. The buffer is anyone's: every offset,
/// width, length and type is checked before it is followed, a value JSON cannot hold (a blob, a
/// float that is not finite) is refused by name, and nothing panics on any input. Read here
/// rather than by the `flexbuffers` crate's reader, which indexes without checking.
pub(crate) fn decode(bytes: &[u8]) -> Result<Value, String> {
    let fail = |why: &str| format!("not a FlexBuffers value: {why}");
    let n = bytes.len();
    if n < 3 {
        return Err(fail("shorter than a root"));
    }
    let width = usize::from(bytes[n - 1]);
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(fail("the root's width is not 1, 2, 4 or 8"));
    }
    let packed = bytes[n - 2];
    let at = n
        .checked_sub(2 + width)
        .ok_or_else(|| fail("the root's value is outside the buffer"))?;
    let mut reader = Flex {
        buf: bytes,
        values: 0,
    };
    reader.value(at, width, packed, 0).map_err(|why| fail(&why))
}

struct Flex<'a> {
    buf: &'a [u8],
    values: usize,
}

/// The FlexBuffers types this reader knows, by their codes.
mod ty {
    pub const NULL: u8 = 0;
    pub const INT: u8 = 1;
    pub const UINT: u8 = 2;
    pub const FLOAT: u8 = 3;
    pub const KEY: u8 = 4;
    pub const STRING: u8 = 5;
    pub const INDIRECT_INT: u8 = 6;
    pub const INDIRECT_UINT: u8 = 7;
    pub const INDIRECT_FLOAT: u8 = 8;
    pub const MAP: u8 = 9;
    pub const VECTOR: u8 = 10;
    pub const VECTOR_INT: u8 = 11;
    pub const VECTOR_UINT: u8 = 12;
    pub const VECTOR_FLOAT: u8 = 13;
    pub const VECTOR_KEY: u8 = 14;
    pub const VECTOR_INT2: u8 = 16;
    pub const VECTOR_FLOAT4: u8 = 24;
    pub const BLOB: u8 = 25;
    pub const BOOL: u8 = 26;
    pub const VECTOR_BOOL: u8 = 36;
}

impl Flex<'_> {
    fn bytes(&self, at: usize, len: usize) -> Result<&[u8], String> {
        at.checked_add(len)
            .and_then(|end| self.buf.get(at..end))
            .ok_or_else(|| format!("{len} bytes at {at} are outside the buffer"))
    }

    fn uint(&self, at: usize, width: usize) -> Result<u64, String> {
        let b = self.bytes(at, width)?;
        Ok(match width {
            1 => u64::from(b[0]),
            2 => u64::from(u16::from_le_bytes([b[0], b[1]])),
            4 => u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            8 => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            other => return Err(format!("a width of {other} bytes")),
        })
    }

    fn int(&self, at: usize, width: usize) -> Result<i64, String> {
        let raw = self.uint(at, width)?;
        Ok(match width {
            1 => i64::from(raw as u8 as i8),
            2 => i64::from(raw as u16 as i16),
            4 => i64::from(raw as u32 as i32),
            _ => raw as i64,
        })
    }

    fn float(&self, at: usize, width: usize) -> Result<Value, String> {
        let raw = self.uint(at, width)?;
        let value = match width {
            4 => f64::from(f32::from_bits(raw as u32)),
            8 => f64::from_bits(raw),
            other => return Err(format!("a float of {other} bytes")),
        };
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| format!("{value} is not a number JSON holds"))
    }

    /// The address an offset at `at` of `width` bytes points back to.
    fn target(&self, at: usize, width: usize) -> Result<usize, String> {
        let offset = usize::try_from(self.uint(at, width)?).map_err(|e| e.to_string())?;
        at.checked_sub(offset)
            .ok_or_else(|| format!("the offset at {at} points before the buffer"))
    }

    fn length(&self, at: usize, width: usize) -> Result<usize, String> {
        let prefix = at
            .checked_sub(width)
            .ok_or_else(|| format!("the length before {at} is outside the buffer"))?;
        let len = usize::try_from(self.uint(prefix, width)?).map_err(|e| e.to_string())?;
        // Every element takes at least a byte, so a length past the buffer is a lie.
        if len > self.buf.len() {
            return Err(format!(
                "a length of {len} in a buffer of {} bytes",
                self.buf.len()
            ));
        }
        Ok(len)
    }

    fn key(&self, at: usize) -> Result<String, String> {
        let rest = self
            .buf
            .get(at..)
            .ok_or_else(|| format!("a key at {at} is outside the buffer"))?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| format!("the key at {at} has no terminator"))?;
        String::from_utf8(rest[..end].to_vec()).map_err(|e| format!("a key that is not UTF-8: {e}"))
    }

    /// The value at `at`, stored in a slot of `slot` bytes, whose packed type is `packed`.
    fn value(&mut self, at: usize, slot: usize, packed: u8, depth: u32) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("values nested deeper than {MAX_DEPTH}"));
        }
        self.values += 1;
        if self.values > MAX_VALUES {
            return Err(format!("more than {MAX_VALUES} values"));
        }
        let kind = packed >> 2;
        let width = 1usize << (packed & 3);
        match kind {
            ty::NULL => Ok(Value::Null),
            ty::INT => Ok(Value::from(self.int(at, slot)?)),
            ty::UINT => Ok(Value::from(self.uint(at, slot)?)),
            ty::FLOAT => self.float(at, slot),
            ty::BOOL => Ok(Value::Bool(self.uint(at, slot)? != 0)),
            ty::INDIRECT_INT => {
                let to = self.target(at, slot)?;
                Ok(Value::from(self.int(to, width)?))
            }
            ty::INDIRECT_UINT => {
                let to = self.target(at, slot)?;
                Ok(Value::from(self.uint(to, width)?))
            }
            ty::INDIRECT_FLOAT => {
                let to = self.target(at, slot)?;
                self.float(to, width)
            }
            ty::KEY => {
                let to = self.target(at, slot)?;
                Ok(Value::String(self.key(to)?))
            }
            ty::STRING => {
                let to = self.target(at, slot)?;
                let len = self.length(to, width)?;
                let text = self.bytes(to, len)?;
                String::from_utf8(text.to_vec())
                    .map(Value::String)
                    .map_err(|e| format!("a string that is not UTF-8: {e}"))
            }
            ty::BLOB => Err("a blob, which is not a JSON value".to_string()),
            ty::VECTOR => {
                let to = self.target(at, slot)?;
                let len = self.length(to, width)?;
                let types = to + len * width;
                let mut items = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    let packed = self.bytes(types + i, 1)?[0];
                    items.push(self.value(to + i * width, width, packed, depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            ty::MAP => {
                let to = self.target(at, slot)?;
                let len = self.length(to, width)?;
                let keys_at = to
                    .checked_sub(3 * width)
                    .ok_or_else(|| "a map's keys are outside the buffer".to_string())?;
                let keys = self.target(keys_at, width)?;
                let key_width = usize::try_from(self.uint(to - 2 * width, width)?)
                    .map_err(|e| e.to_string())?;
                if !matches!(key_width, 1 | 2 | 4 | 8) {
                    return Err(format!("a map's keys of width {key_width}"));
                }
                if self.length(keys, key_width)? != len {
                    return Err("a map with as many keys as values it does not have".to_string());
                }
                let types = to + len * width;
                let mut map = serde_json::Map::new();
                for i in 0..len {
                    let key_at = self.target(keys + i * key_width, key_width)?;
                    let key = self.key(key_at)?;
                    let packed = self.bytes(types + i, 1)?[0];
                    let value = self.value(to + i * width, width, packed, depth + 1)?;
                    map.insert(key, value);
                }
                Ok(Value::Object(map))
            }
            ty::VECTOR_INT..=ty::VECTOR_KEY | ty::VECTOR_BOOL => {
                let to = self.target(at, slot)?;
                let len = self.length(to, width)?;
                let element = match kind {
                    ty::VECTOR_INT => ty::INT,
                    ty::VECTOR_UINT => ty::UINT,
                    ty::VECTOR_FLOAT => ty::FLOAT,
                    ty::VECTOR_KEY => ty::KEY,
                    _ => ty::BOOL,
                };
                self.typed(to, len, width, element, depth)
            }
            ty::VECTOR_INT2..=ty::VECTOR_FLOAT4 => {
                let to = self.target(at, slot)?;
                let offset = kind - ty::VECTOR_INT2;
                let len = usize::from(offset / 3) + 2;
                let element = [ty::INT, ty::UINT, ty::FLOAT][usize::from(offset % 3)];
                self.typed(to, len, width, element, depth)
            }
            other => Err(format!("type {other}, which is not a JSON value")),
        }
    }

    fn typed(
        &mut self,
        to: usize,
        len: usize,
        width: usize,
        element: u8,
        depth: u32,
    ) -> Result<Value, String> {
        let mut items = Vec::with_capacity(len.min(1024));
        for i in 0..len {
            let packed = (element << 2) | width.trailing_zeros() as u8;
            items.push(self.value(to + i * width, width, packed, depth + 1)?);
        }
        Ok(Value::Array(items))
    }
}

/// A FlexBuffers buffer that holds a map, as that map; `null` members are left out, as a
/// document's settings leave them (MH 4.1).
pub(crate) fn decode_map(bytes: &[u8]) -> Result<serde_json::Map<String, Value>, String> {
    match decode(bytes)? {
        Value::Object(mut map) => {
            map.retain(|_, value| !value.is_null());
            Ok(map)
        }
        other => Err(format!("a map of settings, not {}", kind_of(&other))),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a map",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_round_trip() {
        let value = json!({
            "url": "file:///data",
            "n": 3,
            "neg": -7,
            "big": u64::MAX,
            "f": 1.5,
            "flag": true,
            "list": [1, "two", {"three": [3.0]}],
            "nested": {"a": null}
        });
        let bytes = encode(&value).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), value);
        let map =
            decode_map(&encode(&json!({"a": 1, "b": null})).expect("encodes")).expect("a map");
        assert_eq!(Value::Object(map), json!({"a": 1}));
    }

    #[test]
    fn a_map_is_required() {
        let bytes = encode(&json!([1, 2])).expect("encodes");
        assert_eq!(
            decode_map(&bytes).expect_err("a list is no map"),
            "a map of settings, not a list"
        );
        for (value, said) in [
            (json!(null), "null"),
            (json!(true), "a boolean"),
            (json!(2), "a number"),
            (json!("s"), "a string"),
        ] {
            let bytes = encode(&value).expect("encodes");
            assert_eq!(
                decode_map(&bytes).expect_err("no map"),
                format!("a map of settings, not {said}")
            );
        }
        assert!(decode(&[0xff, 0x00]).is_err());
    }

    /// Every value the encoder writes reads back, typed vectors included, and every truncation
    /// and seeded corruption of one is a value or an error, never a panic.
    #[test]
    fn hostile_buffers_never_panic() {
        let value = json!({"a": [1, -2, 3.5, "x", null, true, {"b": [[], {}]}], "c": u64::MAX});
        let bytes = encode(&value).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), value);
        let mut typed = flexbuffers::Builder::default();
        {
            let mut v = typed.start_vector();
            v.push(&[1i32, 2, 3][..]);
            v.push(&[1.5f64, 2.5][..]);
            v.push(&[7u8, 8, 9, 10][..]);
            v.push(flexbuffers::Blob(&[1u8, 2][..]));
            v.end_vector();
        }
        assert!(decode(typed.view()).expect_err("a blob").contains("a blob"));
        let mut state = 0x5eed_u64;
        for case in 0..20_000 {
            let mut corrupt = bytes.clone();
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if case % 3 == 0 {
                corrupt.truncate((state >> 33) as usize % bytes.len());
            } else {
                for k in 0..=(state % 3) {
                    let at = ((state >> (8 * k + 3)) as usize) % corrupt.len();
                    corrupt[at] = (state >> (k * 7)) as u8;
                }
            }
            let _ = decode(&corrupt);
        }
    }
}
