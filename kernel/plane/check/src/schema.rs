//! A schema message checked, and the model of its fields the walk reads a record batch against
//! (contracts CT-I13, e.8).
//!
//! `check_schema` refuses, with a sentence naming the field, every schema arrow-ipc's converter
//! (`fb_to_schema`) would panic on, and with them a few it would refuse anyway: no field list; a
//! type id with no Arrow type, or whose type table is missing; an integer, dictionary index,
//! decimal or time width, a floating point precision, or a date, time, timestamp, duration,
//! interval or union unit it does not map; a list, list view, fixed size list, map or run-end
//! encoded type without the children it takes; a map whose entries are not a struct of two
//! fields; a negative fixed size binary width; a decimal precision or scale outside a byte; a
//! big-endian decimal; union type ids that are negative, repeated, or not one per child. A
//! schema that passes is described by [`Field`]s, which keep exactly what the walk tells apart.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use crate::fb::Table;
use crate::format::{dictionary, field, schema, types};

/// A column of a schema, as the walk reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub nullable: bool,
    pub kind: Type,
}

impl Field {
    pub fn new(name: impl Into<String>, kind: Type, nullable: bool) -> Field {
        Field {
            name: name.into(),
            nullable,
            kind,
        }
    }
}

/// The types the walk tells apart: their buffers, and the widths that size them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Null,
    Boolean,
    /// A fixed-width column of `width` bytes a value (integers, floating point, dates, times,
    /// timestamps, durations, intervals, decimals); `None` when no width is stated.
    Primitive(Option<usize>),
    /// Values of `width` bytes each.
    FixedSizeBinary(i32),
    /// A string or binary column whose offsets are `width` bytes (4, or 8 for the large ones).
    Text {
        width: usize,
    },
    /// A string or binary view column.
    View,
    /// A list, a large list or a map: offsets of `width` bytes, then the child.
    List {
        width: usize,
        child: Box<Field>,
    },
    /// A list view or a large list view.
    ListView {
        width: usize,
        child: Box<Field>,
    },
    FixedSizeList {
        size: i32,
        child: Box<Field>,
    },
    Struct(Vec<Field>),
    RunEndEncoded {
        run_ends: Box<Field>,
        values: Box<Field>,
    },
    /// Dictionary-encoded: keys of `key_width` bytes, values of `value`'s type (what a
    /// dictionary message holds).
    Dictionary {
        key_width: usize,
        value: Box<Type>,
    },
    Union {
        dense: bool,
        children: Vec<Field>,
    },
}

impl Type {
    /// The width of one value, for the types arrow calls primitive.
    pub fn primitive_width(&self) -> Option<usize> {
        match self {
            Type::Primitive(width) => *width,
            _ => None,
        }
    }

    /// Whether this type, or any child of it, is dictionary encoded.
    pub fn has_dictionary(&self) -> bool {
        match self {
            Type::Dictionary { .. } => true,
            Type::List { child, .. }
            | Type::ListView { child, .. }
            | Type::FixedSizeList { child, .. } => child.kind.has_dictionary(),
            Type::Struct(children) | Type::Union { children, .. } => {
                children.iter().any(|c| c.kind.has_dictionary())
            }
            Type::RunEndEncoded { run_ends, values } => {
                run_ends.kind.has_dictionary() || values.kind.has_dictionary()
            }
            _ => false,
        }
    }
}

/// Refuse, naming the field, a schema `fb_to_schema` would panic on; the fields of one that
/// passes.
pub fn check_schema(table: Table<'_>) -> Result<Vec<Field>, String> {
    let fields = table
        .vector(schema::FIELDS, 4)?
        .ok_or("the schema has no field list")?;
    let big_endian = table.i16(schema::ENDIANNESS, 0)? == schema::BIG;
    let mut out = Vec::new();
    for i in 0..fields.len() {
        let field = fields.table(i)?;
        if big_endian && field.u8(field::TYPE_TYPE, types::NONE)? == types::DECIMAL {
            return Err(format!("field {:?} is a big-endian decimal", name(&field)?));
        }
        check_field(&field)?;
        out.push(model(&field)?);
    }
    Ok(out)
}

/// A field's name as the conversion takes it.
fn name<'a>(field: &Table<'a>) -> Result<&'a str, String> {
    Ok(field.str(field::NAME)?.unwrap_or_default())
}

