//! An Arrow IPC message checked before arrow-ipc reads it (contracts CT-I13, e.8).
//!
//! arrow-ipc panics, where it could return an error, on some messages it cannot read: its schema
//! converter (`arrow::ipc::convert::fb_to_schema`) on a type it cannot map, and its record batch
//! decoder on a buffer outside the body, a validity bitmap shorter than its column, a union whose
//! type ids or offsets do not fit, a view column's buffer counts left over, a dictionary message
//! with no data, a header type with no header, and a few more (each named where it is checked
//! below). Every IPC byte Moruna reads is untrusted: a `.wasm` or a `.pex` function's output, a
//! Parquet footer, a record read back from disk. So each message is checked here for exactly what
//! arrow-ipc relies on, and one it would panic on is refused with a sentence naming what is wrong.
//! This is the one check; every decode site calls it, and there is no second copy.
//!
//! A compressed record batch (`LZ4_FRAME` or `ZSTD`, method `BUFFER`) is inflated here, because
//! arrow-ipc allocates the decompressed length a buffer declares before it decompresses, so a few
//! bytes can ask for any amount of memory. Every buffer's expected size is worked out from the
//! message alone (its column's length and type, or, for the data of a string or binary column,
//! the last offset of its offsets, which are inflated first and checked), the declared size must
//! be that size, and the sizes together must fit the ceiling the caller passed, all before the
//! first byte is allocated for the batch. The batch is then inflated into one buffer of the
//! arena, bounded at each buffer's expected size, and arrow-ipc is handed an uncompressed
//! message and body exactly as for an uncompressed batch (`inflate.rs`).

use std::collections::VecDeque;

use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::{DataType, Field, Schema, UnionFields, UnionMode};
use arrow::ipc;

use super::inflate::{Inflate, Inflated, Packed, Rule, Wrap};

