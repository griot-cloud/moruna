//! Host tests: messages built by hand with a small forward flatbuffer writer, so the crate keeps
//! no dependency (its agreement with arrow's own writer is moruna-kernel's CT-T29 and its unit
//! test of the two schema models).

use alloc::{boxed::Box, string::String, string::ToString, vec, vec::Vec};

use crate::fb::{MAX_DEPTH, Table, pair};
use crate::format::{compression, header, types, version};
use crate::frame::{self, END_OF_STREAM};
use crate::record::{FRAMING_MAX, Need, PAGE, check_record, framing_need, reframe, stream_parts};
use crate::schema::{Field, Type, check_schema};
use crate::walk::{Body, Plan, Rule, Span, walk};

// ---- a forward flatbuffer writer ----

#[derive(Clone)]
enum V {
    U8(u8),
    I8(i8),
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    T(Tb),
    Ts(Vec<Tb>),
    S(Vec<u8>),
    Pairs(Vec<(i64, i64)>),
    I32s(Vec<i32>),
    I64s(Vec<i64>),
}

#[derive(Clone, Default)]
struct Tb(Vec<(usize, V)>);

impl Tb {
    fn with(mut self, slot: usize, v: V) -> Tb {
        self.0.push((slot, v));
        self
    }
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn vector(buf: &mut Vec<u8>, v: &V) -> usize {
    let at = buf.len();
    match v {
        V::S(bytes) => {
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
            buf.push(0);
        }
        V::Pairs(p) => {
            buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
            for (a, b) in p {
                buf.extend_from_slice(&a.to_le_bytes());
                buf.extend_from_slice(&b.to_le_bytes());
            }
        }
        V::I32s(p) => {
            buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
            for a in p {
                buf.extend_from_slice(&a.to_le_bytes());
            }
        }
        V::I64s(p) => {
            buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
            for a in p {
                buf.extend_from_slice(&a.to_le_bytes());
            }
        }
        V::Ts(tables) => {
            buf.extend_from_slice(&(tables.len() as u32).to_le_bytes());
            let slots = buf.len();
            buf.resize(slots + 4 * tables.len(), 0);
            for (i, t) in tables.iter().enumerate() {
                let loc = table(buf, t);
                let pos = slots + 4 * i;
                put_u32(buf, pos, (loc - pos) as u32);
            }
        }
        _ => unreachable!("not a vector"),
    }
    at
}

fn table(buf: &mut Vec<u8>, t: &Tb) -> usize {
    let slots = t.0.iter().map(|(s, _)| s + 1).max().unwrap_or(0);
    let mut offsets = vec![0u16; slots];
    let mut inline = Vec::new();
    let mut later = Vec::new();
    let mut at = 4usize;
    for (slot, v) in &t.0 {
        offsets[*slot] = at as u16;
        let bytes: Vec<u8> = match v {
            V::U8(x) => vec![*x],
            V::I8(x) => x.to_le_bytes().to_vec(),
            V::Bool(x) => vec![*x as u8],
            V::I16(x) => x.to_le_bytes().to_vec(),
            V::I32(x) => x.to_le_bytes().to_vec(),
            V::I64(x) => x.to_le_bytes().to_vec(),
            other => {
                later.push((at, other.clone()));
                vec![0; 4]
            }
        };
        at += bytes.len();
        inline.extend(bytes);
    }
    let vtable = buf.len();
    buf.extend_from_slice(&((4 + 2 * slots) as u16).to_le_bytes());
    buf.extend_from_slice(&(at as u16).to_le_bytes());
    for o in offsets {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    let loc = buf.len();
    buf.extend_from_slice(&((loc - vtable) as i32).to_le_bytes());
    buf.extend(inline);
    for (within, v) in later {
        let target = match &v {
            V::T(child) => table(buf, child),
            other => vector(buf, other),
        };
        let pos = loc + within;
        put_u32(buf, pos, (target - pos) as u32);
    }
    loc
}

fn build(root: &Tb) -> Vec<u8> {
    let mut buf = vec![0u8; 4];
    let loc = table(&mut buf, root);
    put_u32(&mut buf, 0, loc as u32);
    buf
}

/// A whole IPC message: the continuation marker, the length, the flatbuffer padded to 8.
fn whole(flatbuffer: &[u8]) -> Vec<u8> {
    let mut fb = flatbuffer.to_vec();
    while !fb.len().is_multiple_of(8) {
        fb.push(0);
    }
    let mut out = vec![0xff, 0xff, 0xff, 0xff];
    out.extend_from_slice(&(fb.len() as u32).to_le_bytes());
    out.extend(fb);
    out
}

// ---- Arrow tables ----

fn ty(type_type: u8, table: Option<Tb>) -> Tb {
    let t = Tb::default().with(2, V::U8(type_type));
    match table {
        Some(table) => t.with(3, V::T(table)),
        None => t,
    }
}

fn field(name: &str, type_type: u8, type_table: Option<Tb>, children: Vec<Tb>) -> Tb {
    let mut f = ty(type_type, type_table)
        .with(0, V::S(name.as_bytes().to_vec()))
        .with(1, V::Bool(true));
    if !children.is_empty() {
        f = f.with(5, V::Ts(children));
    }
    f
}

fn int(bits: i32) -> Tb {
    Tb::default().with(0, V::I32(bits)).with(1, V::Bool(true))
}

fn int_field(name: &str, bits: i32) -> Tb {
    field(name, types::INT, Some(int(bits)), vec![])
}

fn plain(name: &str, type_type: u8) -> Tb {
    field(name, type_type, Some(Tb::default()), vec![])
}

fn schema(fields: Vec<Tb>) -> Tb {
    Tb::default().with(1, V::Ts(fields))
}

fn schema_message(fields: Vec<Tb>) -> Vec<u8> {
    whole(&build(
        &Tb::default()
            .with(0, V::I16(version::V5))
            .with(1, V::U8(header::SCHEMA))
            .with(2, V::T(schema(fields))),
    ))
}

fn record_batch(rows: i64, nodes: Vec<(i64, i64)>, buffers: Vec<(i64, i64)>) -> Tb {
    Tb::default()
        .with(0, V::I64(rows))
        .with(1, V::Pairs(nodes))
        .with(2, V::Pairs(buffers))
}

fn batch_message(rb: Tb, body: i64) -> Vec<u8> {
    whole(&build(
        &Tb::default()
            .with(0, V::I16(version::V5))
            .with(1, V::U8(header::RECORD_BATCH))
            .with(2, V::T(rb))
            .with(3, V::I64(body)),
    ))
}

fn checked(fields: Vec<Tb>) -> Result<Vec<Field>, String> {
    let bytes = build(&schema(fields));
    check_schema(Table::root(&bytes).unwrap())
}

fn refused(r: Result<impl core::fmt::Debug, String>, says: &str) {
    match r {
        Ok(v) => panic!("{v:?} passed; it should have been refused with {says:?}"),
        Err(e) => assert!(e.contains(says), "{e:?} should say {says:?}"),
    }
}

// ---- fb ----

#[test]
fn fb_reads_what_it_wrote_and_refuses_what_lies() {
    let bytes = build(
        &Tb::default()
            .with(0, V::U8(7))
            .with(1, V::I8(-3))
            .with(2, V::Bool(true))
            .with(3, V::I16(-9))
            .with(4, V::I32(70_000))
            .with(5, V::I64(-1 << 40))
            .with(6, V::S(b"name".to_vec()))
            .with(7, V::I32s(vec![1, -2]))
            .with(8, V::I64s(vec![5]))
            .with(9, V::Pairs(vec![(1, 2)])),
    );
    let t = Table::root(&bytes).unwrap();
    assert_eq!(t.buf().len(), bytes.len());
    let again = Table::at(t.buf(), t.loc()).unwrap();
    assert_eq!(again.u8(0, 0).unwrap(), 7);
    assert_eq!(t.i8(1, 0).unwrap(), -3);
    assert!(t.bool(2, false).unwrap());
    assert_eq!(t.i16(3, 0).unwrap(), -9);
    assert_eq!(t.i32(4, 0).unwrap(), 70_000);
    assert_eq!(t.i64(5, 0).unwrap(), -1 << 40);
    assert_eq!(t.str(6).unwrap(), Some("name"));
    let v = t.vector(7, 4).unwrap().unwrap();
    assert_eq!(
        (v.len(), v.is_empty(), v.i32(0), v.i32(1), v.i32(2)),
        (2, false, Some(1), Some(-2), None)
    );
    assert_eq!(v.bytes().len(), 8);
    assert_eq!(t.vector(8, 8).unwrap().unwrap().i64(0), Some(5));
    let p = t.vector(9, 16).unwrap().unwrap();
    assert_eq!(p.element(0).and_then(pair), Some((1, 2)));
    assert_eq!(p.element(1), None);
    assert!(p.table(0).is_err());
    assert!(pair(&[0u8; 8]).is_none());
    // Absent fields take their defaults; a slot past the vtable is absent.
    assert_eq!(t.u8(40, 9).unwrap(), 9);
    assert_eq!(t.table(40).unwrap().map(|t| t.loc()), None);
    assert!(t.vector(40, 4).unwrap().is_none());
    assert!(t.str(40).unwrap().is_none());
    // A string that is not UTF-8.
    let bad = build(&Tb::default().with(0, V::S(vec![0xff, 0xfe])));
    refused(Table::root(&bad).unwrap().str(0), "not UTF-8");
    // A vector longer than what follows.
    let mut long = bytes.clone();
    let at = t.position(7).unwrap().unwrap();
    let start = at + u32::from_le_bytes(long[at..at + 4].try_into().unwrap()) as usize;
    long[start..start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    refused(Table::root(&long).unwrap().vector(7, 4), "past the end");
    // An offset past the end, a root past the end, a short buffer.
    let mut far = bytes.clone();
    far[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    refused(
        Table::root(&far).unwrap().vector(7, 4),
        "past the end of the flatbuffer",
    );
    refused(Table::root(&[0, 0]), "ends inside");
    refused(Table::root(&[200, 0, 0, 0]), "past the end");
    // A vtable outside the buffer, or not a whole vtable.
    let mut lost = bytes.clone();
    let loc = t.loc();
    lost[loc..loc + 4].copy_from_slice(&i32::MIN.to_le_bytes());
    refused(Table::root(&lost), "vtable outside");
    let mut odd = bytes.clone();
    let vt = loc - i32::from_le_bytes(bytes[loc..loc + 4].try_into().unwrap()) as usize;
    odd[vt..vt + 2].copy_from_slice(&3u16.to_le_bytes());
    refused(Table::root(&odd), "not a whole vtable");
    let mut big = bytes.clone();
    big[vt..vt + 2].copy_from_slice(&(u16::MAX - 1).to_le_bytes());
    refused(Table::root(&big), "ends inside a vtable");
    // A scalar whose bytes are past the end.
    let mut cut = build(&Tb::default().with(0, V::I64(1)));
    let n = cut.len();
    cut.truncate(n - 4);
    refused(Table::root(&cut).unwrap().i64(0, 0), "ends inside a field");
}

#[test]
fn fb_refuses_a_nesting_deeper_than_its_bound() {
    let mut t = Tb::default().with(0, V::I32(1));
    for _ in 0..MAX_DEPTH {
        t = Tb::default().with(0, V::T(t));
    }
    let bytes = build(&t);
    let mut at = Table::root(&bytes).unwrap();
    let mut result = Ok(());
    for _ in 0..=MAX_DEPTH {
        match at.table(0) {
            Ok(Some(next)) => at = next,
            Ok(None) => break,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    refused(result, "more than 64 deep");
}

#[test]
fn format_names_are_arrows() {
    use crate::format::{compression as c, header as h, types as t, version as v};
    assert_eq!(h::name(h::SCHEMA), "Schema");
    assert_eq!(h::name(h::DICTIONARY_BATCH), "DictionaryBatch");
    assert_eq!(h::name(h::RECORD_BATCH), "RecordBatch");
    assert_eq!(h::name(h::TENSOR), "Tensor");
    assert_eq!(h::name(h::SPARSE_TENSOR), "SparseTensor");
    assert_eq!(h::name(h::NONE), "NONE");
    assert_eq!(h::name(9), "<UNKNOWN 9>");
    assert_eq!(v::name(v::V5), "V5");
    assert_eq!(v::name(9), "<UNKNOWN 9>");
    assert_eq!(t::name(t::STRUCT), "Struct_");
    assert_eq!(t::name(t::LARGE_LIST_VIEW), "LargeListView");
    assert_eq!(t::name(99), "<UNKNOWN 99>");
    assert_eq!(c::codec(0), "LZ4_FRAME");
    assert_eq!(c::codec(1), "ZSTD");
    assert_eq!(c::codec(5), "<UNKNOWN 5>");
}

// ---- schema ----

fn every_type() -> Vec<(Tb, Type)> {
    let item = || int_field("item", 32);
    let i32t = Type::Primitive(Some(4));
    let child = |t: Type| Box::new(Field::new("item", t, true));
    let unit = |u: i16| Some(Tb::default().with(0, V::I16(u)));
    vec![
        (plain("null", types::NULL), Type::Null),
        (plain("bool", types::BOOL), Type::Boolean),
        (int_field("i8", 8), Type::Primitive(Some(1))),
        (int_field("i64", 64), Type::Primitive(Some(8))),
        (
            field("half", types::FLOATING_POINT, Some(Tb::default()), vec![]),
            Type::Primitive(Some(2)),
        ),
        (
            field(
                "single",
                types::FLOATING_POINT,
                Some(Tb::default().with(0, V::I16(1))),
                vec![],
            ),
            Type::Primitive(Some(4)),
        ),
        (
            field(
                "double",
                types::FLOATING_POINT,
                Some(Tb::default().with(0, V::I16(2))),
                vec![],
            ),
            Type::Primitive(Some(8)),
        ),
        (
            field(
                "d128",
                types::DECIMAL,
                Some(Tb::default().with(0, V::I32(38))),
                vec![],
            ),
            Type::Primitive(Some(16)),
        ),
        (
            field(
                "d32",
                types::DECIMAL,
                Some(Tb::default().with(0, V::I32(9)).with(2, V::I32(32))),
                vec![],
            ),
            Type::Primitive(Some(4)),
        ),
        (
            field("date32", types::DATE, unit(types::DAY), vec![]),
            Type::Primitive(Some(4)),
        ),
        (
            field("date64", types::DATE, Some(Tb::default()), vec![]),
            Type::Primitive(Some(8)),
        ),
        (
            field("time32", types::TIME, Some(Tb::default()), vec![]),
            Type::Primitive(Some(4)),
        ),
        (
            field(
                "time64",
                types::TIME,
                Some(
                    Tb::default()
                        .with(0, V::I16(types::NANOSECOND))
                        .with(1, V::I32(64)),
                ),
                vec![],
            ),
            Type::Primitive(Some(8)),
        ),
        (
            field("ts", types::TIMESTAMP, Some(Tb::default()), vec![]),
            Type::Primitive(Some(8)),
        ),
        (
            field("dur", types::DURATION, Some(Tb::default()), vec![]),
            Type::Primitive(Some(8)),
        ),
        (
            field("ym", types::INTERVAL, Some(Tb::default()), vec![]),
            Type::Primitive(Some(4)),
        ),
        (
            field("dt", types::INTERVAL, unit(types::DAY_TIME), vec![]),
            Type::Primitive(Some(8)),
        ),
        (
            field("mdn", types::INTERVAL, unit(types::MONTH_DAY_NANO), vec![]),
            Type::Primitive(Some(16)),
        ),
        (
            field(
                "fsb",
                types::FIXED_SIZE_BINARY,
                Some(Tb::default().with(0, V::I32(5))),
                vec![],
            ),
            Type::FixedSizeBinary(5),
        ),
        (plain("bin", types::BINARY), Type::Text { width: 4 }),
        (plain("utf8", types::UTF8), Type::Text { width: 4 }),
        (plain("lbin", types::LARGE_BINARY), Type::Text { width: 8 }),
        (plain("lutf8", types::LARGE_UTF8), Type::Text { width: 8 }),
        (plain("bv", types::BINARY_VIEW), Type::View),
        (plain("uv", types::UTF8_VIEW), Type::View),
        (
            field("list", types::LIST, Some(Tb::default()), vec![item()]),
            Type::List {
                width: 4,
                child: child(i32t.clone()),
            },
        ),
        (
            field(
                "llist",
                types::LARGE_LIST,
                Some(Tb::default()),
                vec![item()],
            ),
            Type::List {
                width: 8,
                child: child(i32t.clone()),
            },
        ),
        (
            field("lv", types::LIST_VIEW, Some(Tb::default()), vec![item()]),
            Type::ListView {
                width: 4,
                child: child(i32t.clone()),
            },
        ),
        (
            field(
                "llv",
                types::LARGE_LIST_VIEW,
                Some(Tb::default()),
                vec![item()],
            ),
            Type::ListView {
                width: 8,
                child: child(i32t.clone()),
            },
        ),
        (
            field(
                "fsl",
                types::FIXED_SIZE_LIST,
                Some(Tb::default().with(0, V::I32(3))),
                vec![item()],
            ),
            Type::FixedSizeList {
                size: 3,
                child: child(i32t.clone()),
            },
        ),
        (
            field(
                "struct",
                types::STRUCT,
                Some(Tb::default()),
                vec![item(), plain("b", types::BOOL)],
            ),
            Type::Struct(vec![
                Field::new("item", i32t.clone(), true),
                Field::new("b", Type::Boolean, true),
            ]),
        ),
        (
            field(
                "map",
                types::MAP,
                Some(Tb::default()),
                vec![field(
                    "entries",
                    types::STRUCT,
                    Some(Tb::default()),
                    vec![plain("key", types::UTF8), item()],
                )],
            ),
            Type::List {
                width: 4,
                child: Box::new(Field::new(
                    "entries",
                    Type::Struct(vec![
                        Field::new("key", Type::Text { width: 4 }, true),
                        Field::new("item", i32t.clone(), true),
                    ]),
                    true,
                )),
            },
        ),
        (
            field(
                "ree",
                types::RUN_END_ENCODED,
                Some(Tb::default()),
                vec![int_field("run_ends", 32), plain("values", types::UTF8)],
            ),
            Type::RunEndEncoded {
                run_ends: Box::new(Field::new("run_ends", i32t.clone(), true)),
                values: Box::new(Field::new("values", Type::Text { width: 4 }, true)),
            },
        ),
        (
            field(
                "dense",
                types::UNION,
                Some(
                    Tb::default()
                        .with(0, V::I16(types::DENSE))
                        .with(1, V::I32s(vec![3, 7])),
                ),
                vec![item(), plain("b", types::BOOL)],
            ),
            Type::Union {
                dense: true,
                children: vec![
                    Field::new("item", i32t.clone(), true),
                    Field::new("b", Type::Boolean, true),
                ],
            },
        ),
        (
            field("sparse", types::UNION, Some(Tb::default()), vec![item()]),
            Type::Union {
                dense: false,
                children: vec![Field::new("item", i32t.clone(), true)],
            },
        ),
        (
            plain("dict", types::UTF8).with(
                4,
                V::T(Tb::default().with(0, V::I64(1)).with(1, V::T(int(16)))),
            ),
            Type::Dictionary {
                key_width: 2,
                value: Box::new(Type::Text { width: 4 }),
            },
        ),
    ]
}

#[test]
fn schema_models_every_type_as_arrow_converts_it() {
    let (tables, kinds): (Vec<Tb>, Vec<Type>) = every_type().into_iter().unzip();
    let fields = checked(tables).unwrap();
    assert_eq!(fields.len(), kinds.len());
    for (f, k) in fields.iter().zip(&kinds) {
        assert_eq!(&f.kind, k, "field {}", f.name);
        assert!(f.nullable);
    }
    assert!(fields.iter().any(|f| f.kind.has_dictionary()));
    assert!(!Type::Boolean.has_dictionary());
    let nested_dict = Type::RunEndEncoded {
        run_ends: Box::new(Field::new("r", Type::Primitive(Some(4)), false)),
        values: Box::new(Field::new(
            "v",
            Type::Dictionary {
                key_width: 1,
                value: Box::new(Type::Null),
            },
            false,
        )),
    };
    assert!(nested_dict.has_dictionary());
    assert_eq!(Type::Boolean.primitive_width(), None);
}

#[test]
fn schema_refuses_what_the_conversion_would_panic_on() {
    refused(
        check_schema(Table::root(&build(&Tb::default())).unwrap()),
        "the schema has no field list",
    );
    let one = |f: Tb| checked(vec![f]);
    refused(one(int_field("i", 7)), "cannot convert (Int, 0 children)");
    refused(
        one(field("i", types::INT, None, vec![])),
        "cannot convert (Int",
    );
    refused(
        one(plain("x", 99)),
        "cannot convert (<UNKNOWN 99>, 0 children)",
    );
    refused(
        one(field(
            "f",
            types::FIXED_SIZE_BINARY,
            Some(Tb::default().with(0, V::I32(-1))),
            vec![],
        )),
        "cannot convert (FixedSizeBinary",
    );
    refused(
        one(field(
            "f",
            types::FLOATING_POINT,
            Some(Tb::default().with(0, V::I16(3))),
            vec![],
        )),
        "cannot convert (FloatingPoint",
    );
    refused(
        one(field(
            "d",
            types::DATE,
            Some(Tb::default().with(0, V::I16(2))),
            vec![],
        )),
        "cannot convert (Date",
    );
    refused(
        one(field(
            "t",
            types::TIME,
            Some(Tb::default().with(1, V::I32(64))),
            vec![],
        )),
        "cannot convert (Time",
    );
    refused(
        one(field(
            "t",
            types::TIMESTAMP,
            Some(Tb::default().with(0, V::I16(4))),
            vec![],
        )),
        "cannot convert (Timestamp",
    );
    refused(
        one(field(
            "t",
            types::DURATION,
            Some(Tb::default().with(0, V::I16(-1))),
            vec![],
        )),
        "cannot convert (Duration",
    );
    refused(
        one(field(
            "t",
            types::INTERVAL,
            Some(Tb::default().with(0, V::I16(3))),
            vec![],
        )),
        "cannot convert (Interval",
    );
    refused(
        one(plain("l", types::LIST)),
        "cannot convert (List, 0 children)",
    );
    refused(
        one(field(
            "l",
            types::FIXED_SIZE_LIST,
            None,
            vec![int_field("i", 8)],
        )),
        "cannot convert (FixedSizeList, 1 children)",
    );
    refused(
        one(plain("r", types::RUN_END_ENCODED)),
        "cannot convert (RunEndEncoded",
    );
    refused(
        one(field("m", types::MAP, None, vec![int_field("i", 8)])),
        "cannot convert (Map",
    );
    refused(
        one(field(
            "m",
            types::MAP,
            Some(Tb::default()),
            vec![int_field("i", 8)],
        )),
        "is a map whose entries are not a struct of two fields",
    );
    for decimal in [
        Tb::default().with(0, V::I32(300)),
        Tb::default().with(1, V::I32(200)),
        Tb::default().with(2, V::I32(100)),
    ] {
        refused(
            one(field("d", types::DECIMAL, Some(decimal), vec![])),
            "cannot convert (Decimal",
        );
    }
    let union = |mode: i16, ids: Option<Vec<i32>>, n: usize| {
        let mut t = Tb::default().with(0, V::I16(mode));
        if let Some(ids) = ids {
            t = t.with(1, V::I32s(ids));
        }
        field(
            "u",
            types::UNION,
            Some(t),
            (0..n).map(|i| int_field(&i.to_string(), 8)).collect(),
        )
    };
    refused(one(union(2, None, 1)), "cannot convert (Union");
    refused(one(union(0, Some(vec![1, 1]), 2)), "cannot convert (Union");
    refused(one(union(0, Some(vec![-1]), 1)), "cannot convert (Union");
    refused(one(union(0, Some(vec![1]), 2)), "cannot convert (Union");
    refused(one(union(0, None, 129)), "cannot convert (Union");
    checked(vec![union(0, Some(vec![256 + 5]), 1)]).unwrap();
    refused(
        one(field("u", types::UNION, None, vec![])),
        "cannot convert (Union",
    );
    // A dictionary with no index type, or one of a width it does not map.
    refused(
        one(plain("d", types::UTF8).with(4, V::T(Tb::default()))),
        "is dictionary encoded with no index type",
    );
    refused(
        one(plain("d", types::UTF8).with(4, V::T(Tb::default().with(1, V::T(int(12)))))),
        "has a 12 bit dictionary index",
    );
    // A nested child is checked too.
    refused(
        one(field(
            "s",
            types::STRUCT,
            Some(Tb::default()),
            vec![int_field("bad", 3)],
        )),
        "field \"bad\" has a type",
    );
    // A big-endian decimal.
    let decimal = field(
        "d",
        types::DECIMAL,
        Some(Tb::default().with(0, V::I32(9))),
        vec![],
    );
    let big = build(&schema(vec![decimal]).with(0, V::I16(1)));
    refused(
        check_schema(Table::root(&big).unwrap()),
        "is a big-endian decimal",
    );
}

// ---- walk ----

struct Counting {
    taken: Vec<(String, &'static str, Rule)>,
    refuse: bool,
}

impl Plan for Counting {
    fn take(
        &mut self,
        column: &str,
        what: &'static str,
        span: Span,
        rule: Rule,
    ) -> Result<Span, String> {
        if self.refuse {
            return Err(String::from("the plan refused it"));
        }
        self.taken.push((column.to_string(), what, rule));
        Ok(Span {
            slot: self.taken.len() - 1,
            ..span
        })
    }
}

fn walked(
    fields: &[Field],
    rb: Tb,
    body: usize,
    plan: Option<&mut dyn Plan>,
) -> Result<crate::walk::Walked, String> {
    walked_at(fields, rb, body, 0, version::V5, plan)
}

fn walked_at(
    fields: &[Field],
    rb: Tb,
    body: usize,
    addr: usize,
    meta: i16,
    plan: Option<&mut dyn Plan>,
) -> Result<crate::walk::Walked, String> {
    let bytes = build(&rb);
    walk(
        Table::root(&bytes).unwrap(),
        Body { len: body, addr },
        meta,
        fields,
        plan,
    )
}

fn f(name: &str, kind: Type) -> Field {
    Field::new(name, kind, true)
}

#[test]
fn walk_reads_every_type_in_arrows_order() {
    let i32t = || Type::Primitive(Some(4));
    let fields = vec![
        f("null", Type::Null),
        f("b", Type::Boolean),
        f("i", i32t()),
        f("fsb", Type::FixedSizeBinary(2)),
        f("s", Type::Text { width: 4 }),
        f("v", Type::View),
        f(
            "l",
            Type::List {
                width: 4,
                child: Box::new(f("x", i32t())),
            },
        ),
        f(
            "lv",
            Type::ListView {
                width: 8,
                child: Box::new(f("x", i32t())),
            },
        ),
        f(
            "fsl",
            Type::FixedSizeList {
                size: 2,
                child: Box::new(f("x", i32t())),
            },
        ),
        f("st", Type::Struct(vec![f("x", i32t())])),
        f(
            "ree",
            Type::RunEndEncoded {
                run_ends: Box::new(f("r", i32t())),
                values: Box::new(f("x", Type::Text { width: 8 })),
            },
        ),
        f(
            "d",
            Type::Dictionary {
                key_width: 2,
                value: Box::new(Type::Null),
            },
        ),
        f(
            "u",
            Type::Union {
                dense: true,
                children: vec![f("x", i32t())],
            },
        ),
        f("p", Type::Primitive(None)),
    ];
    let n = |len| (len, 0);
    let b = |len| (0i64, len);
    let nodes = vec![
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(4),
        n(8),
        n(4),
        n(4),
        n(4),
        n(4),
    ];
    let buffers = vec![
        b(0),
        b(1), // bool
        b(0),
        b(16), // int
        b(0),
        b(8), // fsb
        b(0),
        b(20),
        b(8), // string
        b(0),
        b(64),
        b(4), // view, one data buffer
        b(0),
        b(20),
        b(0),
        b(16), // list and child
        b(0),
        b(32),
        b(32),
        b(0),
        b(16), // list view and child
        b(0),
        b(0),
        b(32), // fixed size list and child
        b(0),
        b(0),
        b(16), // struct and child
        b(0),
        b(16),
        b(0),
        b(40),
        b(8), // run ends, values (large)
        b(0),
        b(8), // dictionary keys
        b(4),
        b(16),
        b(0),
        b(16), // dense union: type ids, offsets, child
        b(0),
        b(0), // primitive of no width
    ];
    let rb = record_batch(4, nodes, buffers).with(4, V::I64s(vec![1]));
    let out = walked(&fields, rb.clone(), 64, None).unwrap();
    assert_eq!(out.columns.len(), fields.len());
    assert_eq!(out.spans.len(), 40);
    assert_eq!(out.columns[0], 0..0);
    // The same batch with a plan sees every buffer with the rule its column implies.
    let mut plan = Counting {
        taken: Vec::new(),
        refuse: false,
    };
    let rb = rb.with(3, V::T(Tb::default()));
    let out = walked(&fields, rb.clone(), 64, Some(&mut plan)).unwrap();
    assert_eq!(out.spans[3].slot, 3);
    let rules: Vec<Rule> = plan.taken.iter().map(|(_, _, r)| *r).collect();
    assert_eq!(rules[0], Rule::Validity(4));
    assert_eq!(rules[1], Rule::Exact(1));
    assert_eq!(rules[3], Rule::Exact(16));
    assert_eq!(rules[5], Rule::Exact(8));
    assert_eq!(rules[7], Rule::Text { len: 4, width: 4 });
    assert_eq!(rules[8], Rule::Data { offsets: 7 });
    assert_eq!(rules[11], Rule::Free);
    assert_eq!(rules[13], Rule::Offsets { len: 4, width: 4 });
    assert_eq!(*rules.last().unwrap(), Rule::Free);
    assert_eq!(plan.taken[0].0, "b");
    // A compressed batch with no plan, and a plan that refuses.
    refused(
        walked(&fields, rb.clone(), 64, None),
        "its reader inflates nothing",
    );
    let mut no = Counting {
        taken: Vec::new(),
        refuse: true,
    };
    refused(
        walked(&fields, rb, 64, Some(&mut no)),
        "the plan refused it",
    );
    assert_eq!(
        Rule::of_values(&Type::FixedSizeBinary(-1), 2),
        Rule::Exact(u64::MAX)
    );
}

#[test]
fn walk_refuses_what_the_decoder_would_panic_on() {
    let i64f = || f("x", Type::Primitive(Some(8)));
    let one = |kind: Type, nodes: Vec<(i64, i64)>, buffers: Vec<(i64, i64)>, body: usize| {
        walked(&[f("c", kind)], record_batch(4, nodes, buffers), body, None)
    };
    refused(
        walked(&[i64f()], record_batch(-1, vec![], vec![]), 0, None),
        "negative row count (-1)",
    );
    refused(
        walked(&[i64f()], Tb::default().with(2, V::Pairs(vec![])), 0, None),
        "has no field nodes",
    );
    refused(
        walked(&[i64f()], Tb::default().with(1, V::Pairs(vec![])), 0, None),
        "has no buffers",
    );
    refused(
        one(Type::Null, vec![], vec![], 0),
        "has no field node; the record batch has too few",
    );
    refused(
        one(Type::Null, vec![(4, 5)], vec![], 0),
        "field node of length 4 with 5 nulls",
    );
    refused(
        one(Type::Null, vec![(-4, 0)], vec![], 0),
        "field node of length -4",
    );
    refused(
        one(Type::Primitive(Some(8)), vec![(4, 0)], vec![(0, 0)], 0),
        "has no buffer left",
    );
    refused(
        one(
            Type::Primitive(Some(8)),
            vec![(4, 0)],
            vec![(0, 0), (8, 32)],
            16,
        ),
        "a buffer of 32 bytes at 8, outside the body of 16 bytes",
    );
    refused(
        one(Type::Primitive(Some(8)), vec![(4, 0)], vec![(-1, 0)], 16),
        "outside the body",
    );
    refused(
        one(
            Type::Primitive(Some(8)),
            vec![(16, 1)],
            vec![(0, 1), (0, 128)],
            128,
        ),
        "has 16 values and a validity bitmap of 1 bytes",
    );
    refused(
        one(
            Type::Text { width: 4 },
            vec![(4, 0)],
            vec![(0, 0), (0, 18), (0, 0)],
            64,
        ),
        "has offsets of 18 bytes, not a whole number of 4-byte values",
    );
    refused(
        one(Type::View, vec![(4, 0)], vec![], 0),
        "is a view with no variadic count",
    );
    refused(
        walked(
            &[f("c", Type::View)],
            record_batch(4, vec![(4, 0)], vec![]).with(4, V::I64s(vec![-2])),
            0,
            None,
        ),
        "negative variadic count (-2)",
    );
    refused(
        walked(
            &[i64f()],
            record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 32)]).with(4, V::I64s(vec![1])),
            32,
            None,
        ),
        "1 variadic buffer counts that no view column uses",
    );
    refused(
        one(
            Type::List {
                width: 8,
                child: Box::new(i64f()),
            },
            vec![(4, 0)],
            vec![(0, 0), (0, 12)],
            64,
        ),
        "offsets of 12 bytes, not a whole number of 8-byte values",
    );
    refused(
        one(
            Type::ListView {
                width: 4,
                child: Box::new(i64f()),
            },
            vec![(4, 0)],
            vec![(0, 0), (0, 16), (0, 6)],
            64,
        ),
        "sizes of 6 bytes",
    );
    refused(
        one(
            Type::FixedSizeList {
                size: i32::MAX,
                child: Box::new(i64f()),
            },
            vec![(i64::MAX, 0)],
            vec![(0, 0)],
            64,
        ),
        "more than can be counted",
    );
    refused(
        one(
            Type::RunEndEncoded {
                run_ends: Box::new(f("r", Type::Primitive(Some(4)))),
                values: Box::new(i64f()),
            },
            vec![(4, 0), (2, 0)],
            vec![(0, 0), (0, 6)],
            64,
        ),
        "column \"r\" has values of 6 bytes",
    );
    refused(
        one(
            Type::Dictionary {
                key_width: 4,
                value: Box::new(Type::Null),
            },
            vec![(4, 0)],
            vec![(0, 0), (0, 6)],
            64,
        ),
        "keys of 6 bytes",
    );
    let union = |dense| Type::Union {
        dense,
        children: vec![],
    };
    refused(
        one(union(false), vec![(4, 0)], vec![(0, 2)], 64),
        "a union of 4 values with 2 bytes of type ids",
    );
    refused(
        one(union(true), vec![(4, 0)], vec![(0, 4), (0, 8)], 64),
        "a dense union of 4 values with 8 bytes of offsets",
    );
    refused(
        walked_at(
            &[f("c", union(true))],
            record_batch(4, vec![(4, 0)], vec![(0, 4), (0, 16)]),
            64,
            2,
            version::V5,
            None,
        ),
        "not 4-byte aligned",
    );
    // Before V5 a union has a validity buffer first.
    let v4 = walked_at(
        &[f("c", union(false))],
        record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 4)]),
        64,
        0,
        version::V4,
        None,
    )
    .unwrap();
    assert_eq!(v4.spans.len(), 2);
}

// ---- frame ----

fn int64_record(rows: i64, nulls: bool) -> (Vec<u8>, Vec<u8>) {
    let schema = schema_message(vec![int_field("x", 64)]);
    let validity = if nulls { (rows + 7) / 8 } else { 0 };
    let rb = record_batch(
        rows,
        vec![(rows, 0)],
        vec![(0, validity), (8 * ((validity + 7) / 8), 8 * rows)],
    );
    let body = 8 * ((validity + 7) / 8) + 8 * rows;
    (schema, batch_message(rb, body))
}

#[test]
fn frame_reads_and_rewrites_the_record_batch_message() {
    let (schema, batch) = int64_record(100, true);
    assert_eq!(frame::message_len(&schema).unwrap() + 8, schema.len());
    refused(frame::message_len(&[0xff; 4]), "needs 8 bytes");
    refused(
        frame::message_len(&[0; 8]),
        "continuation marker is missing",
    );
    refused(frame::message_len(&END_OF_STREAM), "length is zero");
    refused(
        frame::message_len(&schema[..12]),
        "past the end of the buffer",
    );
    let mut framing = schema.clone();
    assert_eq!(frame::framing_target(&framing[..4]).unwrap(), None);
    assert_eq!(frame::framing_target(&framing).unwrap(), None);
    framing.extend_from_slice(&batch);
    assert_eq!(
        frame::framing_target(&framing).unwrap(),
        Some(framing.len())
    );
    let mut wrong = framing.clone();
    wrong[schema.len()] = 0;
    refused(
        frame::framing_target(&wrong),
        "record batch message's continuation marker",
    );
    refused(
        frame::framing_target(&[0u8; 8]),
        "continuation marker is missing",
    );
    assert_eq!(
        frame::buffer_entries(&batch).unwrap(),
        vec![(0, 13), (16, 800)]
    );
    refused(frame::buffer_entries(&schema), "not a record batch");
    let mut b = batch.clone();
    frame::write_entry(&mut b, 1, 4096, 800).unwrap();
    assert_eq!(frame::buffer_entries(&b).unwrap()[1], (4096, 800));
    refused(
        frame::write_entry(&mut b, 2, 0, 0),
        "buffer entry 2 of 2 does not exist",
    );
    frame::write_entry(&mut b, 0, -1, 0).unwrap();
    refused(frame::buffer_entries(&b), "negative offset or length");
    frame::write_body_length(&mut b, 9000).unwrap();
    assert_eq!(frame::body_length(&b).unwrap(), 9000);
    // A message with no body length field takes only zero.
    let mut none = whole(&build(
        &Tb::default()
            .with(0, V::I16(version::V5))
            .with(1, V::U8(header::RECORD_BATCH))
            .with(2, V::T(record_batch(0, vec![], vec![]))),
    ));
    assert_eq!(frame::body_length_at(&none).unwrap(), None);
    frame::write_body_length(&mut none, 0).unwrap();
    refused(
        frame::write_body_length(&mut none, 8),
        "no body length to set",
    );
    let no_buffers = whole(&build(
        &Tb::default()
            .with(1, V::U8(header::RECORD_BATCH))
            .with(2, V::T(Tb::default())),
    ));
    refused(frame::entries_span(&no_buffers), "has no buffers");
    let headless = whole(&build(&Tb::default().with(1, V::U8(header::RECORD_BATCH))));
    refused(frame::entries_span(&headless), "not a record batch");
    let rubbish = whole(&[9, 9, 9, 9]);
    refused(frame::entries_span(&rubbish), "record batch message:");
    assert_eq!(frame::round_up(4097, 4096), 8192);
}

#[test]
fn frame_lays_a_record_out_page_by_page() {
    let (schema, batch) = int64_record(100, true);
    let mut framing = schema.clone();
    framing.extend_from_slice(&batch);
    let placed = frame::page_layout(&mut framing, &[13, 800], PAGE, 8192).unwrap();
    assert_eq!(placed, vec![8192 + 4096, 8192 + 8192]);
    let rb = &framing[schema.len()..];
    let n = framing.len() as u64;
    assert_eq!(
        frame::buffer_entries(rb).unwrap(),
        vec![(4096 - n, 13), (8192 - n, 800)]
    );
    assert_eq!(
        frame::body_length(rb).unwrap() as u64,
        frame::round_up(8192 - n + 800, 8)
    );
    refused(
        frame::page_layout(&mut framing.clone(), &[], 0, 0),
        "page size is zero",
    );
    refused(
        frame::page_layout(&mut framing.clone(), &[], PAGE, 1),
        "not a multiple of 4096",
    );
    // An empty buffer's offset is clamped to the body's end.
    let (schema, batch) = int64_record(0, false);
    let mut framing = schema.clone();
    framing.extend_from_slice(&batch);
    frame::page_layout(&mut framing, &[0, 0], PAGE, 0).unwrap();
    let rb = &framing[schema.len()..];
    assert_eq!(frame::buffer_entries(rb).unwrap(), vec![(0, 0), (0, 0)]);
}

// ---- record ----

/// A record of one `Int64` column of `rows` values, laid out as `encode_framing` lays it out,
/// with its values written.
fn int64_plane(rows: i64) -> (Vec<u8>, usize) {
    let (schema, batch) = int64_record(rows, false);
    let laid = reframe(&schema, &batch).unwrap();
    let mut bytes = laid.framing.clone();
    let n = bytes.len();
    bytes.resize(laid.record_len as usize, 0);
    (bytes, n)
}

#[test]
fn record_framing_need_reads_the_two_lengths() {
    let (bytes, n) = int64_plane(10);
    assert_eq!(framing_need(&bytes[..3]).unwrap(), Need::More(8));
    let first = 8 + frame::message_len(&bytes).unwrap();
    assert_eq!(framing_need(&bytes[..8]).unwrap(), Need::More(first + 8));
    assert_eq!(framing_need(&bytes[..first + 8]).unwrap(), Need::Framing(n));
    assert_eq!(framing_need(&bytes).unwrap(), Need::Framing(n));
    refused(
        framing_need(&[1, 2, 3, 4, 5]),
        "first message does not start",
    );
    let mut second = bytes.clone();
    second[first] = 0;
    refused(framing_need(&second), "second message does not start");
    refused(
        framing_need(&END_OF_STREAM),
        "first message has a length of zero",
    );
    let mut zero = bytes.clone();
    zero[first + 4..first + 8].copy_from_slice(&0u32.to_le_bytes());
    refused(framing_need(&zero), "second message has a length of zero");
    let mut huge = bytes.clone();
    huge[4..8].copy_from_slice(&(FRAMING_MAX as u32).to_le_bytes());
    refused(framing_need(&huge), "past the 1048576 bytes");
    let mut huge = bytes.clone();
    huge[first + 4..first + 8].copy_from_slice(&(FRAMING_MAX as u32).to_le_bytes());
    refused(framing_need(&huge), "past the 1048576 bytes");
    let (s, b) = stream_parts(&bytes[..n]).unwrap();
    assert_eq!((s.start, s.end, b.end), (0, first, n));
    refused(stream_parts(&bytes[..n + 8]), "its two messages take");
    refused(stream_parts(&bytes[..first + 2]), "need at least");
}

#[test]
fn record_check_passes_a_record_and_names_its_buffers() {
    let (bytes, n) = int64_plane(1000);
    let r = check_record(&bytes[..n], bytes.len() as u64).unwrap();
    assert_eq!(r.body_start, n as u64);
    assert_eq!(r.rows, 1000);
    assert_eq!(r.body_length, bytes.len() as u64 - n as u64);
    assert_eq!(
        r.fields,
        vec![Field::new("x", Type::Primitive(Some(8)), true)]
    );
    assert_eq!(r.buffers, vec![(PAGE, 0), (PAGE, 8000)]);
    assert_eq!(r.columns.len(), 1);
    assert_eq!(r.columns[0].name, "x");
    assert_eq!(r.columns[0].buffers, r.buffers);
    // More room after the record than it declares is what a window leaves.
    check_record(&bytes[..n], bytes.len() as u64 + 3 * PAGE).unwrap();
}

#[test]
fn record_check_refuses_what_is_not_a_plane_record() {
    let (bytes, n) = int64_plane(1000);
    let len = bytes.len() as u64;
    refused(
        check_record(&bytes[..n], n as u64 - 1),
        "framing alone takes",
    );
    refused(check_record(&bytes[..n], len - 8), "the record holds");
    refused(check_record(&bytes[..n + 8], len), "its two messages take");
    let first = 8 + frame::message_len(&bytes).unwrap();
    // The first message is not a schema: two batch messages.
    let mut two = bytes[first..n].to_vec();
    two.extend_from_slice(&bytes[first..n]);
    refused(
        check_record(&two, 1 << 16),
        "first message is a RecordBatch message",
    );
    // The second is not a record batch.
    let mut schemas = bytes[..first].to_vec();
    schemas.extend_from_slice(&bytes[..first]);
    refused(
        check_record(&schemas, 1 << 16),
        "second message is a Schema message",
    );
    // A schema refusal.
    let bad = schema_message(vec![int_field("x", 3)]);
    let mut framing = bad.clone();
    framing.extend_from_slice(&bytes[first..n]);
    refused(check_record(&framing, 1 << 16), "cannot convert (Int");
    // A dictionary column.
    let dict = schema_message(vec![
        plain("d", types::UTF8).with(4, V::T(Tb::default().with(1, V::T(int(8))))),
    ]);
    let mut framing = dict.clone();
    framing.extend_from_slice(&bytes[first..n]);
    refused(check_record(&framing, 1 << 16), "carries no dictionaries");
    // A version that is not V4 or V5, and a header with no table.
    let old = whole(&build(
        &Tb::default()
            .with(0, V::I16(version::V1))
            .with(1, V::U8(header::RECORD_BATCH))
            .with(2, V::T(record_batch(0, vec![], vec![]))),
    ));
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&old);
    refused(check_record(&framing, 1 << 16), "metadata version V1");
    let headless = whole(&build(
        &Tb::default()
            .with(0, V::I16(version::V5))
            .with(1, V::U8(header::RECORD_BATCH)),
    ));
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&headless);
    refused(
        check_record(&framing, 1 << 16),
        "second message has no header",
    );
    // A message that is no flatbuffer.
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&whole(&[200, 0, 0, 0]));
    refused(
        check_record(&framing, 1 << 16),
        "second message is not a flatbuffer",
    );
    // A compressed record.
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 32)])
        .with(3, V::T(Tb::default().with(compression::CODEC, V::I8(1))));
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&batch_message(rb, 32));
    refused(
        check_record(&framing, 1 << 16),
        "compressed with ZSTD, and a data-plane record may inflate 0 bytes",
    );
    // A negative body length, and a buffer the walk refuses.
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 32)]);
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&batch_message(rb.clone(), -8));
    refused(
        check_record(&framing, 1 << 16),
        "declares a body of -8 bytes",
    );
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (1 << 20, 32)]);
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&batch_message(rb, 0));
    refused(check_record(&framing, 1 << 16), "outside the body of");
    // A buffer off its page.
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (8, 32)]);
    let mut framing = bytes[..first].to_vec();
    framing.extend_from_slice(&batch_message(rb, 40));
    refused(check_record(&framing, 1 << 16), "not on a page boundary");
}

