//! An Arrow IPC message checked before arrow-ipc reads it (contracts CT-I13, e.8).
//!
//! arrow-ipc panics, where it could return an error, on some messages it cannot read: its schema
//! converter (`arrow::ipc::convert::fb_to_schema`) on a type it cannot map, and its record batch
//! decoder on a buffer outside the body, a validity bitmap shorter than its column, a union whose
//! type ids or offsets do not fit, a view column's buffer counts left over, a dictionary message
//! with no data, a header type with no header, and a few more. Every IPC byte Moruna reads is
//! untrusted: a `.wasm` or a `.pex` function's output, a Parquet footer, a record read back from
//! disk. The check itself is `moruna-ipc-check`, `no_std`, the one the data plane runs on the
//! Moruna kernel too (SDD 29): this module hands it arrow-ipc's messages, converts the schema a
//! batch is read against to its model, and routes a message that passed to arrow-ipc.
//!
//! A compressed record batch (`LZ4_FRAME` or `ZSTD`, method `BUFFER`) is inflated here, because
//! arrow-ipc allocates the decompressed length a buffer declares before it decompresses, so a few
//! bytes can ask for any amount of memory. The walk hands every buffer to the inflation plan
//! (`inflate.rs`) with the size its column implies, the plan holds the declared size to it and
//! the sizes together to the ceiling the caller passed, all before the first byte is allocated
//! for the batch; then the batch is inflated into one buffer of the arena, bounded at each
//! buffer's expected size, and arrow-ipc is handed an uncompressed message and body exactly as
//! for an uncompressed batch.

use arrow::buffer::Buffer as ArrowBuffer;
use arrow::datatypes::{DataType, Field, Schema, UnionMode};
use arrow::ipc;
use moruna_ipc_check::fb::Table;
use moruna_ipc_check::schema::{self as model};
use moruna_ipc_check::walk::{Body, Plan, walk};

use super::inflate::{Inflate, Inflated, Packed, Wrap};

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
            let fields: Vec<model::Field> = schema.fields().iter().map(|f| field(f)).collect();
            let packed = walked(batch, body.as_slice(), version, inflate, &fields)?;
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
                Some(DataType::Dictionary(_, value)) => kind(value),
                _ => return Err(format!("dictionary {id} is not in the schema")),
            };
            let field = model::Field::new("", value, true);
            let packed = walked(data, body.as_slice(), version, inflate, &[field])?;
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

/// The walk over one record batch, with the plan of what to inflate when it is compressed: the
/// codec and method are judged first, then the walk hands the plan every buffer.
fn walked<'a, 'i>(
    batch: ipc::RecordBatch<'_>,
    body: &'a [u8],
    version: ipc::MetadataVersion,
    inflate: &Inflate<'i>,
    fields: &[model::Field],
) -> Result<Option<Packed<'a, 'i>>, String> {
    let mut packed = batch
        .compression()
        .map(|compression| Packed::new(compression, body, version, *inflate))
        .transpose()?;
    let table = Table::at(batch._tab.buf(), batch._tab.loc())?;
    let at = Body {
        len: body.len(),
        addr: body.as_ptr() as usize,
    };
    let plan = packed.as_mut().map(|p| p as &mut dyn Plan);
    walk(table, at, version.0, fields, plan)?;
    Ok(packed)
}

/// A schema `fb_to_schema` converts without panicking, whose fields arrow-ipc's decoder can
/// build arrays of.
pub(super) fn check_schema(schema: ipc::Schema<'_>) -> Result<(), String> {
    let table = Table::at(schema._tab.buf(), schema._tab.loc())?;
    model::check_schema(table).map(|_| ())
}

/// An arrow field as the check's model of it.
fn field(field: &Field) -> model::Field {
    model::Field::new(
        field.name().as_str(),
        kind(field.data_type()),
        field.is_nullable(),
    )
}

