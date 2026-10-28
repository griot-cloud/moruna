//! Arrow IPC streams for the malformed-IPC tests of every decode site (contracts d.15, CT-I13).
//!
//! `streams` gives valid streams that between them hold every type the IPC reader converts,
//! with nulls, dictionaries, views with variadic buffers, unions of both modes and both metadata
//! versions, run-end encoding and maps; `corruptions` derives from one stream the malformed
//! ones a site is fed: every metadata byte set to each of `VALUES`, random byte replacements
//! and truncations. `Rng` is the seeded generator, so a failing case is reproducible from its
//! label. `unknown_type` is the crafted stream of the first test of each site.

use std::sync::Arc;

use moruna_kernel::arrow;
use moruna_kernel::arrow::array::{
    ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array,
    Decimal128Array, Decimal256Array, DictionaryArray, DurationSecondArray, FixedSizeBinaryArray,
    FixedSizeListArray, Float16Array, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, IntervalMonthDayNanoArray, LargeBinaryArray, LargeListArray,
    LargeStringArray, ListArray, ListViewArray, MapArray, NullArray, RunArray, StringArray,
    StringViewArray, StructArray, Time32MillisecondArray, Time64NanosecondArray,
    TimestampMicrosecondArray, UInt16Array, UInt64Array, UnionArray,
};
use moruna_kernel::arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use moruna_kernel::arrow::datatypes::{
    DataType, Field, Fields, Int32Type, IntervalMonthDayNano, Schema, UnionFields, i256,
};
use moruna_kernel::arrow::ipc::MetadataVersion;
use moruna_kernel::arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use moruna_kernel::arrow::record_batch::RecordBatch;

/// The values every metadata byte is set to in turn: the edges of a byte, small type ids and
/// widths, and the 229 of an unknown type id.
pub const VALUES: [u8; 8] = [0x00, 0x01, 0x02, 0x07, 0x7F, 0x80, 0xE5, 0xFF];

/// A seeded SplitMix64 generator.
pub struct Rng(u64);

impl Rng {
    /// A generator that yields the same numbers for the same seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next number.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`, which is not zero.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// `batches` as one IPC stream written with `version` metadata.
pub fn stream(batches: &[RecordBatch], version: MetadataVersion) -> Vec<u8> {
    let schema = batches[0].schema();
    let options = IpcWriteOptions::try_new(8, false, version).expect("write options");
    let mut out = Vec::new();
    let mut writer =
        StreamWriter::try_new_with_options(&mut out, &schema, options).expect("a stream writer");
    for batch in batches {
        writer.write(batch).expect("a batch written");
    }
    writer.finish().expect("the stream finished");
    drop(writer);
    out
}

fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns.into_iter().map(|(_, a)| a).collect(),
    )
    .expect("a batch")
}

