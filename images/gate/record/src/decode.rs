//! An input record read in place: checked by the engine's own record check
//! (`moruna_ipc_check::record::check_record`), then its schema's logical types and its arrays,
//! every buffer a slice of the record.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use moruna_gate_functions::cols::{Bits, Col};
use moruna_ipc_check::fb::{Table, pair};
use moruna_ipc_check::format::{batch, field, header, message, schema, types};
use moruna_ipc_check::record::{self, Need};
use moruna_ipc_check::schema::Type as CheckType;

use crate::Error;
use crate::types::{LField, LType, Unit};

/// One array of a record: its length and null count (its `FieldNode`), its buffers in IPC
/// order, and its children.
#[derive(Clone, Debug)]
pub struct Array<'a> {
    pub len: usize,
    pub null_count: usize,
    pub buffers: Vec<&'a [u8]>,
    pub children: Vec<Array<'a>>,
}

impl<'a> Array<'a> {
    /// The validity bitmap, `None` when no value is null.
    pub fn validity(&self) -> Result<Option<Bits<'a>>, Error> {
        match self.buffers.first() {
            Some(b) if self.null_count > 0 && !b.is_empty() => Ok(Some(Bits::new(b, 0, self.len)?)),
            _ if self.null_count > 0 => Err(Error::layout("nulls counted and no validity bitmap")),
            _ => Ok(None),
        }
    }

    /// Buffer `i` as `len` values of `T`.
    pub fn values<T: bytemuck::Pod>(&self, i: usize, len: usize) -> Result<&'a [T], Error> {
        let b = self
            .buffers
            .get(i)
            .ok_or_else(|| Error::layout("a buffer the type has is missing"))?;
        let all: &[T] = bytemuck::try_cast_slice(b)
            .or_else(|_| {
                // A length that is not a whole number of values: the whole ones.
                let whole = b.len() / core::mem::size_of::<T>() * core::mem::size_of::<T>();
                bytemuck::try_cast_slice(&b[..whole])
            })
            .map_err(|e| {
                Error::layout(&format!("a buffer is not aligned for its values: {e:?}"))
            })?;
        all.get(..len).ok_or_else(|| {
            Error::layout(&format!("a buffer holds {} values, not {len}", all.len()))
        })
    }

    /// The fixed-width values with their validity.
    pub fn col<T: bytemuck::Pod>(&self) -> Result<Col<'a, T>, Error> {
        Ok(Col::new(self.values::<T>(1, self.len)?, self.validity()?))
    }
}

/// A record, read.
#[derive(Clone, Debug)]
pub struct Batch<'a> {
    pub rows: usize,
    pub fields: Vec<LField>,
    pub columns: Vec<Array<'a>>,
}

impl<'a> Batch<'a> {
    /// Reads the record `input`: its whole length, as the slot gave it.
    pub fn read(input: &'a [u8]) -> Result<Batch<'a>, Error> {
        let end = match record::framing_need(input).map_err(Error::Record)? {
            Need::Framing(n) if n <= input.len() => n,
            _ => return Err(Error::layout("the record is shorter than its framing")),
        };
        let framing = &input[..end];
        let checked = record::check_record(framing, input.len() as u64).map_err(Error::Record)?;
        let (schema_range, batch_range) = record::stream_parts(framing).map_err(Error::Record)?;
        let fields = read_schema(&framing[schema_range])?;
        let nodes = read_nodes(&framing[batch_range])?;
        if fields.len() != checked.columns.len() {
            return Err(Error::layout(
                "the schema and the record disagree on columns",
            ));
        }
        let rows = usize::try_from(checked.rows).map_err(|_| Error::layout("a row count"))?;
        let mut node = 0usize;
        let mut columns = Vec::with_capacity(fields.len());
        for ((f, column), check) in fields.iter().zip(&checked.columns).zip(&checked.fields) {
            let bufs: Vec<&[u8]> = column
                .buffers
                .iter()
                .map(|&(at, len)| slice(input, at, len))
                .collect::<Result<_, _>>()?;
            let mut b = 0usize;
            let start = node;
            let array = build(&f.ty, &check.kind, &nodes, &mut node, &bufs, &mut b)?;
            if b != bufs.len() {
                return Err(Error::layout(&format!(
                    "column {:?} has {} buffers and its type takes {b}",
                    f.name,
                    bufs.len()
                )));
            }
            let _ = start;
            if array.len != rows {
                return Err(Error::layout(&format!(
                    "column {:?} has {} values and the batch {rows} rows",
                    f.name, array.len
                )));
            }
            columns.push(array);
        }
        if node != nodes.len() {
            return Err(Error::layout("the record has nodes no column uses"));
        }
        Ok(Batch {
            rows,
            fields,
            columns,
        })
    }

    /// A column by name.
    pub fn column(&self, name: &str) -> Result<(&LField, &Array<'a>), Error> {
        self.fields
            .iter()
            .zip(&self.columns)
            .find(|(f, _)| f.name == name)
            .ok_or_else(|| Error::schema(&format!("the batch has no column {name:?}")))
    }

    /// An `int64` column, or a timestamp column as its raw values.
    pub fn ints(&self, name: &str) -> Result<Col<'a, i64>, Error> {
        let (f, a) = self.column(name)?;
        match f.ty {
            LType::Int {
                bits: 64,
                signed: true,
            }
            | LType::Timestamp { .. } => a.col::<i64>(),
            ref other => Err(Error::schema(&format!(
                "column {name:?} is {other:?}, not int64 or a timestamp"
            ))),
        }
    }