/// An arrow type as the check's model of it: what the walk tells apart.
fn kind(data_type: &DataType) -> model::Type {
    use model::Type;
    let child = |f: &Field| Box::new(field(f));
    match data_type {
        DataType::Null => Type::Null,
        DataType::Boolean => Type::Boolean,
        DataType::Utf8 | DataType::Binary => Type::Text { width: 4 },
        DataType::LargeUtf8 | DataType::LargeBinary => Type::Text { width: 8 },
        DataType::BinaryView | DataType::Utf8View => Type::View,
        DataType::List(c) | DataType::Map(c, _) => Type::List {
            width: 4,
            child: child(c),
        },
        DataType::LargeList(c) => Type::List {
            width: 8,
            child: child(c),
        },
        DataType::ListView(c) => Type::ListView {
            width: 4,
            child: child(c),
        },
        DataType::LargeListView(c) => Type::ListView {
            width: 8,
            child: child(c),
        },
        DataType::FixedSizeList(c, size) => Type::FixedSizeList {
            size: *size,
            child: child(c),
        },
        DataType::Struct(fields) => Type::Struct(fields.iter().map(|f| field(f)).collect()),
        DataType::RunEndEncoded(run_ends, values) => Type::RunEndEncoded {
            run_ends: child(run_ends),
            values: child(values),
        },
        DataType::Dictionary(key, value) => Type::Dictionary {
            key_width: key.primitive_width().unwrap_or(1),
            value: Box::new(kind(value)),
        },
        DataType::Union(fields, mode) => Type::Union {
            dense: *mode == UnionMode::Dense,
            children: fields.iter().map(|(_, f)| field(f)).collect(),
        },
        DataType::FixedSizeBinary(width) => Type::FixedSizeBinary(*width),
        other => Type::Primitive(other.primitive_width()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::datatypes::{
        DataType, Field, Fields, IntervalUnit, Schema, TimeUnit, UnionFields, UnionMode,
    };
    use arrow::ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions};
    use moruna_ipc_check::fb::Table;
    use moruna_ipc_check::schema as model;

    /// Every type arrow writes a schema message for, the nested ones with nested children.
    fn every_type() -> Schema {
        let item = || Arc::new(Field::new("item", DataType::Int32, true));
        let entries = Arc::new(Field::new(
            "entries",
            DataType::Struct(Fields::from(vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("value", DataType::Float64, true),
            ])),
            false,
        ));
        let union = |mode| {
            DataType::Union(
                UnionFields::try_new(
                    [3, 7],
                    [
                        Field::new("a", DataType::Int8, true),
                        Field::new("b", DataType::LargeUtf8, true),
                    ],
                )
                .expect("union fields"),
                mode,
            )
        };
        let types = vec![
            DataType::Null,
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            DataType::Date32,
            DataType::Date64,
            DataType::Time32(TimeUnit::Second),
            DataType::Time64(TimeUnit::Microsecond),
            DataType::Duration(TimeUnit::Millisecond),
            DataType::Interval(IntervalUnit::YearMonth),
            DataType::Interval(IntervalUnit::DayTime),
            DataType::Interval(IntervalUnit::MonthDayNano),
            DataType::Binary,
            DataType::FixedSizeBinary(5),
            DataType::LargeBinary,
            DataType::BinaryView,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Utf8View,
            DataType::List(item()),
            DataType::ListView(item()),
            DataType::FixedSizeList(item(), 3),
            DataType::LargeList(item()),
            DataType::LargeListView(item()),
            DataType::Struct(Fields::from(vec![
                Field::new("x", DataType::Int64, false),
                Field::new("y", DataType::List(item()), true),
            ])),
            union(UnionMode::Dense),
            union(UnionMode::Sparse),
            DataType::Dictionary(Box::new(DataType::Int16), Box::new(DataType::Utf8)),
            DataType::Decimal32(9, 2),
            DataType::Decimal64(18, 3),
            DataType::Decimal128(38, 4),
            DataType::Decimal256(76, 5),
            DataType::Map(entries, false),
            DataType::RunEndEncoded(
                Arc::new(Field::new("run_ends", DataType::Int32, false)),
                Arc::new(Field::new("values", DataType::Utf8, true)),
            ),
        ];
        Schema::new(
            types
                .into_iter()
                .enumerate()
                .map(|(i, t)| Field::new(format!("c{i}"), t, i % 2 == 0))
                .collect::<Vec<_>>(),
        )
    }

    /// The check's model of a schema message, read from its flatbuffer, is the model of the
    /// schema arrow-ipc converts the same message into: a record read on the Moruna kernel
    /// (SDD 29) is walked against what a record read here is.
    #[test]
    fn the_two_models_of_a_schema_agree() {
        let schema = every_type();
        let encoded = IpcDataGenerator::default().schema_to_bytes_with_dictionary_tracker(
            &schema,
            &mut DictionaryTracker::new(false),
            &IpcWriteOptions::default(),
        );
        let message = arrow::ipc::root_as_message(&encoded.ipc_message).expect("a message");
        let fb = message.header_as_schema().expect("a schema");
        let converted = arrow::ipc::convert::fb_to_schema(fb);
        let from_arrow: Vec<model::Field> =
            converted.fields().iter().map(|f| super::field(f)).collect();
        let table = Table::root(&encoded.ipc_message).expect("a table");
        let header = table.table(2).expect("a header").expect("present");
        let from_flatbuffer = model::check_schema(header).expect("checked");
        assert_eq!(from_flatbuffer.len(), schema.fields().len());
        for (a, b) in from_arrow.iter().zip(&from_flatbuffer) {
            assert_eq!(a, b, "field {}", a.name);
        }
    }
}
