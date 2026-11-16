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
