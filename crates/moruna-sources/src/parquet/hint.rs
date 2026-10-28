//! The Arrow schema a Parquet footer may carry, checked before it is converted (SO-I10).
//!
//! A file written from Arrow keeps its Arrow schema base64-encoded under `ARROW:schema` in the
//! footer's key-value metadata, and the Parquet reader converts it with the Arrow IPC reader
//! (`arrow::ipc::convert::fb_to_schema`). That conversion panics on a schema it cannot map (an
//! unknown type id, a missing type table, a list without its one child, a decimal precision
//! that does not fit a byte) where it could return an error. A footer is untrusted input, so
//! each field is checked here for exactly what the conversion relies on, and a schema it would
//! panic on is refused with the field and what is wrong with it. A value that is not base64 or
//! not a schema message is left to the Parquet reader, which returns an error for it.

use arrow::datatypes::{DataType, Field, UnionFields};
use arrow::ipc;
use parquet::file::metadata::FileMetaData;

/// The key the Arrow writer stores its schema under.
const KEY: &str = "ARROW:schema";

/// `Err` naming the field and the fault when the footer's Arrow schema is one the IPC reader
/// would panic on; `Ok` when there is none, or when it is one the reader converts or refuses.
pub(crate) fn check(metadata: &FileMetaData) -> Result<(), String> {
    for entry in metadata.key_value_metadata().into_iter().flatten() {
        if let (KEY, Some(value)) = (entry.key.as_str(), entry.value.as_deref())
            && let Some(bytes) = base64(value)
        {
            check_message(&bytes)?;
        }
    }
    Ok(())
}