    /// An `int64` column only.
    pub fn int64(&self, name: &str) -> Result<Col<'a, i64>, Error> {
        let (f, a) = self.column(name)?;
        if f.ty != LType::I64 {
            return Err(Error::schema(&format!(
                "column {name:?} is {:?}, not int64",
                f.ty
            )));
        }
        a.col::<i64>()
    }

    /// A `double` column.
    pub fn floats(&self, name: &str) -> Result<Col<'a, f64>, Error> {
        let (f, a) = self.column(name)?;
        if f.ty != LType::F64 {
            return Err(Error::schema(&format!(
                "column {name:?} is {:?}, not double",
                f.ty
            )));
        }
        a.col::<f64>()
    }
}

fn slice(input: &[u8], at: u64, len: u64) -> Result<&[u8], Error> {
    let at = usize::try_from(at).map_err(|_| Error::layout("a buffer offset"))?;
    let len = usize::try_from(len).map_err(|_| Error::layout("a buffer length"))?;
    input
        .get(
            at..at
                .checked_add(len)
                .ok_or_else(|| Error::layout("a buffer's end"))?,
        )
        .ok_or_else(|| Error::layout("a buffer past the record"))
}

/// The array of type `ty` at node `*node`, taking its buffers from `bufs` at `*b`.
fn build<'a>(
    ty: &LType,
    check: &CheckType,
    nodes: &[(usize, usize)],
    node: &mut usize,
    bufs: &[&'a [u8]],
    b: &mut usize,
) -> Result<Array<'a>, Error> {
    let (len, null_count) = *nodes
        .get(*node)
        .ok_or_else(|| Error::layout("the record has fewer nodes than its schema"))?;
    *node += 1;
    let mut take = |n: usize| -> Result<Vec<&'a [u8]>, Error> {
        let got = bufs
            .get(*b..*b + n)
            .ok_or_else(|| Error::layout("the record has fewer buffers than its schema"))?;
        *b += n;
        Ok(got.to_vec())
    };
    let mut children = Vec::new();
    let buffers = match (ty, check) {
        (LType::Int { .. } | LType::Float { .. } | LType::Timestamp { .. } | LType::Bool, _) => {
            take(2)?
        }
        (LType::Utf8 | LType::LargeUtf8 | LType::Binary | LType::LargeBinary, _) => take(3)?,
        (LType::FixedSizeList { child, .. }, CheckType::FixedSizeList { child: c, .. }) => {
            let own = take(1)?;
            children.push(build(&child.ty, &c.kind, nodes, node, bufs, b)?);
            own
        }
        (
            LType::List { child } | LType::LargeList { child } | LType::Map { child, .. },
            CheckType::List { child: c, .. },
        ) => {
            let own = take(2)?;
            children.push(build(&child.ty, &c.kind, nodes, node, bufs, b)?);
            own
        }
        (LType::Struct(fs), CheckType::Struct(cs)) if fs.len() == cs.len() => {
            let own = take(1)?;
            for (f, c) in fs.iter().zip(cs) {
                children.push(build(&f.ty, &c.kind, nodes, node, bufs, b)?);
            }
            own
        }
        (_, other) => {
            // A type no gate function reads: skip its nodes and buffers.
            *node += nodes_in(other) - 1;
            take(buffers_in(other))?
        }
    };
    Ok(Array {
        len,
        null_count,
        buffers,
        children,
    })
}

fn nodes_in(t: &CheckType) -> usize {
    match t {
        CheckType::List { child, .. }
        | CheckType::ListView { child, .. }
        | CheckType::FixedSizeList { child, .. } => 1 + nodes_in(&child.kind),
        CheckType::Struct(cs) | CheckType::Union { children: cs, .. } => {
            1 + cs.iter().map(|c| nodes_in(&c.kind)).sum::<usize>()
        }
        CheckType::RunEndEncoded { run_ends, values } => {
            1 + nodes_in(&run_ends.kind) + nodes_in(&values.kind)
        }
        _ => 1,
    }
}

fn buffers_in(t: &CheckType) -> usize {
    match t {
        CheckType::Null => 0,
        CheckType::Boolean
        | CheckType::Primitive(_)
        | CheckType::FixedSizeBinary(_)
        | CheckType::Dictionary { .. } => 2,
        CheckType::Text { .. } => 3,
        CheckType::View => 2,
        CheckType::List { child, .. } => 2 + buffers_in(&child.kind),
        CheckType::ListView { child, .. } => 3 + buffers_in(&child.kind),
        CheckType::FixedSizeList { child, .. } => 1 + buffers_in(&child.kind),
        CheckType::Struct(cs) => 1 + cs.iter().map(|c| buffers_in(&c.kind)).sum::<usize>(),
        CheckType::Union { dense, children } => {
            1 + usize::from(*dense) + children.iter().map(|c| buffers_in(&c.kind)).sum::<usize>()
        }
        CheckType::RunEndEncoded { run_ends, values } => {
            buffers_in(&run_ends.kind) + buffers_in(&values.kind)
        }
    }
}

