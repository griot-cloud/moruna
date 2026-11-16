//! Columns as the core reads them: values and a validity bitmap, borrowed from wherever the
//! batch lives (an arrow-rs array, or a record mapped into a task).

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Error, Result};

/// A validity bitmap: value `i` is valid when bit `offset + i` of `bytes` is set, least
/// significant bit first, as Arrow lays it out.
#[derive(Clone, Copy, Debug)]
pub struct Bits<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Bits<'a> {
    /// A bitmap of at least `len` bits from bit `offset`, refused when `bytes` is too short.
    pub fn new(bytes: &'a [u8], offset: usize, len: usize) -> Result<Bits<'a>> {
        let need = offset
            .checked_add(len)
            .map(|bits| bits.div_ceil(8))
            .ok_or_else(|| Error::Layout(format!("a bitmap of {len} bits at {offset}")))?;
        if bytes.len() < need {
            return Err(Error::Layout(format!(
                "a bitmap of {len} bits at bit {offset} needs {need} bytes and has {}",
                bytes.len()
            )));
        }
        Ok(Bits { bytes, offset })
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        let at = self.offset + i;
        self.bytes[at / 8] >> (at % 8) & 1 == 1
    }
}

/// A column of `T` with its validity; no bitmap means every value is valid.
#[derive(Clone, Copy, Debug)]
pub struct Col<'a, T> {
    pub values: &'a [T],
    pub valid: Option<Bits<'a>>,
}

impl<'a, T: Copy> Col<'a, T> {
    /// A column of `values` whose bitmap, if any, covers every value.
    pub fn new(values: &'a [T], valid: Option<Bits<'a>>) -> Col<'a, T> {
        Col { values, valid }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        self.valid.is_none_or(|b| b.get(i))
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<T> {
        self.is_valid(i).then(|| self.values[i])
    }

    /// The number of nulls.
    pub fn null_count(&self) -> usize {
        match self.valid {
            None => 0,
            Some(b) => (0..self.values.len()).filter(|&i| !b.get(i)).count(),
        }
    }
}

/// Checks that every column has `rows` values.
pub fn same_len(rows: usize, cols: &[(&str, usize)]) -> Result<()> {
    for (name, len) in cols {
        if *len != rows {
            return Err(Error::Layout(format!(
                "column {name} has {len} values and the batch {rows} rows"
            )));
        }
    }
    Ok(())
}

/// A validity bitmap being built, one bit a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitsBuf {
    pub bytes: Vec<u8>,
    pub len: usize,
    pub nulls: usize,
}

impl BitsBuf {
    pub fn with_capacity(n: usize) -> BitsBuf {
        BitsBuf {
            bytes: Vec::with_capacity(n.div_ceil(8)),
            len: 0,
            nulls: 0,
        }
    }

    /// `n` set bits.
    pub fn all_set(n: usize) -> BitsBuf {
        let mut bytes = vec![0xffu8; n.div_ceil(8)];
        if let (false, Some(last)) = (n.is_multiple_of(8), bytes.last_mut()) {
            *last = (1u8 << (n % 8)) - 1;
        }
        BitsBuf {
            bytes,
            len: n,
            nulls: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, bit: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.len % 8);
        } else {
            self.nulls += 1;
        }
        self.len += 1;
    }

    pub fn get(&self, i: usize) -> bool {
        self.bytes[i / 8] >> (i % 8) & 1 == 1
    }
}

/// The offsets of a string, binary or list column: 32 or 64 bits.
#[derive(Clone, Copy, Debug)]
pub enum Offsets<'a> {
    Small(&'a [i32]),
    Large(&'a [i64]),
}

impl Offsets<'_> {
    /// The number of values the offsets bound (one fewer than the offsets).
    pub fn len(&self) -> usize {
        match self {
            Offsets::Small(o) => o.len().saturating_sub(1),
            Offsets::Large(o) => o.len().saturating_sub(1),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Value `i`'s range, checked: non-negative and in order.
    pub fn range(&self, i: usize) -> Result<core::ops::Range<usize>> {
        let bad = || Error::Layout(format!("offsets {i} and {} are not a range", i + 1));
        let (s, e) = match self {
            Offsets::Small(o) => (
                o.get(i).copied().map(i64::from),
                o.get(i + 1).copied().map(i64::from),
            ),
            Offsets::Large(o) => (o.get(i).copied(), o.get(i + 1).copied()),
        };
        let (s, e) = (s.ok_or_else(bad)?, e.ok_or_else(bad)?);
        let (s, e) = (
            usize::try_from(s).map_err(|_| bad())?,
            usize::try_from(e).map_err(|_| bad())?,
        );
        if s > e {
            return Err(bad());
        }
        Ok(s..e)
    }
}

/// A string column: offsets, data and validity.
#[derive(Clone, Copy, Debug)]
pub struct Strs<'a> {
    pub offsets: Offsets<'a>,
    pub data: &'a [u8],
    pub valid: Option<Bits<'a>>,
}

impl<'a> Strs<'a> {
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub fn is_valid(&self, i: usize) -> bool {
        self.valid.is_none_or(|b| b.get(i))
    }

    /// Value `i`, `None` when null; a range outside the data or bytes that are not UTF-8 are
    /// errors, as pyarrow refuses them.
    pub fn get(&self, i: usize) -> Result<Option<&'a str>> {
        if !self.is_valid(i) {
            return Ok(None);
        }
        let r = self.offsets.range(i)?;
        let bytes = self
            .data
            .get(r)
            .ok_or_else(|| Error::Layout(format!("string {i} is outside its data")))?;
        core::str::from_utf8(bytes)
            .map(Some)
            .map_err(|_| Error::Value(format!("UnicodeDecodeError: string {i} is not UTF-8")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps_and_columns() {
        assert!(Bits::new(&[0xff], 4, 5).is_err());
        let b = Bits::new(&[0b0000_0101], 0, 3).unwrap();
        let c = Col::new(&[1i64, 2, 3][..], Some(b));
        assert_eq!((c.len(), c.is_empty(), c.null_count()), (3, false, 1));
        assert_eq!((c.get(0), c.get(1)), (Some(1), None));
        assert!(same_len(3, &[("a", 3)]).is_ok());
        assert!(same_len(3, &[("a", 2)]).is_err());
        let all = BitsBuf::all_set(10);
        assert_eq!(all.bytes, [0xff, 0x03]);
        assert!(all.get(9));
        assert_eq!(BitsBuf::all_set(8).bytes, [0xff]);
    }

    #[test]
    fn strings_are_checked() {
        let o = [0i64, 2, 5, 3];
        let s = Strs {
            offsets: Offsets::Large(&o),
            data: b"hi\xff\xfeA",
            valid: None,
        };
        assert_eq!((s.len(), s.is_empty()), (3, false));
        assert_eq!(s.get(0).unwrap(), Some("hi"));
        assert!(s.get(1).is_err()); // not UTF-8
        assert!(s.get(2).is_err()); // backwards
        assert!(s.get(3).is_err()); // past the offsets
        let short = [0i32, 9];
        let t = Strs {
            offsets: Offsets::Small(&short),
            data: b"abc",
            valid: Some(Bits::new(&[0b1], 0, 1).unwrap()),
        };
        assert!(t.get(0).is_err()); // past the data
        assert!(Offsets::Small(&[]).is_empty());
        assert!(Offsets::Small(&[-1, 0]).range(0).is_err());
    }
}