/// The header of a message that passed `check`, ready for arrow-ipc.
#[derive(Debug)]
pub enum Checked<'m> {
    /// A schema message: `fb_to_schema` converts it without panicking.
    Schema(ipc::Schema<'m>),
    /// A record batch message: arrow-ipc's decoder reads it over the body without panicking.
    Batch(Passed<'m>),
    /// A dictionary batch message: `read_dictionary` reads it over the body without panicking.
    Dictionary(Passed<'m>),
    /// A message with no header, which a stream reader skips.
    Empty,
}

/// A record batch or dictionary message that passed the check, with what arrow-ipc's decoders
/// read it over: the message and body it came with, or, for a compressed batch, the uncompressed
/// ones the check made of it.
#[derive(Debug)]
pub struct Passed<'m> {
    message: ipc::Message<'m>,
    inflated: Option<Inflated>,
}

impl Passed<'_> {
    /// True when the batch was compressed and `with_batch` hands out the inflated copy.
    pub fn is_inflated(&self) -> bool {
        self.inflated.is_some()
    }

    /// Run `read` over the record batch header and body arrow-ipc decodes: `body` is the body
    /// the message came with, which is used when the batch was not compressed.
    pub fn with_batch<R>(
        &self,
        body: &ArrowBuffer,
        read: impl FnOnce(ipc::RecordBatch<'_>, &ArrowBuffer) -> R,
    ) -> Result<R, String> {
        match &self.inflated {
            None => {
                let batch = self
                    .message
                    .header_as_record_batch()
                    .ok_or("a record batch message has no record batch")?;
                Ok(read(batch, body))
            }
            Some(inflated) => {
                let message = inflated.message()?;
                let batch = message
                    .header_as_record_batch()
                    .ok_or("an inflated record batch message has no record batch")?;
                Ok(read(batch, inflated.body()))
            }
        }
    }

    /// Run `read` over the dictionary batch header and body arrow-ipc decodes, as `with_batch`
    /// does for a record batch.
    pub fn with_dictionary<R>(
        &self,
        body: &ArrowBuffer,
        read: impl FnOnce(ipc::DictionaryBatch<'_>, &ArrowBuffer) -> R,
    ) -> Result<R, String> {
        match &self.inflated {
            None => {
                let dictionary = self
                    .message
                    .header_as_dictionary_batch()
                    .ok_or("a dictionary message has no dictionary batch")?;
                Ok(read(dictionary, body))
            }
            Some(inflated) => {
                let message = inflated.message()?;
                let dictionary = message
                    .header_as_dictionary_batch()
                    .ok_or("an inflated dictionary message has no dictionary batch")?;
                Ok(read(dictionary, inflated.body()))
            }
        }
    }
}

/// Refuse, naming the fault, a message arrow-ipc would panic on (CT-I13), and inflate a
/// compressed batch within `inflate`'s ceiling (e.8).
///
/// `body` is the message's body as the decoder will read it, at the address it will be read
/// from; `schema` is the stream's schema once its schema message has been read (as
/// `fb_to_schema` converted it), which a record batch or dictionary message is read against.
pub fn check<'m>(
    message: &ipc::Message<'m>,
    body: &ArrowBuffer,
    schema: Option<&Schema>,
    inflate: &Inflate<'_>,
) -> Result<Checked<'m>, String> {
    let version = message.version();
    match message.header_type() {
        ipc::MessageHeader::Schema => {
            let fb = message
                .header_as_schema()
                .ok_or("a schema message has no schema")?;
            check_schema(fb)?;
            Ok(Checked::Schema(fb))
        }
        ipc::MessageHeader::RecordBatch => {
            let batch = message
                .header_as_record_batch()
                .ok_or("a record batch message has no record batch")?;
            let schema = schema.ok_or("a record batch comes before the schema")?;
            let packed = Walk::new(batch, body.as_slice(), version, inflate)?
                .columns(schema.fields().iter().map(|f| f.as_ref()))?;
            let inflated = packed
                .map(|packed| packed.finish(batch, version, Wrap::Batch))
                .transpose()?;
            Ok(Checked::Batch(Passed {
                message: *message,
                inflated,
            }))
        }
        ipc::MessageHeader::DictionaryBatch => {
            let dictionary = message
                .header_as_dictionary_batch()
                .ok_or("a dictionary message has no dictionary batch")?;
            let id = dictionary.id();
            let schema =
                schema.ok_or_else(|| format!("dictionary {id} comes before the schema"))?;
            let data = dictionary
                .data()
                .ok_or_else(|| format!("dictionary {id} has no record batch"))?;
            // The field arrow-ipc reads the dictionary's values as: the first field with this id.
            #[allow(deprecated)]
            let fields = schema.fields_with_dict_id(id);
            let value = match fields.first().map(|f| f.data_type()) {
                Some(DataType::Dictionary(_, value)) => value.as_ref().clone(),
                _ => return Err(format!("dictionary {id} is not in the schema")),
            };
            let field = Field::new("", value, true);
            let packed = Walk::new(data, body.as_slice(), version, inflate)?
                .columns(std::iter::once(&field))?;
            let inflated = packed
                .map(|packed| {
                    let wrap = Wrap::Dictionary {
                        id,
                        is_delta: dictionary.isDelta(),
                    };
                    packed.finish(data, version, wrap)
                })
                .transpose()?;
            Ok(Checked::Dictionary(Passed {
                message: *message,
                inflated,
            }))
        }
        ipc::MessageHeader::NONE => Ok(Checked::Empty),
        other => Err(format!(
            "a {other:?} message, which an Arrow IPC stream does not carry"
        )),
    }
}

/// A schema `fb_to_schema` converts without panicking, whose fields arrow-ipc's decoder can
/// build arrays of.
pub(super) fn check_schema(schema: ipc::Schema<'_>) -> Result<(), String> {
    let fields = schema.fields().ok_or("the schema has no field list")?;
    let big_endian = schema.endianness() == ipc::Endianness::Big;
    for field in fields {
        if big_endian && field.type_type() == ipc::Type::Decimal {
            return Err(format!("field {:?} is a big-endian decimal", name(&field)));
        }
        check_field(&field)?;
    }
    Ok(())
}

