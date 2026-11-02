//! A bounds-checked reader of the flatbuffers Arrow's `Message.fbs` and `Schema.fbs` describe
//! (contracts e.8, SDD 29).
//!
//! Every read is checked against the buffer, so a message that lies (an offset past its end, a
//! vtable outside it, a vector longer than what follows) is a sentence, never a panic and never a
//! read of memory it does not own. Offsets to tables, vectors and strings only ever point
//! forward, so a walk always ends; nesting is still bounded at 64 tables, the depth arrow's own
//! verifier allows, so a message nested deeply cannot exhaust a stack. The reader keeps no
//! state: a [`Table`] is a buffer and a position.

use alloc::{format, string::String};

/// The deepest nesting of tables a message may have (arrow's verifier's `max_depth`).
pub const MAX_DEPTH: u8 = 64;

fn short(what: &str, at: usize) -> String {
    format!("the flatbuffer ends inside {what} at byte {at}")
}

fn bytes<const N: usize>(buf: &[u8], at: usize, what: &str) -> Result<[u8; N], String> {
    at.checked_add(N)
        .and_then(|end| buf.get(at..end))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| short(what, at))
}

fn read_u16(buf: &[u8], at: usize, what: &str) -> Result<u16, String> {
    bytes::<2>(buf, at, what).map(u16::from_le_bytes)
}

fn read_u32(buf: &[u8], at: usize, what: &str) -> Result<u32, String> {
    bytes::<4>(buf, at, what).map(u32::from_le_bytes)
}

/// The position an unsigned offset stored at `at` points to.
fn follow(buf: &[u8], at: usize, what: &str) -> Result<usize, String> {
    let offset = read_u32(buf, at, what)? as usize;
    match at.checked_add(offset) {
        Some(to) if to < buf.len() => Ok(to),
        _ => Err(format!(
            "{what} at byte {at} points to byte {}, past the end of the flatbuffer of {} bytes",
            at as u64 + offset as u64,
            buf.len()
        )),
    }
}

/// One table of a flatbuffer: where it is, and how deep in the message it lies.
#[derive(Clone, Copy, Debug)]
pub struct Table<'a> {
    buf: &'a [u8],
    loc: usize,
    vtable: usize,
    vtable_len: u16,
    depth: u8,
}