#[test]
fn record_reframe_refuses_what_it_cannot_lay_out() {
    let (schema, batch) = int64_record(4, false);
    refused(
        reframe(&schema[..schema.len() - 8], &batch),
        "the schema message: a message of",
    );
    refused(
        reframe(&schema, &[0u8; 4]),
        "record batch message: a message header",
    );
    let mut longer = batch.clone();
    longer.extend_from_slice(&[0; 8]);
    refused(reframe(&schema, &longer), "record batch message is");
    refused(
        reframe(&schema, &schema),
        "second message is a Schema message",
    );
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 32)]).with(3, V::T(Tb::default()));
    refused(
        reframe(&schema, &batch_message(rb, 32)),
        "compressed with LZ4_FRAME",
    );
    let rb = record_batch(4, vec![(4, 0)], vec![(0, 0), (0, 32)]);
    refused(
        reframe(&schema, &batch_message(rb.clone(), -1)),
        "declares a body of -1 bytes",
    );
    refused(
        reframe(&schema, &batch_message(rb, 16)),
        "buffer 1 of 32 bytes at 0 is outside the body of 16 bytes",
    );
    let laid = reframe(&schema, &batch).unwrap();
    assert_eq!(laid.moves, vec![(0, PAGE, 32)]);
    assert_eq!(laid.record_len, PAGE + 32);
}
