//! An output record written into a window: a schema message, a record batch message and the
//! body, each non-empty buffer on a page boundary from the record's start (contracts e.7), laid
//! out by `moruna_ipc_check::frame::page_layout`, the layout the engine's check expects.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use moruna_gate_functions::cols::BitsBuf;
use moruna_ipc_check::format::{header, types, version};
use moruna_ipc_check::frame::{self, CONTINUATION};
use moruna_ipc_check::record::PAGE;

use crate::Error;
use crate::decode::Array;
use crate::fb::{Obj, Val, finish};
use crate::types::{LField, LType};

/// One output array: its length, null count, buffers in IPC order and children.
#[derive(Clone, Debug, Default)]
pub struct OutArray<'a> {
    pub len: usize,
    pub null_count: usize,
    pub buffers: Vec<Cow<'a, [u8]>>,
    pub children: Vec<OutArray<'a>>,
}

fn validity_buf<'a>(v: Option<BitsBuf>) -> (usize, Cow<'a, [u8]>) {
    match v {
        Some(b) if b.nulls > 0 => (b.nulls, Cow::Owned(b.bytes)),
        _ => (0, Cow::Borrowed(&[][..])),
    }
}

impl<'a> OutArray<'a> {
    /// A fixed-width column of `values`, with `valid` when some are null.
    pub fn fixed<T: bytemuck::Pod>(values: Vec<T>, valid: Option<BitsBuf>) -> OutArray<'a> {
        let (null_count, validity) = validity_buf(valid);
        OutArray {
            len: values.len(),
            null_count,
            buffers: vec![validity, Cow::Owned(bytemuck::cast_slice(&values).to_vec())],
            children: Vec::new(),
        }
    }

    /// A boolean column, no nulls.
    pub fn bools(values: &[bool]) -> OutArray<'a> {
        let mut bits = BitsBuf::with_capacity(values.len());
        for v in values {
            bits.push(*v);
        }
        OutArray {
            len: values.len(),
            null_count: 0,
            buffers: vec![Cow::Borrowed(&[][..]), Cow::Owned(bits.bytes)],
            children: Vec::new(),
        }
    }

    /// An input array passed through unchanged, its buffers borrowed.
    pub fn borrowed(a: &Array<'a>) -> OutArray<'a> {
        OutArray {
            len: a.len,
            null_count: a.null_count,
            buffers: a.buffers.iter().map(|b| Cow::Borrowed(*b)).collect(),
            children: a.children.iter().map(OutArray::borrowed).collect(),
        }
    }

    /// The rows `rows` of an input array of type `ty`, in order.
    pub fn take(a: &Array<'a>, ty: &LType, rows: &[u32]) -> Result<OutArray<'a>, Error> {
        let valid = a.validity()?;
        let validity = valid.map(|v| {
            let mut out = BitsBuf::with_capacity(rows.len());
            for &r in rows {
                out.push(v.get(r as usize));
            }
            out
        });
        let check = |r: u32| {
            if (r as usize) < a.len {
                Ok(r as usize)
            } else {
                Err(Error::layout("a row past the array"))
            }
        };
        let (null_count, vbuf) = validity_buf(validity);
        let buffers = match ty {
            LType::Int { .. } | LType::Float { .. } | LType::Timestamp { .. } => {
                let w = ty.width().unwrap_or(8);
                let src = a.values::<u8>(1, a.len * w)?;
                let mut out = Vec::with_capacity(rows.len() * w);
                for &r in rows {
                    let r = check(r)?;
                    out.extend_from_slice(&src[r * w..(r + 1) * w]);
                }
                vec![vbuf, Cow::Owned(out)]
            }
            LType::Bool => {
                let src = moruna_gate_functions::cols::Bits::new(
                    a.buffers.get(1).copied().unwrap_or(&[]),
                    0,
                    a.len,
                )?;
                let mut out = BitsBuf::with_capacity(rows.len());
                for &r in rows {
                    out.push(src.get(check(r)?));
                }
                vec![vbuf, Cow::Owned(out.bytes)]
            }
            LType::Utf8 | LType::Binary => {
                let (o, d) = take_text::<i32>(a, rows)?;
                vec![vbuf, Cow::Owned(o), Cow::Owned(d)]
            }
            LType::LargeUtf8 | LType::LargeBinary => {
                let (o, d) = take_text::<i64>(a, rows)?;
                vec![vbuf, Cow::Owned(o), Cow::Owned(d)]
            }
            other => {
                return Err(Error::schema(&format!(
                    "the native filter takes no column of type {other:?}"
                )));
            }
        };
        Ok(OutArray {
            len: rows.len(),
            null_count,
            buffers,
            children: Vec::new(),
        })
    }
}

/// An offset type of a text column.
pub trait Offset: bytemuck::Pod + Copy {
    fn to_usize(self) -> Option<usize>;
    fn from_usize(v: usize) -> Option<Self>;
}

impl Offset for i32 {
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }
    fn from_usize(v: usize) -> Option<i32> {
        i32::try_from(v).ok()
    }
}