/// The flat and nested types, four rows with nulls.
fn every_type() -> RecordBatch {
    let ints = || Arc::new(Int32Array::from(vec![Some(1), None, Some(3), Some(4)])) as ArrayRef;
    let item = Arc::new(Field::new("item", DataType::Int32, true));
    let list = ListArray::new(
        item.clone(),
        OffsetBuffer::new(ScalarBuffer::from(vec![0, 1, 1, 3, 4])),
        ints(),
        Some(vec![true, false, true, true].into()),
    );
    let large_list = LargeListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(ScalarBuffer::from(vec![0i64, 2, 2, 3, 4])),
        Arc::new(StringArray::from(vec![
            Some("a"),
            None,
            Some("ccc"),
            Some("d"),
        ])),
        None,
    );
    let fixed_list = FixedSizeListArray::new(
        Arc::new(Field::new("item", DataType::Int16, true)),
        2,
        Arc::new(Int16Array::from(vec![
            Some(1),
            Some(2),
            None,
            Some(4),
            Some(5),
            Some(6),
            Some(7),
            None,
        ])),
        Some(vec![true, true, false, true].into()),
    );
    let list_view = ListViewArray::new(
        item,
        ScalarBuffer::from(vec![0, 1, 2, 0]),
        ScalarBuffer::from(vec![1, 0, 2, 4]),
        ints(),
        Some(vec![true, false, true, true].into()),
    );
    let structs = StructArray::new(
        Fields::from(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ]),
        vec![
            ints(),
            Arc::new(StringArray::from(vec![
                Some("x"),
                Some("y"),
                None,
                Some("z"),
            ])),
        ],
        Some(vec![true, true, true, false].into()),
    );
    let entries = Fields::from(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("value", DataType::Int64, true),
    ]);
    let map = MapArray::new(
        Arc::new(Field::new(
            "entries",
            DataType::Struct(entries.clone()),
            false,
        )),
        OffsetBuffer::new(ScalarBuffer::from(vec![0, 2, 2, 3, 3])),
        StructArray::new(
            entries,
            vec![
                Arc::new(StringArray::from(vec!["k1", "k2", "k3"])),
                Arc::new(Int64Array::from(vec![Some(10), None, Some(30)])),
            ],
            None,
        ),
        Some(vec![true, false, true, true].into()),
        false,
    );
    let union_fields = UnionFields::try_new(
        vec![0, 3],
        vec![
            Field::new("i", DataType::Int32, true),
            Field::new("s", DataType::Utf8, true),
        ],
    )
    .expect("union fields");
    let dense = UnionArray::try_new(
        union_fields.clone(),
        ScalarBuffer::from(vec![0i8, 3, 0, 3]),
        Some(ScalarBuffer::from(vec![0, 0, 1, 1])),
        vec![
            Arc::new(Int32Array::from(vec![Some(7), None])),
            Arc::new(StringArray::from(vec!["u", "v"])),
        ],
    )
    .expect("a dense union");
    let sparse = UnionArray::try_new(
        union_fields,
        ScalarBuffer::from(vec![3i8, 0, 0, 3]),
        None,
        vec![
            Arc::new(Int32Array::from(vec![None, Some(1), Some(2), None])),
            Arc::new(StringArray::from(vec![Some("p"), None, None, Some("q")])),
        ],
    )
    .expect("a sparse union");
    let runs = RunArray::<Int32Type>::try_new(
        &Int32Array::from(vec![2, 4]),
        &StringArray::from(vec![Some("r"), None]),
    )
    .expect("a run array");
    let long = "a string longer than twelve bytes";
    batch(vec![
        ("null", Arc::new(NullArray::new(4))),
        (
            "bool",
            Arc::new(BooleanArray::from(vec![
                Some(true),
                None,
                Some(false),
                Some(true),
            ])),
        ),
        (
            "i8",
            Arc::new(Int8Array::from(vec![Some(-1), None, Some(2), Some(3)])),
        ),
        ("u16", Arc::new(UInt16Array::from(vec![1, 2, 3, 4]))),
        ("i32", ints()),
        (
            "u64",
            Arc::new(UInt64Array::from(vec![Some(9), Some(8), None, Some(6)])),
        ),
        (
            "f16",
            // 1.5, a null, 1.0 and 0.0 as IEEE half-precision bits.
            Arc::new(Float16Array::new(
                ScalarBuffer::new(Buffer::from_vec(vec![0x3E00u16, 0, 0x3C00, 0]), 0, 4),
                Some(vec![true, false, true, true].into()),
            )),
        ),
        (
            "f32",
            Arc::new(Float32Array::from(vec![1.0, 2.0, 3.0, 4.0])),
        ),
        (
            "f64",
            Arc::new(Float64Array::from(vec![Some(0.5), None, None, Some(4.5)])),
        ),
        (
            "utf8",
            Arc::new(StringArray::from(vec![
                Some("a"),
                None,
                Some(long),
                Some(""),
            ])),
        ),
        (
            "large_utf8",
            Arc::new(LargeStringArray::from(vec!["a", "bb", "ccc", "d"])),
        ),
        (
            "binary",
            Arc::new(BinaryArray::from(vec![&b"x"[..], b"", b"yz", b"w"])),
        ),
        (
            "large_binary",
            Arc::new(LargeBinaryArray::from(vec![
                Some(&b"x"[..]),
                None,
                Some(b"y"),
                Some(b"z"),
            ])),
        ),
        (
            "utf8_view",
            Arc::new(StringViewArray::from(vec![
                Some("short"),
                None,
                Some(long),
                Some(long),
            ])),
        ),
        (
            "binary_view",
            Arc::new(BinaryViewArray::from(vec![
                Some(long.as_bytes()),
                Some(&b"b"[..]),
                None,
                Some(long.as_bytes()),
            ])),
        ),
        (
            "fixed_binary",
            Arc::new(
                FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                    vec![Some(b"abc"), None, Some(b"def"), Some(b"ghi")].into_iter(),
                    3,
                )
                .expect("fixed size binary"),
            ),
        ),
        (
            "date32",
            Arc::new(Date32Array::from(vec![Some(1), None, Some(3), Some(4)])),
        ),
        ("date64", Arc::new(Date64Array::from(vec![1, 2, 3, 4]))),
        (
            "time32",
            Arc::new(Time32MillisecondArray::from(vec![1, 2, 3, 4])),
        ),
        (
            "time64",
            Arc::new(Time64NanosecondArray::from(vec![
                Some(1),
                None,
                Some(3),
                Some(4),
            ])),
        ),
        (
            "timestamp",
            Arc::new(TimestampMicrosecondArray::from(vec![1, 2, 3, 4]).with_timezone("UTC")),
        ),
        (
            "duration",
            Arc::new(DurationSecondArray::from(vec![1, 2, 3, 4])),
        ),
        (
            "interval",
            Arc::new(IntervalMonthDayNanoArray::from(vec![
                IntervalMonthDayNano::new(1, 2, 3),
                IntervalMonthDayNano::new(0, 0, 0),
                IntervalMonthDayNano::new(-1, 5, 9),
                IntervalMonthDayNano::new(2, 2, 2),
            ])),
        ),
        (
            "decimal128",
            Arc::new(
                Decimal128Array::from(vec![Some(123), None, Some(-5), Some(0)])
                    .with_precision_and_scale(10, 2)
                    .expect("decimal"),
            ),
        ),
        (
            "decimal256",
            Arc::new(
                Decimal256Array::from(vec![i256::from(1), i256::from(-2), i256::ZERO, i256::ONE])
                    .with_precision_and_scale(40, 3)
                    .expect("decimal"),
            ),
        ),
        ("list", Arc::new(list)),
        ("large_list", Arc::new(large_list)),
        ("fixed_list", Arc::new(fixed_list)),
        ("list_view", Arc::new(list_view)),
        ("struct", Arc::new(structs)),
        ("map", Arc::new(map)),
        ("dense", Arc::new(dense)),
        ("sparse", Arc::new(sparse)),
        ("runs", Arc::new(runs)),
    ])
}