/// One field and the children the conversion recurses into.
fn check_field(field: &ipc::Field<'_>) -> Result<(), String> {
    let fault = |what: String| format!("field {:?} {what}", name(field));
    if let Some(dictionary) = field.dictionary() {
        let index = dictionary
            .indexType()
            .ok_or_else(|| fault("is dictionary encoded with no index type".into()))?;
        bits(index.bitWidth())
            .ok_or_else(|| fault(format!("has a {} bit dictionary index", index.bitWidth())))?;
    }
    let children = field.children();
    let count = children.map(|c| c.len());
    let kind = field.type_type();
    known(field, count).ok_or_else(|| {
        fault(format!(
            "has a type the Arrow IPC reader cannot convert ({kind:?}, {} children)",
            count.unwrap_or(0)
        ))
    })?;
    // A map's entries are a struct of a key and a value: arrow builds a `MapArray` with
    // `expect` on exactly that.
    if kind == ipc::Type::Map {
        let entries = children.map(|c| c.get(0));
        let pair = entries.is_some_and(|e| {
            e.type_type() == ipc::Type::Struct_ && e.children().map(|c| c.len()) == Some(2)
        });
        if !pair {
            return Err(fault(
                "is a map whose entries are not a struct of two fields".into(),
            ));
        }
    }
    let nested = matches!(
        kind,
        ipc::Type::List
            | ipc::Type::LargeList
            | ipc::Type::ListView
            | ipc::Type::LargeListView
            | ipc::Type::FixedSizeList
            | ipc::Type::Struct_
            | ipc::Type::RunEndEncoded
            | ipc::Type::Map
            | ipc::Type::Union
    );
    if nested {
        for child in children.into_iter().flatten() {
            check_field(&child)?;
        }
    }
    Ok(())
}

/// `Some` when `fb_to_schema` maps this field's type without panicking and arrow can lay out
/// arrays of it: the type table the type id names is present, its enums and widths are ones it
/// knows, and a nested type has the number of children it takes.
fn known(field: &ipc::Field<'_>, children: Option<usize>) -> Option<()> {
    use ipc::{DateUnit, IntervalUnit, Precision, TimeUnit, Type, UnionMode};
    let unit = |u: TimeUnit| {
        matches!(
            u,
            TimeUnit::SECOND | TimeUnit::MILLISECOND | TimeUnit::MICROSECOND | TimeUnit::NANOSECOND
        )
    };
    let ok = |holds: bool| holds.then_some(());
    match field.type_type() {
        Type::Null
        | Type::Bool
        | Type::Binary
        | Type::BinaryView
        | Type::LargeBinary
        | Type::Utf8
        | Type::Utf8View
        | Type::LargeUtf8
        | Type::Struct_ => Some(()),
        Type::Int => bits(field.type_as_int()?.bitWidth()),
        // arrow's buffer layout converts the width with `unwrap`.
        Type::FixedSizeBinary => ok(field.type_as_fixed_size_binary()?.byteWidth() >= 0),
        Type::FloatingPoint => ok(matches!(
            field.type_as_floating_point()?.precision(),
            Precision::HALF | Precision::SINGLE | Precision::DOUBLE
        )),
        Type::Date => ok(matches!(
            field.type_as_date()?.unit(),
            DateUnit::DAY | DateUnit::MILLISECOND
        )),
        Type::Time => {
            let time = field.type_as_time()?;
            ok(matches!(
                (time.bitWidth(), time.unit()),
                (32, TimeUnit::SECOND | TimeUnit::MILLISECOND)
                    | (64, TimeUnit::MICROSECOND | TimeUnit::NANOSECOND)
            ))
        }
        Type::Timestamp => ok(unit(field.type_as_timestamp()?.unit())),
        Type::Duration => ok(unit(field.type_as_duration()?.unit())),
        Type::Interval => ok(matches!(
            field.type_as_interval()?.unit(),
            IntervalUnit::YEAR_MONTH | IntervalUnit::DAY_TIME | IntervalUnit::MONTH_DAY_NANO
        )),
        Type::List | Type::LargeList | Type::ListView | Type::LargeListView => {
            ok(children == Some(1))
        }
        Type::FixedSizeList => ok(field.type_as_fixed_size_list().is_some() && children == Some(1)),
        Type::RunEndEncoded => ok(children == Some(2)),
        Type::Map => ok(field.type_as_map().is_some() && children == Some(1)),
        Type::Decimal => {
            let decimal = field.type_as_decimal()?;
            ok(u8::try_from(decimal.precision()).is_ok()
                && i8::try_from(decimal.scale()).is_ok()
                && matches!(decimal.bitWidth(), 32 | 64 | 128 | 256))
        }
        Type::Union => {
            let union = field.type_as_union()?;
            let count = children.unwrap_or(0);
            let ids = match union.typeIds() {
                // The conversion numbers the children itself, and an `i8` holds 128 of them.
                None => count <= 128,
                Some(ids) => UnionFields::try_new(
                    ids.iter().map(|id| id as i8),
                    (0..count).map(|i| Field::new(i.to_string(), DataType::Null, true)),
                )
                .is_ok(),
            };
            ok(matches!(union.mode(), UnionMode::Dense | UnionMode::Sparse) && ids)
        }
        _ => None,
    }
}