impl Offset for i64 {
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }
    fn from_usize(v: usize) -> Option<i64> {
        i64::try_from(v).ok()
    }
}

/// Value `i` of a text array: its bytes, checked against the data buffer.
pub fn text_value<'a, O: Offset>(
    offsets: &[O],
    data: &'a [u8],
    i: usize,
) -> Result<&'a [u8], Error> {
    let bad = || Error::layout("a string's offsets are outside its data");
    let (s, e) = (
        offsets.get(i).ok_or_else(bad)?,
        offsets.get(i + 1).ok_or_else(bad)?,
    );
    let (s, e) = (s.to_usize().ok_or_else(bad)?, e.to_usize().ok_or_else(bad)?);
    if s > e {
        return Err(bad());
    }
    data.get(s..e).ok_or_else(bad)
}

fn take_text<O: Offset>(a: &Array<'_>, rows: &[u32]) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let offsets = a.values::<O>(1, a.len + 1)?;
    let data = a.buffers.get(2).copied().unwrap_or(&[]);
    let mut out_o: Vec<O> = Vec::with_capacity(rows.len() + 1);
    let mut out_d = Vec::new();
    out_o.push(O::from_usize(0).ok_or_else(|| Error::layout("an offset"))?);
    for &r in rows {
        if r as usize >= a.len {
            return Err(Error::layout("a row past the array"));
        }
        out_d.extend_from_slice(text_value(offsets, data, r as usize)?);
        out_o.push(
            O::from_usize(out_d.len())
                .ok_or_else(|| Error::layout("a string column past its offsets"))?,
        );
    }
    Ok((bytemuck::cast_slice(&out_o).to_vec(), out_d))
}

/// A text column built value by value.
#[derive(Debug, Default)]
pub struct TextBuf {
    offsets: Vec<i32>,
    data: Vec<u8>,
    valid: BitsBuf,
}

impl TextBuf {
    pub fn new() -> TextBuf {
        TextBuf {
            offsets: vec![0],
            data: Vec::new(),
            valid: BitsBuf::default(),
        }
    }

    pub fn push(&mut self, v: Option<&[u8]>) -> Result<(), Error> {
        if let Some(v) = v {
            self.data.extend_from_slice(v);
        }
        self.valid.push(v.is_some());
        let end = i32::try_from(self.data.len())
            .map_err(|_| Error::layout("a string column past 2 GiB"))?;
        self.offsets.push(end);
        Ok(())
    }

    pub fn finish<'a>(self) -> OutArray<'a> {
        let len = self.valid.len;
        let (null_count, v) = validity_buf(Some(self.valid));
        OutArray {
            len,
            null_count,
            buffers: vec![
                v,
                Cow::Owned(bytemuck::cast_slice(&self.offsets).to_vec()),
                Cow::Owned(self.data),
            ],
            children: Vec::new(),
        }
    }
}

/// The `Type` union's id and table for `ty`.
fn type_obj(ty: &LType) -> Result<(u8, Obj), Error> {
    Ok(match ty {
        LType::Int { bits, signed } => (
            types::INT,
            Obj::new()
                .with(0, Val::I32(i32::from(*bits)))
                .with(1, Val::Bool(*signed)),
        ),
        LType::Float { bits } => (
            types::FLOATING_POINT,
            Obj::new().with(
                0,
                Val::I16(match bits {
                    16 => 0,
                    32 => 1,
                    _ => 2,
                }),
            ),
        ),
        LType::Bool => (types::BOOL, Obj::new()),
        LType::Utf8 => (types::UTF8, Obj::new()),
        LType::LargeUtf8 => (types::LARGE_UTF8, Obj::new()),
        LType::Binary => (types::BINARY, Obj::new()),
        LType::LargeBinary => (types::LARGE_BINARY, Obj::new()),
        LType::Timestamp { unit, tz } => {
            let mut o = Obj::new().with(0, Val::I16(unit.to_fb()));
            if let Some(tz) = tz {
                o = o.with(1, Val::Str(tz.clone()));
            }
            (types::TIMESTAMP, o)
        }
        LType::FixedSizeList { size, .. } => {
            (types::FIXED_SIZE_LIST, Obj::new().with(0, Val::I32(*size)))
        }
        LType::List { .. } => (types::LIST, Obj::new()),
        LType::LargeList { .. } => (types::LARGE_LIST, Obj::new()),
        LType::Struct(_) => (types::STRUCT, Obj::new()),
        LType::Map { sorted, .. } => (types::MAP, Obj::new().with(0, Val::Bool(*sorted))),
        LType::Other(id) => {
            return Err(Error::schema(&format!(
                "an output column of a type the record writer has no table for ({})",
                types::name(*id)
            )));
        }
    })
}