impl<'a> Table<'a> {
    /// The root table of a whole flatbuffer.
    pub fn root(buf: &'a [u8]) -> Result<Table<'a>, String> {
        let loc = follow(buf, 0, "the root offset")?;
        Table::new(buf, loc, 0)
    }

    /// The table at `loc` of `buf`: how a caller that holds an already verified flatbuffer (an
    /// arrow-ipc message's `_tab.buf()` and `_tab.loc()`) hands it over.
    pub fn at(buf: &'a [u8], loc: usize) -> Result<Table<'a>, String> {
        Table::new(buf, loc, 0)
    }

    fn new(buf: &'a [u8], loc: usize, depth: u8) -> Result<Table<'a>, String> {
        if depth >= MAX_DEPTH {
            return Err(format!(
                "the flatbuffer nests tables more than {MAX_DEPTH} deep"
            ));
        }
        let back = i32::from_le_bytes(bytes::<4>(buf, loc, "a table's vtable offset")?);
        let vtable = (loc as i64)
            .checked_sub(i64::from(back))
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v < buf.len())
            .ok_or_else(|| {
                format!("the table at byte {loc} has its vtable outside the flatbuffer")
            })?;
        let vtable_len = read_u16(buf, vtable, "a vtable")?;
        if vtable_len < 4 || vtable_len % 2 != 0 {
            return Err(format!(
                "the vtable at byte {vtable} declares {vtable_len} bytes, not a whole vtable"
            ));
        }
        if (vtable as u64) + u64::from(vtable_len) > buf.len() as u64 {
            return Err(short("a vtable", vtable));
        }
        Ok(Table {
            buf,
            loc,
            vtable,
            vtable_len,
            depth,
        })
    }

    /// The flatbuffer the table is in.
    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

    /// Where the table starts in [`Table::buf`].
    pub fn loc(&self) -> usize {
        self.loc
    }

    /// Where field `slot` (its index in the schema, from 0) is stored, or `None` when it is
    /// absent, which flatbuffers does for a value equal to its default.
    pub fn position(&self, slot: usize) -> Result<Option<usize>, String> {
        let entry = 4 + 2 * slot;
        if entry + 2 > usize::from(self.vtable_len) {
            return Ok(None);
        }
        let offset = read_u16(self.buf, self.vtable + entry, "a vtable")?;
        if offset == 0 {
            return Ok(None);
        }
        Ok(Some(self.loc + usize::from(offset)))
    }

    fn scalar<const N: usize>(&self, slot: usize) -> Result<Option<[u8; N]>, String> {
        match self.position(slot)? {
            Some(at) => bytes::<N>(self.buf, at, "a field").map(Some),
            None => Ok(None),
        }
    }

    pub fn u8(&self, slot: usize, default: u8) -> Result<u8, String> {
        Ok(self.scalar::<1>(slot)?.map_or(default, |b| b[0]))
    }

    pub fn i8(&self, slot: usize, default: i8) -> Result<i8, String> {
        Ok(self.scalar::<1>(slot)?.map_or(default, i8::from_le_bytes))
    }

    pub fn bool(&self, slot: usize, default: bool) -> Result<bool, String> {
        Ok(self.scalar::<1>(slot)?.map_or(default, |b| b[0] != 0))
    }

    pub fn i16(&self, slot: usize, default: i16) -> Result<i16, String> {
        Ok(self.scalar::<2>(slot)?.map_or(default, i16::from_le_bytes))
    }

    pub fn i32(&self, slot: usize, default: i32) -> Result<i32, String> {
        Ok(self.scalar::<4>(slot)?.map_or(default, i32::from_le_bytes))
    }

    pub fn i64(&self, slot: usize, default: i64) -> Result<i64, String> {
        Ok(self.scalar::<8>(slot)?.map_or(default, i64::from_le_bytes))
    }

    /// The table field `slot` points to.
    pub fn table(&self, slot: usize) -> Result<Option<Table<'a>>, String> {
        match self.position(slot)? {
            Some(at) => {
                let loc = follow(self.buf, at, "a table offset")?;
                Table::new(self.buf, loc, self.depth + 1).map(Some)
            }
            None => Ok(None),
        }
    }

    /// The vector field `slot` points to, of elements `width` bytes each, inline.
    pub fn vector(&self, slot: usize, width: usize) -> Result<Option<Vector<'a>>, String> {
        let Some(at) = self.position(slot)? else {
            return Ok(None);
        };
        let start = follow(self.buf, at, "a vector offset")?;
        let len = read_u32(self.buf, start, "a vector's length")? as usize;
        let data = start + 4;
        let end = len
            .checked_mul(width)
            .and_then(|n| data.checked_add(n))
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| {
                format!(
                    "the vector at byte {start} declares {len} elements of {width} bytes, past \
                     the end of the flatbuffer of {} bytes",
                    self.buf.len()
                )
            })?;
        Ok(Some(Vector {
            buf: self.buf,
            data,
            end,
            len,
            width,
            depth: self.depth,
        }))
    }

    /// The string field `slot` points to; a string that is not UTF-8 is refused.
    pub fn str(&self, slot: usize) -> Result<Option<&'a str>, String> {
        match self.vector(slot, 1)? {
            Some(v) => core::str::from_utf8(v.bytes())
                .map(Some)
                .map_err(|_| format!("the string at byte {} is not UTF-8", v.data)),
            None => Ok(None),
        }
    }
}

/// A vector of a flatbuffer: `len` elements of `width` bytes from `data`.
#[derive(Clone, Copy, Debug)]
pub struct Vector<'a> {
    buf: &'a [u8],
    data: usize,
    end: usize,
    len: usize,
    width: usize,
    depth: u8,
}

impl<'a> Vector<'a> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where the elements start in the flatbuffer.
    pub fn start(&self) -> usize {
        self.data
    }

    /// The elements' bytes.
    pub fn bytes(&self) -> &'a [u8] {
        &self.buf[self.data..self.end]
    }

    /// Element `i`'s bytes.
    pub fn element(&self, i: usize) -> Option<&'a [u8]> {
        if i >= self.len {
            return None;
        }
        let at = self.data + i * self.width;
        self.buf.get(at..at + self.width)
    }

    /// Element `i` as a little-endian `i32` (a vector of `int`).
    pub fn i32(&self, i: usize) -> Option<i32> {
        self.element(i)
            .and_then(|b| b.try_into().ok())
            .map(i32::from_le_bytes)
    }

    /// Element `i` as a little-endian `i64` (a vector of `long`).
    pub fn i64(&self, i: usize) -> Option<i64> {
        self.element(i)
            .and_then(|b| b.try_into().ok())
            .map(i64::from_le_bytes)
    }

    /// Element `i` of a vector of tables.
    pub fn table(&self, i: usize) -> Result<Table<'a>, String> {
        if i >= self.len || self.width != 4 {
            return Err(format!("the vector has no table {i}"));
        }
        let at = self.data + i * 4;
        let loc = follow(self.buf, at, "a table offset")?;
        Table::new(self.buf, loc, self.depth + 1)
    }
}

/// The two `i64` of a struct of 16 bytes: a `FieldNode` (length, null count) or a `Buffer`
/// (offset, length).
pub fn pair(element: &[u8]) -> Option<(i64, i64)> {
    let a: [u8; 8] = element.get(0..8)?.try_into().ok()?;
    let b: [u8; 8] = element.get(8..16)?.try_into().ok()?;
    Some((i64::from_le_bytes(a), i64::from_le_bytes(b)))
}