/// The field's children, as the vector of tables it holds.
fn children<'a>(field: &Table<'a>) -> Result<Vec<Table<'a>>, String> {
    let Some(vector) = field.vector(field::CHILDREN, 4)? else {
        return Ok(Vec::new());
    };
    (0..vector.len()).map(|i| vector.table(i)).collect()
}

/// The table of a field's type, when its type id is `kind` and the table is there.
fn type_as<'a>(field: &Table<'a>, kind: u8) -> Result<Option<Table<'a>>, String> {
    if field.u8(field::TYPE_TYPE, types::NONE)? != kind {
        return Ok(None);
    }
    field.table(field::TYPE)
}

fn is_nested(kind: u8) -> bool {
    matches!(
        kind,
        types::LIST
            | types::LARGE_LIST
            | types::LIST_VIEW
            | types::LARGE_LIST_VIEW
            | types::FIXED_SIZE_LIST
            | types::STRUCT
            | types::RUN_END_ENCODED
            | types::MAP
            | types::UNION
    )
}

/// One field and the children the conversion recurses into.
fn check_field(field: &Table<'_>) -> Result<(), String> {
    let label = name(field)?;
    let fault = |what: String| format!("field {label:?} {what}");
    if let Some(encoding) = field.table(field::DICTIONARY)? {
        let index = encoding
            .table(dictionary::INDEX_TYPE)?
            .ok_or_else(|| fault("is dictionary encoded with no index type".into()))?;
        let width = index.i32(types::INT_BIT_WIDTH, 0)?;
        bits(width).ok_or_else(|| fault(format!("has a {width} bit dictionary index")))?;
    }
    let vector = field.vector(field::CHILDREN, 4)?;
    let count = vector.map(|v| v.len());
    let kind = field.u8(field::TYPE_TYPE, types::NONE)?;
    known(field, count)?.ok_or_else(|| {
        fault(format!(
            "has a type the Arrow IPC reader cannot convert ({}, {} children)",
            types::name(kind),
            count.unwrap_or(0)
        ))
    })?;
    // A map's entries are a struct of a key and a value: arrow builds a `MapArray` with `expect`
    // on exactly that.
    if kind == types::MAP {
        let entries = match vector {
            Some(v) if !v.is_empty() => Some(v.table(0)?),
            _ => None,
        };
        let pair = match entries {
            Some(e) => {
                e.u8(field::TYPE_TYPE, types::NONE)? == types::STRUCT
                    && e.vector(field::CHILDREN, 4)?.map(|c| c.len()) == Some(2)
            }
            None => false,
        };
        if !pair {
            return Err(fault(
                "is a map whose entries are not a struct of two fields".into(),
            ));
        }
    }
    if is_nested(kind) {
        for child in children(field)? {
            check_field(&child)?;
        }
    }
    Ok(())
}