/// `Some` for an integer width the conversion maps.
fn bits(width: i32) -> Option<()> {
    matches!(width, 8 | 16 | 32 | 64).then_some(())
}

/// A field's name as the conversion takes it.
fn name<'a>(field: &ipc::Field<'a>) -> &'a str {
    field.name().unwrap_or_default()
}

/// One node of a record batch: its length and null count, both non-negative.
#[derive(Clone, Copy)]
pub(super) struct Node {
    pub(super) len: usize,
    pub(super) nulls: usize,
}

/// One buffer of a record batch: where it starts in the body and how long it is. For a compressed
/// batch the length is the uncompressed one, which `Packed::take` has already held to the size
/// the message implies, and `slot` names the buffer in the plan of what to inflate.
#[derive(Clone, Copy)]
pub(super) struct Span {
    pub(super) offset: usize,
    pub(super) len: usize,
    pub(super) slot: usize,
}

/// The walk arrow-ipc's `RecordBatchDecoder::create_array` makes over a record batch's nodes and
/// buffers, field by field in the same order, checking each thing it relies on. For a compressed
/// batch the same walk plans the inflation: each buffer is taken with the size its column implies
/// (a `Rule`), which `Packed` holds the declared size to.
struct Walk<'a, 'i> {
    nodes: std::vec::IntoIter<ipc::FieldNode>,
    buffers: std::vec::IntoIter<ipc::Buffer>,
    variadic: VecDeque<i64>,
    body: &'a [u8],
    version: ipc::MetadataVersion,
    packed: Option<Packed<'a, 'i>>,
}

impl<'a, 'i> Walk<'a, 'i> {
    fn new(
        batch: ipc::RecordBatch<'_>,
        body: &'a [u8],
        version: ipc::MetadataVersion,
        inflate: &Inflate<'i>,
    ) -> Result<Walk<'a, 'i>, String> {
        let packed = batch
            .compression()
            .map(|compression| Packed::new(compression, body, version, *inflate))
            .transpose()?;
        if batch.length() < 0 {
            return Err(format!(
                "the record batch has a negative row count ({})",
                batch.length()
            ));
        }
        let nodes = batch.nodes().ok_or("the record batch has no field nodes")?;
        let buffers = batch.buffers().ok_or("the record batch has no buffers")?;
        Ok(Walk {
            nodes: nodes.iter().copied().collect::<Vec<_>>().into_iter(),
            buffers: buffers.iter().copied().collect::<Vec<_>>().into_iter(),
            variadic: batch.variadicBufferCounts().into_iter().flatten().collect(),
            body,
            version,
            packed,
        })
    }

