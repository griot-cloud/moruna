//! An answer bigger than a window, part by part (stream P's `Rings::step_parts`): the output's
//! rows cut into records that each fit a window, in order. The first part computes the output;
//! the rows after it are kept, copied out of the input, for the parts that follow.

use alloc::borrow::Cow;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use moruna_gate_functions::cols::{Bits, BitsBuf};

use crate::encode::{self, OutArray};
use crate::types::{LField, LType};
use crate::{Error, Job, Output};

/// What a part wrote into its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrote {
    /// A record of this many bytes, the answer's last.
    Last(usize),
    /// A record of this many bytes, and more of the answer to come.
    More(usize),
}

/// The rows of an answer not yet written, between parts.
#[derive(Default)]
pub struct Carry {
    rest: Option<Output<'static>>,
}

impl Carry {
    pub fn new() -> Carry {
        Carry::default()
    }

    /// Whether rows are waiting for the next part.
    pub fn pending(&self) -> bool {
        self.rest.is_some()
    }
}

/// Part `part` of `job`'s answer to the record `input`, written into `window`. Part 0 computes
/// the answer; a later part writes the rows part 0 left in `carry`.
pub fn apply_part(
    job: Job,
    input: &[u8],
    window: &mut [u8],
    part: u32,
    carry: &mut Carry,
) -> Result<Wrote, Error> {
    if part == 0 {
        carry.rest = None;
        let batch = crate::decode::Batch::read(input)?;
        let out = crate::compute(job, &batch)?;
        return write_from(&out, 0, window, carry);
    }
    let rest = carry
        .rest
        .take()
        .ok_or_else(|| Error::layout("a later part with no rows left from the first"))?;
    write_from(&rest, 0, window, carry)
}

/// Writes rows from `start` of `out` into `window`, as many as fit; the rows left go to `carry`.
fn write_from(
    out: &Output<'_>,
    start: usize,
    window: &mut [u8],
    carry: &mut Carry,
) -> Result<Wrote, Error> {
    let left = out.rows - start;
    if start == 0 {
        match encode::write(&out.fields, &out.columns, out.rows, window) {
            Ok(len) => return Ok(Wrote::Last(len)),
            Err(Error::WindowTooSmall { .. }) if left > 1 => {}
            Err(e) => return Err(e),
        }
    }
    // Rows in proportion to the room past one row's record, then halved until they fit.
    let one = slice_output(out, start, 1)?;
    let one_len = encode::lay_out(&one.fields, &one.columns, 1)?.record_len;
    let all_len = encode::lay_out(&out.fields, &out.columns, out.rows)?.record_len;
    let have = window.len() as u64;
    if one_len > have {
        return Err(Error::WindowTooSmall {
            need: one_len,
            have,
        });
    }
    let per_row = (all_len.saturating_sub(one_len) / (out.rows.max(2) as u64 - 1)).max(1);
    let mut rows = (((have - one_len) / per_row) as usize)
        .saturating_add(1)
        .clamp(1, left - 1);
    loop {
        let part = slice_output(out, start, rows)?;
        match encode::write(&part.fields, &part.columns, rows, window) {
            Ok(len) => {
                carry.rest = Some(slice_output(out, start + rows, left - rows)?);
                return Ok(Wrote::More(len));
            }
            Err(Error::WindowTooSmall { .. }) if rows > 1 => rows /= 2,
            Err(e) => return Err(e),
        }
    }
}

/// `len` rows of `out` from `start`, owned.
pub fn slice_output(out: &Output<'_>, start: usize, len: usize) -> Result<Output<'static>, Error> {
    let mut o = Output::new(len);
    for (f, c) in out.fields.iter().zip(&out.columns) {
        o.push(f.clone(), slice_array(c, &f.ty, start, len)?);
    }
    Ok(o)
}

fn slice_bits(bytes: &[u8], start: usize, len: usize) -> Result<BitsBuf, Error> {
    let src = Bits::new(bytes, start, len)?;
    let mut b = BitsBuf::with_capacity(len);
    for i in 0..len {
        b.push(src.get(i));
    }
    Ok(b)
}

fn slice_validity<'s>(
    a: &OutArray<'_>,
    start: usize,
    len: usize,
) -> Result<(usize, Cow<'s, [u8]>), Error> {
    match a.buffers.first() {
        Some(v) if a.null_count > 0 && !v.is_empty() => {
            let b = slice_bits(v, start, len)?;
            Ok(if b.nulls > 0 {
                (b.nulls, Cow::Owned(b.bytes))
            } else {
                (0, Cow::Borrowed(&[][..]))
            })
        }
        _ => Ok((0, Cow::Borrowed(&[][..]))),
    }
}