/// `Some` when `fb_to_schema` maps this field's type without panicking and arrow can lay out
/// arrays of it: the type table the type id names is present, its enums and widths are ones it
/// knows, and a nested type has the number of children it takes.
fn known(field: &Table<'_>, children: Option<usize>) -> Result<Option<()>, String> {
    let unit = |u: i16| {
        matches!(
            u,
            types::SECOND | types::MILLISECOND | types::MICROSECOND | types::NANOSECOND
        )
    };
    let ok = |holds: bool| holds.then_some(());
    let kind = field.u8(field::TYPE_TYPE, types::NONE)?;
    let table = |id: u8| type_as(field, id);
    Ok(match kind {
        types::NULL
        | types::BOOL
        | types::BINARY
        | types::BINARY_VIEW
        | types::LARGE_BINARY
        | types::UTF8
        | types::UTF8_VIEW
        | types::LARGE_UTF8
        | types::STRUCT => Some(()),
        types::INT => match table(kind)? {
            Some(t) => bits(t.i32(types::INT_BIT_WIDTH, 0)?),
            None => None,
        },
        // arrow's buffer layout converts the width with `unwrap`.
        types::FIXED_SIZE_BINARY => match table(kind)? {
            Some(t) => ok(t.i32(types::WIDTH, 0)? >= 0),
            None => None,
        },
        types::FLOATING_POINT => match table(kind)? {
            Some(t) => ok(matches!(t.i16(types::PRECISION, 0)?, 0..=2)),
            None => None,
        },
        types::DATE => match table(kind)? {
            Some(t) => ok(matches!(
                t.i16(types::UNIT, types::MILLISECOND)?,
                types::DAY | types::MILLISECOND
            )),
            None => None,
        },
        types::TIME => match table(kind)? {
            Some(t) => ok(matches!(
                (
                    t.i32(types::TIME_BIT_WIDTH, 32)?,
                    t.i16(types::UNIT, types::MILLISECOND)?
                ),
                (32, types::SECOND | types::MILLISECOND)
                    | (64, types::MICROSECOND | types::NANOSECOND)
            )),
            None => None,
        },
        types::TIMESTAMP => match table(kind)? {
            Some(t) => ok(unit(t.i16(types::UNIT, types::SECOND)?)),
            None => None,
        },
        types::DURATION => match table(kind)? {
            Some(t) => ok(unit(t.i16(types::UNIT, types::MILLISECOND)?)),
            None => None,
        },
        types::INTERVAL => match table(kind)? {
            Some(t) => ok(matches!(
                t.i16(types::UNIT, types::YEAR_MONTH)?,
                types::YEAR_MONTH | types::DAY_TIME | types::MONTH_DAY_NANO
            )),
            None => None,
        },
        types::LIST | types::LARGE_LIST | types::LIST_VIEW | types::LARGE_LIST_VIEW => {
            ok(children == Some(1))
        }
        types::FIXED_SIZE_LIST => ok(table(kind)?.is_some() && children == Some(1)),
        types::RUN_END_ENCODED => ok(children == Some(2)),
        types::MAP => ok(table(kind)?.is_some() && children == Some(1)),
        types::DECIMAL => match table(kind)? {
            Some(t) => ok(u8::try_from(t.i32(types::DECIMAL_PRECISION, 0)?).is_ok()
                && i8::try_from(t.i32(types::DECIMAL_SCALE, 0)?).is_ok()
                && matches!(t.i32(types::DECIMAL_BIT_WIDTH, 128)?, 32 | 64 | 128 | 256)),
            None => None,
        },
        types::UNION => match table(kind)? {
            Some(t) => {
                let count = children.unwrap_or(0);
                let ids = match t.vector(types::UNION_TYPE_IDS, 4)? {
                    // The conversion numbers the children itself, and an `i8` holds 128 of them.
                    None => count <= 128,
                    Some(ids) => union_ids(&ids, count),
                };
                ok(matches!(
                    t.i16(types::UNION_MODE, types::SPARSE)?,
                    types::SPARSE | types::DENSE
                ) && ids)
            }
            None => None,
        },
        _ => None,
    })
}

/// Union type ids as arrow's `UnionFields::try_new` takes them: one per child, each cast to an
/// `i8`, none negative, none repeated.
fn union_ids(ids: &crate::fb::Vector<'_>, count: usize) -> bool {
    if ids.len() != count {
        return false;
    }
    let mut seen = 0u128;
    for i in 0..ids.len() {
        let Some(id) = ids.i32(i) else {
            return false;
        };
        let id = id as i8;
        if id < 0 {
            return false;
        }
        let mask = 1u128 << id;
        if seen & mask != 0 {
            return false;
        }
        seen |= mask;
    }
    true
}

/// `Some` for an integer width the conversion maps.
fn bits(width: i32) -> Option<()> {
    matches!(width, 8 | 16 | 32 | 64).then_some(())
}

/// The model of a field that passed `check_field`: its type as `fb_to_schema` converts it.
fn model(field: &Table<'_>) -> Result<Field, String> {
    let label = String::from(name(field)?);
    let nullable = field.bool(field::NULLABLE, false)?;
    let value = value_type(field)?;
    let kind = match field.table(field::DICTIONARY)? {
        Some(encoding) => {
            let width = match encoding.table(dictionary::INDEX_TYPE)? {
                Some(index) => index.i32(types::INT_BIT_WIDTH, 0)?,
                None => 0,
            };
            Type::Dictionary {
                key_width: usize::try_from(width / 8).unwrap_or(1),
                value: Box::new(value),
            }
        }
        None => value,
    };
    Ok(Field::new(label, kind, nullable))
}