    /// Every top-level column, then what arrow asserts once they are built. The plan of what to
    /// inflate comes back when the batch is compressed.
    fn columns<'f>(
        mut self,
        fields: impl Iterator<Item = &'f Field>,
    ) -> Result<Option<Packed<'a, 'i>>, String> {
        for field in fields {
            self.array(field)?;
        }
        if !self.variadic.is_empty() {
            return Err(format!(
                "the record batch has {} variadic buffer counts that no view column uses",
                self.variadic.len()
            ));
        }
        Ok(self.packed)
    }

    /// One column, its children, and, for a primitive column, the span of its values.
    fn array(&mut self, field: &Field) -> Result<Option<Span>, String> {
        use DataType::*;
        match field.data_type() {
            Utf8 | Binary | LargeBinary | LargeUtf8 => {
                let node = self.node(field)?;
                let width = offset_width(field.data_type());
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
            BinaryView | Utf8View => {
                let count = self.variadic.pop_front().ok_or_else(|| {
                    format!("column {:?} is a view with no variadic count", field.name())
                })?;
                let count = usize::try_from(count).map_err(|_| {
                    format!(
                        "column {:?} has a negative variadic count ({count})",
                        field.name()
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
            List(child) | LargeList(child) | Map(child, _) => {
                let node = self.node(field)?;
                let width = offset_width(field.data_type());
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
            ListView(child) | LargeListView(child) => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let width = offset_width(field.data_type());
                let offsets = self.buffer(field, "offsets", Rule::values(node.len, width))?;
                whole(field, offsets, width, "offsets")?;
                let sizes = self.buffer(field, "sizes", Rule::values(node.len, width))?;
                whole(field, sizes, width, "sizes")?;
                nulls(field, node, validity)?;
                self.array(child)?;
                Ok(None)
            }
            FixedSizeList(child, size) => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                nulls(field, node, validity)?;
                // arrow's validation multiplies the two with `expect`.
                let size = usize::try_from(*size).unwrap_or(0);
                if node.len.checked_mul(size).is_none() {
                    return Err(format!(
                        "column {:?} has {} lists of {size} values, more than can be counted",
                        field.name(),
                        node.len
                    ));
                }
                self.array(child)?;
                Ok(None)
            }
            Struct(children) => {
                let node = self.node(field)?;
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                nulls(field, node, validity)?;
                children.iter().try_for_each(|child| {
                    self.array(child)?;
                    Ok::<(), String>(())
                })?;
                Ok(None)
            }
            RunEndEncoded(run_ends, values) => {
                self.node(field)?;
                // The run ends are a primitive column (validity, then values) that arrow's
                // validation reads as typed values.
                let ends = self.array(run_ends)?;
                if let Some(ends) = ends {
                    let width = run_ends.data_type().primitive_width().unwrap_or(1);
                    whole(run_ends, ends, width, "values")?;
                }
                self.array(values)?;
                Ok(None)
            }
            Dictionary(key, _) => {
                let node = self.node(field)?;
                let width = key.primitive_width().unwrap_or(1);
                let validity = self.buffer(field, "validity", Rule::Validity(node.len))?;
                let keys = self.buffer(field, "keys", Rule::values(node.len, width))?;
                whole(field, keys, width, "keys")?;
                nulls(field, node, validity)?;
                Ok(None)
            }
            Union(children, mode) => {
                self.union(field, children, *mode)?;
                Ok(None)
            }
            Null => {
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
    fn union(
        &mut self,
        field: &Field,
        children: &UnionFields,
        mode: UnionMode,
    ) -> Result<(), String> {
        let node = self.node(field)?;
        if self.version < ipc::MetadataVersion::V5 {
            self.buffer(field, "validity", Rule::Validity(node.len))?;
        }
        let type_ids = self.buffer(field, "type ids", Rule::values(node.len, 1))?;
        if type_ids.len < node.len {
            return Err(format!(
                "column {:?} is a union of {} values with {} bytes of type ids",
                field.name(),
                node.len,
                type_ids.len
            ));
        }
        if mode == UnionMode::Dense {
            let offsets = self.buffer(field, "offsets", Rule::values(node.len, 4))?;
            let need = node.len.checked_mul(4);
            if need.is_none_or(|need| offsets.len < need) {
                return Err(format!(
                    "column {:?} is a dense union of {} values with {} bytes of offsets",
                    field.name(),
                    node.len,
                    offsets.len
                ));
            }
            // An inflated body places every buffer at a multiple of 64 from an arena buffer.
            if self.packed.is_none() {
                let address = self.body.as_ptr() as usize + offsets.offset;
                if !address.is_multiple_of(4) {
                    return Err(format!(
                        "column {:?} is a dense union whose offsets are not 4-byte aligned",
                        field.name()
                    ));
                }
            }
        }
        children.iter().try_for_each(|(_, child)| {
            self.array(child)?;
            Ok(())
        })
    }

    fn node(&mut self, field: &Field) -> Result<Node, String> {
        let node = self.nodes.next().ok_or_else(|| {
            format!(
                "column {:?} has no field node; the record batch has too few",
                field.name()
            )
        })?;
        let (len, nulls) = (node.length(), node.null_count());
        match (usize::try_from(len), usize::try_from(nulls)) {
            (Ok(len), Ok(nulls)) if nulls <= len => Ok(Node { len, nulls }),
            _ => Err(format!(
                "column {:?} has a field node of length {len} with {nulls} nulls",
                field.name()
            )),
        }
    }

    /// The next buffer of the message: inside the body, and for a compressed batch of the size
    /// `rule` says its column needs (`what` names it in a refusal).
    fn buffer(&mut self, field: &Field, what: &'static str, rule: Rule) -> Result<Span, String> {
        let buffer = self.buffers.next().ok_or_else(|| {
            format!(
                "column {:?} has no buffer left; the record batch has too few",
                field.name()
            )
        })?;
        let (offset, len) = (buffer.offset(), buffer.length());
        let span = match (usize::try_from(offset), usize::try_from(len)) {
            (Ok(offset), Ok(len)) => Some(Span {
                offset,
                len,
                slot: usize::MAX,
            }),
            _ => None,
        };
        let span = match span {
            Some(span)
                if span
                    .offset
                    .checked_add(span.len)
                    .is_some_and(|end| end <= self.body.len()) =>
            {
                span
            }
            _ => {
                return Err(format!(
                    "column {:?} has a buffer of {len} bytes at {offset}, outside the body of {} bytes",
                    field.name(),
                    self.body.len()
                ));
            }
        };
        match &mut self.packed {
            Some(packed) => packed.take(field, what, span, rule),
            None => Ok(span),
        }
    }
}

/// The width of a list's or a string's offsets.
fn offset_width(data_type: &DataType) -> usize {
    match data_type {
        DataType::LargeUtf8
        | DataType::LargeBinary
        | DataType::LargeList(_)
        | DataType::LargeListView(_) => 8,
        _ => 4,
    }
}

/// A buffer arrow's validation reads as a slice of `width`-byte values, which it asserts it
/// holds a whole number of.
fn whole(field: &Field, span: Span, width: usize, what: &str) -> Result<(), String> {
    if !span.len.is_multiple_of(width) {
        return Err(format!(
            "column {:?} has {what} of {} bytes, not a whole number of {width}-byte values",
            field.name(),
            span.len
        ));
    }
    Ok(())
}

/// A column with nulls has its validity bitmap read as `len` bits, which arrow asserts it holds.
fn nulls(field: &Field, node: Node, validity: Span) -> Result<(), String> {
    if node.nulls > 0 && validity.len.saturating_mul(8) < node.len {
        return Err(format!(
            "column {:?} has {} values and a validity bitmap of {} bytes",
            field.name(),
            node.len,
            validity.len
        ));
    }
    Ok(())
}