/// Dictionary-encoded columns: two batches, so the second carries its own dictionary messages.
fn dictionaries() -> Vec<RecordBatch> {
    let make = |keys: Vec<Option<i32>>, values: Vec<&str>| {
        batch(vec![
            (
                "dict",
                Arc::new(
                    DictionaryArray::try_new(
                        Int32Array::from(keys),
                        Arc::new(StringArray::from(values)),
                    )
                    .expect("a dictionary"),
                ) as ArrayRef,
            ),
            ("n", Arc::new(Int64Array::from(vec![1, 2, 3]))),
        ])
    };
    vec![
        make(vec![Some(0), None, Some(1)], vec!["red", "green"]),
        make(
            vec![Some(2), Some(0), Some(1)],
            vec!["blue", "cyan", "teal"],
        ),
    ]
}

/// One Int32 column of four rows: the smallest stream, cheap to corrupt exhaustively.
pub fn small() -> RecordBatch {
    batch(vec![(
        "i",
        Arc::new(Int32Array::from(vec![Some(1), None, Some(3), Some(4)])),
    )])
}

/// Valid batches that between them hold every type the IPC reader converts, each labelled with
/// the metadata version they are written with.
pub fn batches() -> Vec<(&'static str, Vec<RecordBatch>, MetadataVersion)> {
    let types = every_type();
    let unions = types.project(&[0, 30, 31]).expect("the union columns");
    vec![
        ("small", vec![small()], MetadataVersion::V5),
        ("every_type", vec![types], MetadataVersion::V5),
        ("unions_v4", vec![unions], MetadataVersion::V4),
        ("dictionaries", dictionaries(), MetadataVersion::V5),
    ]
}

/// `batches`, each written as one IPC stream.
pub fn streams() -> Vec<(&'static str, Vec<u8>)> {
    batches()
        .into_iter()
        .map(|(label, batches, version)| (label, stream(&batches, version)))
        .collect()
}

/// The span of each message of `stream`: its flatbuffer and its body, as `(start, len)`, up to
/// the end-of-stream marker or the first thing that is not a message.
pub fn messages(stream: &[u8]) -> Vec<((usize, usize), (usize, usize))> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= stream.len() {
        let len = u32::from_le_bytes([
            stream[at + 4],
            stream[at + 5],
            stream[at + 6],
            stream[at + 7],
        ]) as usize;
        if len == 0 {
            break;
        }
        let start = at + 8;
        let Some(message) = stream
            .get(start..start + len)
            .and_then(|m| arrow::ipc::root_as_message(m).ok())
        else {
            break;
        };
        let body = (message.bodyLength().max(0) as usize).min(stream.len() - start - len);
        out.push(((start, len), (start + len, body)));
        at = start + len + body;
    }
    out
}

/// One malformed variant of a stream, as an edit of it.
enum Edit {
    /// One byte set to a value.
    Set(usize, u8),
    /// Random bytes replaced: the case number and each position with its value.
    Random(usize, Vec<(usize, u8)>),
    /// The stream cut to a length.
    Cut(usize),
}

