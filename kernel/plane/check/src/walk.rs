//! The walk arrow-ipc's record batch decoder makes over a batch's nodes and buffers, field by
//! field in the same order, checking each thing it relies on (contracts CT-I13, e.8).
//!
//! arrow-ipc's decoder panics, where it could return an error, on a buffer outside the body, a
//! validity bitmap shorter than its column, a union whose type ids or offsets do not fit, a view
//! column's buffer counts left over, and a few more; each is refused here with a sentence naming
//! the column. For a compressed batch the same walk plans the inflation: each buffer is handed
//! to the caller's [`Plan`] with the size its column implies (a [`Rule`]), which the plan holds
//! the declared size to before anything is allocated.

use alloc::{collections::VecDeque, format, string::String, vec::Vec};
use core::ops::Range;

use crate::fb::{Table, pair};
use crate::format::{batch, version};
use crate::schema::{Field, Type};

/// One node of a record batch: its length and null count, both non-negative.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub len: usize,
    pub nulls: usize,
}

/// One buffer of a record batch: where it starts in the body and how long it is. For a
/// compressed batch the length is the uncompressed one, which the plan has already held to the
/// size the message implies, and `slot` names the buffer in the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: usize,
    pub len: usize,
    pub slot: usize,
}

/// The size a buffer must have, from the message and what is already inflated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// No size is stated: the ceiling bounds it.
    Free,
    /// Exactly this many bytes.
    Exact(u64),
    /// A validity bitmap of `len` bits, or none.
    Validity(usize),
    /// The offsets of a list: `len + 1` of `width` bytes, or none for no values.
    Offsets { len: usize, width: usize },
    /// The offsets of a string or binary column: as `Offsets`, inflated and checked, and their
    /// last states the size of the data after them.
    Text { len: usize, width: usize },
    /// The data of the string or binary column whose offsets are in `slot`.
    Data { offsets: usize },
}

impl Rule {
    /// `len` values of `width` bytes.
    pub fn values(len: usize, width: usize) -> Rule {
        Rule::Exact((len as u64).saturating_mul(width as u64))
    }

    /// The values buffer of a column of `kind` and `len` values.
    pub fn of_values(kind: &Type, len: usize) -> Rule {
        match kind {
            Type::Boolean => Rule::Exact((len as u64).div_ceil(8)),
            Type::FixedSizeBinary(width) => {
                Rule::values(len, usize::try_from(*width).unwrap_or(usize::MAX))
            }
            other => other
                .primitive_width()
                .map_or(Rule::Free, |width| Rule::values(len, width)),
        }
    }
}

/// What a compressed batch's inflation does with each buffer the walk reaches: hold its declared
/// size to `rule` and to what is left of its ceiling, and plan it (moruna-kernel's `Packed`).
pub trait Plan {
    fn take(
        &mut self,
        column: &str,
        what: &'static str,
        span: Span,
        rule: Rule,
    ) -> Result<Span, String>;
}

/// The body a batch is read over: its length, and the address its first byte will be read at
/// (for the alignment arrow assumes of a dense union's offsets).
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub len: usize,
    pub addr: usize,
}

/// What a walk saw: every buffer in walk order, and for each top-level column the range of
/// `spans` that belongs to it.
#[derive(Clone, Debug, Default)]
pub struct Walked {
    pub spans: Vec<Span>,
    pub columns: Vec<Range<usize>>,
}

/// Walk `batch` (a `RecordBatch` table, metadata `version`) over `body` against `fields`,
/// refusing what arrow-ipc's decoder would panic on. A compressed batch needs its `plan`; a
/// batch that sets `compression` and is given none is refused.
pub fn walk(
    batch: Table<'_>,
    body: Body,
    version: i16,
    fields: &[Field],
    plan: Option<&mut dyn Plan>,
) -> Result<Walked, String> {
    if batch.table(batch::COMPRESSION)?.is_some() && plan.is_none() {
        return Err(String::from(
            "the record batch is compressed and its reader inflates nothing",
        ));
    }
    let length = batch.i64(batch::LENGTH, 0)?;
    if length < 0 {
        return Err(format!(
            "the record batch has a negative row count ({length})"
        ));
    }
    let nodes = batch
        .vector(batch::NODES, batch::ENTRY)?
        .ok_or("the record batch has no field nodes")?;
    let buffers = batch
        .vector(batch::BUFFERS, batch::ENTRY)?
        .ok_or("the record batch has no buffers")?;
    let mut variadic = VecDeque::new();
    if let Some(counts) = batch.vector(batch::VARIADIC, 8)? {
        for i in 0..counts.len() {
            variadic.push_back(counts.i64(i).ok_or("a variadic count is truncated")?);
        }
    }
    let mut walk = Walk {
        nodes: (0..nodes.len())
            .filter_map(|i| nodes.element(i).and_then(pair))
            .collect(),
        buffers: (0..buffers.len())
            .filter_map(|i| buffers.element(i).and_then(pair))
            .collect(),
        variadic,
        body,
        version,
        plan,
        walked: Walked::default(),
    };
    for field in fields {
        let start = walk.walked.spans.len();
        walk.array(field)?;
        let end = walk.walked.spans.len();
        walk.walked.columns.push(start..end);
    }
    if !walk.variadic.is_empty() {
        return Err(format!(
            "the record batch has {} variadic buffer counts that no view column uses",
            walk.variadic.len()
        ));
    }
    Ok(walk.walked)
}