/// The schema message the Parquet reader would hand to `fb_to_schema`, after the same framing
/// it strips (a continuation marker and a length).
fn check_message(bytes: &[u8]) -> Result<(), String> {
    let framed = bytes.len() > 8 && bytes.get(..4) == Some(&[0xFF; 4][..]);
    let body = if framed { &bytes[8..] } else { bytes };
    let Some(schema) = ipc::root_as_message(body)
        .ok()
        .and_then(|message| message.header_as_schema())
    else {
        return Ok(());
    };
    let fields = schema.fields().ok_or("has no field list")?;
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

/// `Some` when `fb_to_schema` maps this field's type without panicking: the type table the
/// type id names is present, its enums and widths are ones it knows, and a nested type has the
/// number of children it takes.
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
        Type::FixedSizeBinary => field.type_as_fixed_size_binary().map(drop),
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

/// Standard base64, accepting everything the Parquet reader's strict decoder accepts and
/// giving the same bytes for it; `None` for a character outside the alphabet, which that
/// decoder refuses too.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut held) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((acc >> held) as u8);
            acc &= (1 << held) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{IntervalUnit, Schema, TimeUnit, UnionMode};
    use arrow::ipc::writer::{DictionaryTracker, IpcDataGenerator, IpcWriteOptions};
    use std::sync::Arc;

    /// The schema message the IPC writer produces for `schema`.
    fn encoded(schema: &Schema) -> Vec<u8> {
        let mut tracker = DictionaryTracker::new(false);
        IpcDataGenerator::default()
            .schema_to_bytes_with_dictionary_tracker(
                schema,
                &mut tracker,
                &IpcWriteOptions::default(),
            )
            .ipc_message
    }

    #[test]
    fn every_type_the_writer_produces_passes() {
        let item = || Arc::new(Field::new("item", DataType::Int32, true));
        let entries = Field::new(
            "entries",
            DataType::Struct(
                vec![
                    Field::new("key", DataType::Utf8, false),
                    Field::new("value", DataType::Int64, true),
                ]
                .into(),
            ),
            false,
        );
        let union = UnionFields::try_new(
            vec![0, 5],
            vec![
                Field::new("a", DataType::Int8, true),
                Field::new("b", DataType::Float32, true),
            ],
        )
        .expect("a union");
        let types = vec![
            DataType::Null,
            DataType::Boolean,
            DataType::Int8,
            DataType::UInt16,
            DataType::Int32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Utf8View,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::BinaryView,
            DataType::FixedSizeBinary(16),
            DataType::Date32,
            DataType::Date64,
            DataType::Time32(TimeUnit::Second),
            DataType::Time64(TimeUnit::Nanosecond),
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            DataType::Duration(TimeUnit::Millisecond),
            DataType::Interval(IntervalUnit::MonthDayNano),
            DataType::List(item()),
            DataType::LargeList(item()),
            DataType::FixedSizeList(item(), 3),
            DataType::Struct(vec![Field::new("x", DataType::Int16, true)].into()),
            DataType::Map(Arc::new(entries), false),
            DataType::Decimal128(38, 9),
            DataType::Decimal256(76, -2),
            DataType::Union(union.clone(), UnionMode::Dense),
            DataType::Union(union, UnionMode::Sparse),
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            DataType::RunEndEncoded(
                Arc::new(Field::new("run_ends", DataType::Int32, false)),
                Arc::new(Field::new("values", DataType::Utf8, true)),
            ),
        ];
        let fields: Vec<Field> = types
            .into_iter()
            .enumerate()
            .map(|(i, t)| Field::new(format!("c{i}"), t, true))
            .collect();
        let bytes = encoded(&Schema::new(fields));
        assert_eq!(check_message(&bytes), Ok(()));
        // The same message behind the continuation marker and length the stream format adds.
        let mut framed = vec![0xFF; 4];
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(&bytes);
        assert_eq!(check_message(&framed), Ok(()));
    }

    /// Every single-byte corruption of a schema message with every kind of field: whatever the
    /// check passes, the IPC reader converts without panicking, and some it refuses.
    #[test]
    fn what_the_check_passes_the_reader_converts() {
        let item = || Arc::new(Field::new("item", DataType::Int64, true));
        let schema = Schema::new(vec![
            Field::new("i", DataType::Int32, true),
            Field::new("f", DataType::Float64, true),
            Field::new("d", DataType::Date32, true),
            Field::new("t", DataType::Time64(TimeUnit::Microsecond), true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Second, None), true),
            Field::new("du", DataType::Duration(TimeUnit::Nanosecond), true),
            Field::new("iv", DataType::Interval(IntervalUnit::DayTime), true),
            Field::new("l", DataType::List(item()), true),
            Field::new("fl", DataType::FixedSizeList(item(), 2), true),
            Field::new("dec", DataType::Decimal128(10, 2), true),
            Field::new(
                "u",
                DataType::Union(
                    UnionFields::try_new(
                        vec![1, 2],
                        vec![
                            Field::new("a", DataType::Int8, true),
                            Field::new("b", DataType::Utf8, true),
                        ],
                    )
                    .expect("a union"),
                    UnionMode::Sparse,
                ),
                true,
            ),
            Field::new(
                "dict",
                DataType::Dictionary(Box::new(DataType::UInt8), Box::new(DataType::Utf8)),
                true,
            ),
        ]);
        let bytes = encoded(&schema);
        let mut refused = 0;
        for at in 0..bytes.len() {
            for value in [0x00u8, 0x01, 0x02, 0x07, 0x7F, 0x80, 0xE5, 0xFF] {
                let mut corrupt = bytes.clone();
                corrupt[at] = value;
                if check_message(&corrupt).is_err() {
                    refused += 1;
                    continue;
                }
                let converted = std::panic::catch_unwind(|| {
                    ipc::root_as_message(&corrupt)
                        .ok()
                        .and_then(|m| m.header_as_schema())
                        .map(ipc::convert::fb_to_schema)
                        .map(|s| s.fields().len())
                });
                assert!(
                    converted.is_ok(),
                    "byte {at} = {value:#04x} passed and panicked"
                );
            }
        }
        assert!(refused > 0, "no corruption was refused");
    }

    #[test]
    fn what_is_not_a_schema_message_is_left_to_the_reader() {
        assert_eq!(check_message(b"not a flatbuffer"), Ok(()));
        assert_eq!(check_message(&[]), Ok(()));
    }

    #[test]
    fn base64_decodes_the_standard_alphabet() {
        assert_eq!(base64("TW9ydW5h"), Some(b"Moruna".to_vec()));
        assert_eq!(base64("TW9ydW4="), Some(b"Morun".to_vec()));
        assert_eq!(base64("TW9ydQ=="), Some(b"Moru".to_vec()));
        assert_eq!(base64("+/+/"), Some(vec![0xFB, 0xFF, 0xBF]));
        assert_eq!(base64("TW9y\u{0}"), None);
        assert_eq!(base64(""), Some(Vec::new()));
    }
}