/// The malformed streams derived from `stream`, each labelled and made only when the iterator
/// reaches it: every byte of every message's flatbuffer set to each of `VALUES`, `random` cases
/// of one to four random bytes replaced anywhere, every truncation within three bytes of a
/// message's edges, and `random` truncations at random lengths.
pub fn corruptions(
    stream: &[u8],
    seed: u64,
    random: usize,
) -> impl Iterator<Item = (String, Vec<u8>)> + '_ {
    let mut rng = Rng::new(seed);
    let mut edits = Vec::new();
    let spans = messages(stream);
    for ((start, len), _) in &spans {
        for (at, byte) in stream.iter().enumerate().skip(*start).take(*len) {
            for value in VALUES {
                if *byte != value {
                    edits.push(Edit::Set(at, value));
                }
            }
        }
    }
    for case in 0..random {
        let count = 1 + rng.below(4);
        let bytes = (0..count)
            .map(|_| (rng.below(stream.len()), rng.next_u64() as u8))
            .collect();
        edits.push(Edit::Random(case, bytes));
    }
    let mut cuts: Vec<usize> = Vec::new();
    for ((start, len), (body, body_len)) in &spans {
        for edge in [start - 8, *start, start + len, *body, body + body_len] {
            for delta in 0..=6 {
                cuts.push((edge + delta).saturating_sub(3));
            }
        }
    }
    for _ in 0..random {
        cuts.push(rng.below(stream.len()));
    }
    cuts.sort_unstable();
    cuts.dedup();
    edits.extend(
        cuts.into_iter()
            .filter(|&c| c < stream.len())
            .map(Edit::Cut),
    );
    edits.into_iter().map(move |edit| match edit {
        Edit::Set(at, value) => {
            let mut bad = stream.to_vec();
            bad[at] = value;
            (format!("byte {at} = {value:#04x}"), bad)
        }
        Edit::Random(case, bytes) => {
            let mut bad = stream.to_vec();
            for (at, value) in &bytes {
                bad[*at] = *value;
            }
            (
                format!("random {case} ({} bytes, seed {seed:#x})", bytes.len()),
                bad,
            )
        }
        Edit::Cut(cut) => (format!("cut at {cut}"), stream[..cut].to_vec()),
    })
}

/// `stream` with its first field's type id set to 229, which no Arrow type has: the IPC reader's
/// schema converter panics on it (`unimplemented!("Type <UNKNOWN 229> not supported")`).
pub fn unknown_type(stream: &[u8]) -> Vec<u8> {
    let ((start, len), _) = messages(stream)[0];
    for at in start..start + len {
        let mut bad = stream.to_vec();
        bad[at] = 229;
        let hit = arrow::ipc::root_as_message(&bad[start..start + len])
            .ok()
            .and_then(|m| m.header_as_schema())
            .and_then(|s| s.fields())
            .is_some_and(|f| !f.is_empty() && f.get(0).type_type().0 == 229);
        if hit {
            return bad;
        }
    }
    panic!("the schema message has no type id byte")
}

thread_local! {
    /// Set while `panics` runs cases on this thread, so the panics it catches are not printed.
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The cases `decode` panicked on, each with its panic message; a case it returned from, with
/// an error or a value, is not listed. Panics on other threads are printed as usual.
pub fn panics<T, E>(
    cases: impl IntoIterator<Item = (String, Vec<u8>)>,
    decode: impl Fn(Vec<u8>) -> Result<T, E>,
) -> Vec<String> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(|q| q.get()) {
                previous(info);
            }
        }));
    });
    QUIET.with(|q| q.set(true));
    let mut out = Vec::new();
    for (label, bytes) in cases {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(decode(bytes));
        }));
        if let Err(payload) = outcome {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            out.push(format!("{label}: {message}"));
        }
    }
    QUIET.with(|q| q.set(false));
    out
}

/// `failed` (as `panics` lists them) counted by panic message with each number blanked, the most
/// frequent first: what a failing never-panics test prints.
pub fn tally(failed: &[String]) -> String {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for case in failed {
        let message = case.split_once(": ").map_or(case.as_str(), |(_, m)| m);
        let mut stem = String::new();
        for c in message.chars() {
            if !c.is_ascii_digit() {
                stem.push(c);
            } else if !stem.ends_with('#') {
                stem.push('#');
            }
        }
        *counts.entry(stem).or_default() += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    let mut out = format!("{} cases panicked:\n", failed.len());
    for (stem, n) in rows {
        out.push_str(&format!("{n:>7}  {stem}\n"));
    }
    if let Some(first) = failed.first() {
        out.push_str(&format!("first: {first}\n"));
    }
    out
}