struct Walk<'p> {
    nodes: VecDeque<(i64, i64)>,
    buffers: VecDeque<(i64, i64)>,
    variadic: VecDeque<i64>,
    body: Body,
    version: i16,
    plan: Option<&'p mut dyn Plan>,
    walked: Walked,
}

impl Walk<'_> {
    /// One column, its children, and, for a primitive column, the span of its values.
    fn array(&mut self, field: &Field) -> Result<Option<Span>, String> {
        match &field.kind {
            Type::Text { width } => {
                let width = *width;
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let offsets = self.buffer(
                    field,
                    "offsets",
                    Rule::Text {
                        len: node.len,
                        width,
                    },
                )?;
                self.buffer(
                    field,
                    "data",
                    Rule::Data {
                        offsets: offsets.slot,
                    },
                )?;
                whole(field, offsets, width, "offsets")?;
                nulls(field, node, validity)?;
                Ok(None)
            }
            Type::View => {
                let count = self.variadic.pop_front().ok_or_else(|| {
                    format!("column {:?} is a view with no variadic count", field.name)
                })?;
                let count = usize::try_from(count).map_err(|_| {
                    format!(
                        "column {:?} has a negative variadic count ({count})",
                        field.name
                    )
                })?;
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let views = self.buffer(field, "views", Rule::values(node.len, 16))?;
                for _ in 0..count {
                    self.buffer(field, "view data", Rule::Free)?;
                }
                whole(field, views, 16, "views")?;
                nulls(field, node, validity)?;
                Ok(None)
            }
            Type::List { width, child } => {
                let width = *width;
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let offsets = self.buffer(
                    field,
                    "offsets",
                    Rule::Offsets {
                        len: node.len,
                        width,
                    },
                )?;
                whole(field, offsets, width, "offsets")?;
                nulls(field, node, validity)?;
                self.array(child)?;
                Ok(None)
            }
            Type::ListView { width, child } => {
                let width = *width;
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let offsets = self.buffer(field, "offsets", Rule::values(node.len, width))?;
                whole(field, offsets, width, "offsets")?;
                let sizes = self.buffer(field, "sizes", Rule::values(node.len, width))?;
                whole(field, sizes, width, "sizes")?;
                nulls(field, node, validity)?;
                self.array(child)?;
                Ok(None)
            }
            Type::FixedSizeList { size, child } => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                nulls(field, node, validity)?;
                // arrow's validation multiplies the two with `expect`.
                let size = usize::try_from(*size).unwrap_or(0);
                if node.len.checked_mul(size).is_none() {
                    return Err(format!(
                        "column {:?} has {} lists of {size} values, more than can be counted",
                        field.name, node.len
                    ));
                }
                self.array(child)?;
                Ok(None)
            }
            Type::Struct(children) => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                nulls(field, node, validity)?;
                for child in children {
                    self.array(child)?;
                }
                Ok(None)
            }
            Type::RunEndEncoded { run_ends, values } => {
                self.node(field)?;
                // The run ends are a primitive column (validity, then values) that arrow's
                // validation reads as typed values.
                let ends = self.array(run_ends)?;
                if let Some(ends) = ends {
                    let width = run_ends.kind.primitive_width().unwrap_or(1);
                    whole(run_ends, ends, width, "values")?;
                }
                self.array(values)?;
                Ok(None)
            }
            Type::Dictionary { key_width, .. } => {
                let width = *key_width;
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let keys = self.buffer(field, "keys", Rule::values(node.len, width))?;
                whole(field, keys, width, "keys")?;
                nulls(field, node, validity)?;
                Ok(None)
            }
            Type::Union { dense, children } => {
                self.union(field, children, *dense)?;
                Ok(None)
            }
            Type::Null => {
                self.node(field)?;
                Ok(None)
            }
            other => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let values = self.buffer(field, "values", Rule::of_values(other, node.len))?;
                nulls(field, node, validity)?;
                Ok(Some(values))
            }
        }
    }

    /// A union's type ids and offsets are read as typed buffers of the union's length, without
    /// the bounds or alignment checks the other arrays get.
    fn union(&mut self, field: &Field, children: &[Field], dense: bool) -> Result<(), String> {
        let node = self.node(field)?;
        if self.version < version::V5 {
            self.buffer(field, "validity", Rule::Validity(node.len))?;
        }
        let type_ids = self.buffer(field, "type ids", Rule::values(node.len, 1))?;
        if type_ids.len < node.len {
            return Err(format!(
                "column {:?} is a union of {} values with {} bytes of type ids",
                field.name, node.len, type_ids.len
            ));
        }
        if dense {
            let offsets = self.buffer(field, "offsets", Rule::values(node.len, 4))?;
            let need = node.len.checked_mul(4);
            if need.is_none_or(|need| offsets.len < need) {
                return Err(format!(
                    "column {:?} is a dense union of {} values with {} bytes of offsets",
                    field.name, node.len, offsets.len
                ));
            }
            // An inflated body places every buffer at a multiple of 64 from an arena buffer.
            if self.plan.is_none() {
                let address = self.body.addr.wrapping_add(offsets.offset);
                if !address.is_multiple_of(4) {
                    return Err(format!(
                        "column {:?} is a dense union whose offsets are not 4-byte aligned",
                        field.name
                    ));
                }
            }
        }
        for child in children {
            self.array(child)?;
        }
        Ok(())
    }

    fn node(&mut self, field: &Field) -> Result<Node, String> {
        let (len, nulls) = self.nodes.pop_front().ok_or_else(|| {
            format!(
                "column {:?} has no field node; the record batch has too few",
                field.name
            )
        })?;
        match (usize::try_from(len), usize::try_from(nulls)) {
            (Ok(len), Ok(nulls)) if nulls <= len => Ok(Node { len, nulls }),
            _ => Err(format!(
                "column {:?} has a field node of length {len} with {nulls} nulls",
                field.name
            )),
        }
    }

    /// The next buffer of the message: inside the body, and for a compressed batch of the size
    /// `rule` says its column needs (`what` names it in a refusal).
    fn buffer(&mut self, field: &Field, what: &'static str, rule: Rule) -> Result<Span, String> {
        let (offset, len) = self.buffers.pop_front().ok_or_else(|| {
            format!(
                "column {:?} has no buffer left; the record batch has too few",
                field.name
            )
        })?;
        let span = match (usize::try_from(offset), usize::try_from(len)) {
            (Ok(offset), Ok(len))
                if offset
                    .checked_add(len)
                    .is_some_and(|end| end <= self.body.len) =>
            {
                Span {
                    offset,
                    len,
                    slot: usize::MAX,
                }
            }
            _ => {
                return Err(format!(
                    "column {:?} has a buffer of {len} bytes at {offset}, outside the body of {} bytes",
                    field.name, self.body.len
                ));
            }
        };
        let span = match &mut self.plan {
            Some(plan) => plan.take(&field.name, what, span, rule)?,
            None => span,
        };
        self.walked.spans.push(span);
        Ok(span)
    }
}

/// A buffer arrow's validation reads as a slice of `width`-byte values, which it asserts it
/// holds a whole number of.
fn whole(field: &Field, span: Span, width: usize, what: &str) -> Result<(), String> {
    if !span.len.is_multiple_of(width) {
        return Err(format!(
            "column {:?} has {what} of {} bytes, not a whole number of {width}-byte values",
            field.name, span.len
        ));
    }
    Ok(())
}

/// A column with nulls has its validity bitmap read as `len` bits, which arrow asserts it holds.
fn nulls(field: &Field, node: Node, validity: Span) -> Result<(), String> {
    if node.nulls > 0 && validity.len.saturating_mul(8) < node.len {
        return Err(format!(
            "column {:?} has {} values and a validity bitmap of {} bytes",
            field.name, node.len, validity.len
        ));
    }
    Ok(())
}