fn field_obj(f: &LField) -> Result<Obj, Error> {
    let (kind, table) = type_obj(&f.ty)?;
    let children = match &f.ty {
        LType::FixedSizeList { child, .. }
        | LType::List { child }
        | LType::LargeList { child }
        | LType::Map { child, .. } => vec![field_obj(child)?],
        LType::Struct(fs) => fs.iter().map(field_obj).collect::<Result<_, _>>()?,
        _ => Vec::new(),
    };
    Ok(Obj::new()
        .with(0, Val::Str(f.name.clone()))
        .with(1, Val::Bool(f.nullable))
        .with(2, Val::U8(kind))
        .with(3, Val::Table(Box::new(table)))
        .with(5, Val::Tables(children)))
}

/// A whole IPC message: the continuation marker, the length and the flatbuffer.
fn message(header_type: u8, header: Obj, body_length: i64) -> Vec<u8> {
    let m = Obj::new()
        .with(0, Val::I16(version::V5))
        .with(1, Val::U8(header_type))
        .with(2, Val::Table(Box::new(header)))
        .with(3, Val::I64(body_length));
    let fb = finish(m);
    let mut out = Vec::with_capacity(8 + fb.len());
    out.extend_from_slice(&CONTINUATION);
    out.extend_from_slice(&(fb.len() as u32).to_le_bytes());
    out.extend_from_slice(&fb);
    out
}

fn flatten<'b, 'a>(a: &'b OutArray<'a>, nodes: &mut Vec<(i64, i64)>, bufs: &mut Vec<&'b [u8]>) {
    nodes.push((a.len as i64, a.null_count as i64));
    for b in &a.buffers {
        bufs.push(b);
    }
    for c in &a.children {
        flatten(c, nodes, bufs);
    }
}

/// The record of `columns` (each with its field) laid out: its framing, and each buffer with
/// the offset it goes to. `record_len` is the framing and the body.
pub struct Laid<'b> {
    pub framing: Vec<u8>,
    pub placed: Vec<(u64, &'b [u8])>,
    pub record_len: u64,
}

pub fn lay_out<'b, 'a>(
    fields: &[LField],
    columns: &'b [OutArray<'a>],
    rows: usize,
) -> Result<Laid<'b>, Error> {
    if fields.len() != columns.len() {
        return Err(Error::layout(
            "an output with fields and columns that differ",
        ));
    }
    let schema = Obj::new().with(
        1,
        Val::Tables(fields.iter().map(field_obj).collect::<Result<_, _>>()?),
    );
    let mut nodes = Vec::new();
    let mut bufs = Vec::new();
    for c in columns {
        if c.len != rows {
            return Err(Error::layout(
                "an output column whose length is not the batch's",
            ));
        }
        flatten(c, &mut nodes, &mut bufs);
    }
    let entries: Vec<(i64, i64)> = bufs.iter().map(|b| (0, b.len() as i64)).collect();
    let rb = Obj::new()
        .with(0, Val::I64(rows as i64))
        .with(1, Val::Pairs(nodes))
        .with(2, Val::Pairs(entries));
    let mut framing = message(header::SCHEMA, schema, 0);
    // A non-zero body length, so the field is written and the layout can set it.
    framing.extend_from_slice(&message(header::RECORD_BATCH, rb, 1));
    let lengths: Vec<u64> = bufs.iter().map(|b| b.len() as u64).collect();
    let at = frame::page_layout(&mut framing, &lengths, PAGE, 0).map_err(Error::Record)?;
    let (_, batch) = moruna_ipc_check::record::stream_parts(&framing).map_err(Error::Record)?;
    let body = frame::body_length(&framing[batch]).map_err(Error::Record)? as u64;
    let record_len = framing.len() as u64 + body;
    Ok(Laid {
        placed: at.into_iter().zip(bufs).collect(),
        framing,
        record_len,
    })
}

/// Writes the record of `columns` into `window`; its length.
pub fn write(
    fields: &[LField],
    columns: &[OutArray<'_>],
    rows: usize,
    window: &mut [u8],
) -> Result<usize, Error> {
    let laid = lay_out(fields, columns, rows)?;
    let len = usize::try_from(laid.record_len).map_err(|_| Error::layout("a record length"))?;
    if len > window.len() {
        return Err(Error::WindowTooSmall {
            need: laid.record_len,
            have: window.len() as u64,
        });
    }
    let f = laid.framing.len();
    window[..f].copy_from_slice(&laid.framing);
    let mut end = f;
    for (at, b) in &laid.placed {
        if b.is_empty() {
            continue;
        }
        let at = *at as usize;
        // The gap since the last buffer: zeros, so no stale byte of the window is in the record.
        window[end..at].fill(0);
        window[at..at + b.len()].copy_from_slice(b);
        end = at + b.len();
    }
    window[end..len].fill(0);
    Ok(len)
}

/// The record of `columns` as a new vector, for a host test or the engine's own use.
pub fn to_vec(fields: &[LField], columns: &[OutArray<'_>], rows: usize) -> Result<Vec<u8>, Error> {
    let laid = lay_out(fields, columns, rows)?;
    let mut out = vec![0u8; laid.record_len as usize];
    write(fields, columns, rows, &mut out)?;
    Ok(out)
}

/// For messages: the type of a field.
pub fn describe(f: &LField) -> String {
    format!("{}: {:?}", f.name, f.ty)
}