fn buffer<'b>(a: &'b OutArray<'_>, i: usize) -> Result<&'b [u8], Error> {
    a.buffers
        .get(i)
        .map(|b| &b[..])
        .ok_or_else(|| Error::layout("an output array without the buffers of its type"))
}

fn range(b: &[u8], from: usize, to: usize) -> Result<&[u8], Error> {
    b.get(from..to)
        .ok_or_else(|| Error::layout("an output buffer shorter than its rows"))
}

/// `len` values of `a` (of type `ty`) from `start`, owned.
pub fn slice_array(
    a: &OutArray<'_>,
    ty: &LType,
    start: usize,
    len: usize,
) -> Result<OutArray<'static>, Error> {
    if start.checked_add(len).is_none_or(|end| end > a.len) {
        return Err(Error::layout("a slice past the output's rows"));
    }
    let (null_count, validity) = slice_validity(a, start, len)?;
    let (buffers, children) = match ty {
        LType::Int { .. } | LType::Float { .. } | LType::Timestamp { .. } => {
            let w = ty.width().unwrap_or(8);
            let v = range(buffer(a, 1)?, start * w, (start + len) * w)?;
            (vec![validity, Cow::Owned(v.to_vec())], Vec::new())
        }
        LType::Bool => {
            let b = slice_bits(buffer(a, 1)?, start, len)?;
            (vec![validity, Cow::Owned(b.bytes)], Vec::new())
        }
        LType::Utf8 | LType::Binary => {
            let (o, d) = slice_text::<i32>(a, start, len)?;
            (vec![validity, Cow::Owned(o), Cow::Owned(d)], Vec::new())
        }
        LType::LargeUtf8 | LType::LargeBinary => {
            let (o, d) = slice_text::<i64>(a, start, len)?;
            (vec![validity, Cow::Owned(o), Cow::Owned(d)], Vec::new())
        }
        LType::FixedSizeList { size, child } => {
            let size = usize::try_from(*size).map_err(|_| Error::layout("a list size"))?;
            let c = a
                .children
                .first()
                .ok_or_else(|| Error::layout("a list without its values"))?;
            let child = slice_array(c, &child.ty, start * size, len * size)?;
            (vec![validity], vec![child])
        }
        other => {
            return Err(Error::schema(&format!(
                "an output column of {other:?} cannot be cut into parts"
            )));
        }
    };
    Ok(OutArray {
        len,
        null_count,
        buffers,
        children,
    })
}

fn slice_text<O: encode::Offset>(
    a: &OutArray<'_>,
    start: usize,
    len: usize,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    // The offsets read value by value: an owned buffer need not be aligned for a cast.
    let width = core::mem::size_of::<O>();
    let raw = buffer(a, 1)?;
    let data = buffer(a, 2)?;
    let at = |i: usize| -> Result<usize, Error> {
        let b = raw
            .get(i * width..(i + 1) * width)
            .ok_or_else(|| Error::layout("text offsets past the rows"))?;
        let v = if width == 4 {
            i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        } else {
            i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        };
        usize::try_from(v).map_err(|_| Error::layout("a negative text offset"))
    };
    let (s, e) = (at(start)?, at(start + len)?);
    if s > e {
        return Err(Error::layout("text offsets out of order"));
    }
    let mut out: Vec<O> = Vec::with_capacity(len + 1);
    for i in start..=start + len {
        let v = at(i)?;
        if v < s || v > e {
            return Err(Error::layout("text offsets out of order"));
        }
        out.push(O::from_usize(v - s).ok_or_else(|| Error::layout("an offset"))?);
    }
    Ok((
        bytemuck::cast_slice(&out).to_vec(),
        range(data, s, e)?.to_vec(),
    ))
}

/// For messages: a field's description.
pub fn describe(f: &LField) -> alloc::string::String {
    encode::describe(f)
}