fn child(field: &Table<'_>, i: usize) -> Result<Box<Field>, String> {
    let all = children(field)?;
    let table = all.get(i).ok_or_else(|| {
        format!(
            "field {:?} has no child {i}",
            name(field).unwrap_or_default()
        )
    })?;
    Ok(Box::new(model(table)?))
}

/// The type a field's own type table describes, before any dictionary encoding.
fn value_type(field: &Table<'_>) -> Result<Type, String> {
    let kind = field.u8(field::TYPE_TYPE, types::NONE)?;
    let width_of = |id: u8, slot: usize, default: i32| -> Result<i32, String> {
        match type_as(field, id)? {
            Some(t) => t.i32(slot, default),
            None => Ok(default),
        }
    };
    let fixed = |n: usize| Ok(Type::Primitive(Some(n)));
    match kind {
        types::NULL => Ok(Type::Null),
        types::BOOL => Ok(Type::Boolean),
        types::INT => {
            fixed(usize::try_from(width_of(kind, types::INT_BIT_WIDTH, 0)? / 8).unwrap_or(0))
        }
        types::FLOATING_POINT => {
            let precision = match type_as(field, kind)? {
                Some(t) => t.i16(types::PRECISION, 0)?,
                None => 0,
            };
            fixed(match precision {
                0 => 2,
                1 => 4,
                _ => 8,
            })
        }
        types::DECIMAL => {
            fixed(usize::try_from(width_of(kind, types::DECIMAL_BIT_WIDTH, 128)? / 8).unwrap_or(0))
        }
        types::DATE => {
            let unit = match type_as(field, kind)? {
                Some(t) => t.i16(types::UNIT, types::MILLISECOND)?,
                None => types::MILLISECOND,
            };
            fixed(if unit == types::DAY { 4 } else { 8 })
        }
        types::TIME => {
            fixed(usize::try_from(width_of(kind, types::TIME_BIT_WIDTH, 32)? / 8).unwrap_or(0))
        }
        types::TIMESTAMP | types::DURATION => fixed(8),
        types::INTERVAL => {
            let unit = match type_as(field, kind)? {
                Some(t) => t.i16(types::UNIT, types::YEAR_MONTH)?,
                None => types::YEAR_MONTH,
            };
            fixed(match unit {
                types::YEAR_MONTH => 4,
                types::DAY_TIME => 8,
                _ => 16,
            })
        }
        types::FIXED_SIZE_BINARY => Ok(Type::FixedSizeBinary(width_of(kind, types::WIDTH, 0)?)),
        types::BINARY | types::UTF8 => Ok(Type::Text { width: 4 }),
        types::LARGE_BINARY | types::LARGE_UTF8 => Ok(Type::Text { width: 8 }),
        types::BINARY_VIEW | types::UTF8_VIEW => Ok(Type::View),
        types::LIST | types::MAP => Ok(Type::List {
            width: 4,
            child: child(field, 0)?,
        }),
        types::LARGE_LIST => Ok(Type::List {
            width: 8,
            child: child(field, 0)?,
        }),
        types::LIST_VIEW => Ok(Type::ListView {
            width: 4,
            child: child(field, 0)?,
        }),
        types::LARGE_LIST_VIEW => Ok(Type::ListView {
            width: 8,
            child: child(field, 0)?,
        }),
        types::FIXED_SIZE_LIST => Ok(Type::FixedSizeList {
            size: width_of(kind, types::WIDTH, 0)?,
            child: child(field, 0)?,
        }),
        types::STRUCT => Ok(Type::Struct(
            children(field)?
                .iter()
                .map(model)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        types::RUN_END_ENCODED => Ok(Type::RunEndEncoded {
            run_ends: child(field, 0)?,
            values: child(field, 1)?,
        }),
        types::UNION => {
            let dense = match type_as(field, kind)? {
                Some(t) => t.i16(types::UNION_MODE, types::SPARSE)? == types::DENSE,
                None => false,
            };
            Ok(Type::Union {
                dense,
                children: children(field)?
                    .iter()
                    .map(model)
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
        other => Err(format!(
            "field {:?} has a type the Arrow IPC reader cannot convert ({}, 0 children)",
            name(field)?,
            types::name(other)
        )),
    }
}