fn root(message: &[u8]) -> Result<Table<'_>, Error> {
    let len = moruna_ipc_check::frame::message_len(message).map_err(Error::Record)?;
    Table::root(&message[8..8 + len]).map_err(Error::Record)
}

fn read_nodes(message: &[u8]) -> Result<Vec<(usize, usize)>, Error> {
    let m = root(message)?;
    let rb = m
        .table(message::HEADER)
        .map_err(Error::Record)?
        .ok_or_else(|| Error::layout("the record batch message has no header"))?;
    let Some(v) = rb
        .vector(batch::NODES, batch::ENTRY)
        .map_err(Error::Record)?
    else {
        return Ok(Vec::new());
    };
    (0..v.len())
        .map(|i| {
            let (len, nulls) = v
                .element(i)
                .and_then(pair)
                .ok_or_else(|| Error::layout("a field node past the message"))?;
            let len = usize::try_from(len).map_err(|_| Error::layout("a negative length"))?;
            let nulls =
                usize::try_from(nulls).map_err(|_| Error::layout("a negative null count"))?;
            Ok((len, nulls))
        })
        .collect()
}

fn read_schema(message: &[u8]) -> Result<Vec<LField>, Error> {
    let m = root(message)?;
    if m.u8(message::HEADER_TYPE, header::NONE)
        .map_err(Error::Record)?
        != header::SCHEMA
    {
        return Err(Error::layout("the first message is not a schema"));
    }
    let s = m
        .table(message::HEADER)
        .map_err(Error::Record)?
        .ok_or_else(|| Error::layout("the schema message has no schema"))?;
    let v = s
        .vector(schema::FIELDS, 4)
        .map_err(Error::Record)?
        .ok_or_else(|| Error::layout("the schema has no fields"))?;
    (0..v.len())
        .map(|i| read_field(&v.table(i).map_err(Error::Record)?))
        .collect()
}

fn read_field(t: &Table<'_>) -> Result<LField, Error> {
    let r = Error::Record;
    let name = String::from(t.str(field::NAME).map_err(r)?.unwrap_or_default());
    let nullable = t.bool(field::NULLABLE, false).map_err(r)?;
    let kind = t.u8(field::TYPE_TYPE, types::NONE).map_err(r)?;
    let ty_table = t.table(field::TYPE).map_err(r)?;
    let mut children = Vec::new();
    if let Some(v) = t.vector(field::CHILDREN, 4).map_err(r)? {
        for i in 0..v.len() {
            children.push(read_field(&v.table(i).map_err(r)?)?);
        }
    }
    let one_child = |children: Vec<LField>| -> Result<Box<LField>, Error> {
        children
            .into_iter()
            .next()
            .map(Box::new)
            .ok_or_else(|| Error::layout("a list type without its child"))
    };
    let ty = match (kind, ty_table) {
        (types::INT, Some(tt)) => LType::Int {
            bits: u8::try_from(tt.i32(types::INT_BIT_WIDTH, 0).map_err(r)?)
                .map_err(|_| Error::layout("an integer width"))?,
            signed: tt.bool(1, false).map_err(r)?,
        },
        (types::FLOATING_POINT, Some(tt)) => LType::Float {
            bits: match tt.i16(types::PRECISION, 0).map_err(r)? {
                0 => 16,
                1 => 32,
                _ => 64,
            },
        },
        (types::BOOL, _) => LType::Bool,
        (types::UTF8, _) => LType::Utf8,
        (types::LARGE_UTF8, _) => LType::LargeUtf8,
        (types::BINARY, _) => LType::Binary,
        (types::LARGE_BINARY, _) => LType::LargeBinary,
        (types::TIMESTAMP, Some(tt)) => LType::Timestamp {
            unit: Unit::from_fb(tt.i16(types::UNIT, 0).map_err(r)?)
                .ok_or_else(|| Error::layout("a timestamp unit"))?,
            tz: tt.str(1).map_err(r)?.map(String::from),
        },
        (types::FIXED_SIZE_LIST, Some(tt)) => LType::FixedSizeList {
            size: tt.i32(types::WIDTH, 0).map_err(r)?,
            child: one_child(children)?,
        },
        (types::LIST, _) => LType::List {
            child: one_child(children)?,
        },
        (types::LARGE_LIST, _) => LType::LargeList {
            child: one_child(children)?,
        },
        (types::STRUCT, _) => LType::Struct(children),
        (types::MAP, tt) => LType::Map {
            child: one_child(children)?,
            sorted: match tt {
                Some(tt) => tt.bool(0, false).map_err(r)?,
                None => false,
            },
        },
        (other, _) => LType::Other(other),
    };
    Ok(LField { name, nullable, ty })
}
