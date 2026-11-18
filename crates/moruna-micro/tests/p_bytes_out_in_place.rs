//! 6.3h: what the engine counts for a function's answer is the answer's bytes, whichever way
//! the answer reached it.
//!
//! A stage's `bytes_out` is the sum of `Payload::bytes` over the payloads its kernel gave
//! (`moruna-scheduler`'s worker), and the same figure is the morsel's charge downstream, the
//! stage's learned growth and the controller's amplification. An image task's answer reaches
//! the engine in one of three ways (`moruna_micro::task`): kept in place over the record the
//! task wrote (`in_place::decode`), copied into the run's arena (`land_table`), or as
//! several records joined and landed.
//!
//! In 6.2 run 3 the three did not agree. An answer kept in place counted the whole record
//! once for each of its buffers, so Enrich's reports said 14.8 to 18.7 bytes out per byte in,
//! where its output is 1.17 times its input, and Score's said 0.22 in the runs whose answers
//! were mostly joined and 0.73 in the runs whose answers were mostly kept.

use std::sync::Arc;

use moruna_adapters::land::land_table;
use moruna_kernel::arrow::array::{Array, ArrayRef, Float64Array, Int64Array, StringArray};
use moruna_kernel::arrow::buffer::{Buffer as ArrowBuffer, MutableBuffer};
use moruna_kernel::arrow::compute::concat_batches;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Allocator, Payload, Tier, ipc};
use moruna_micro::in_place;
use moruna_micro::layout::{self, PAGE, Piece};
use moruna_testkit::FakeAllocator;

/// Rows of a piece of the benchmark's morsels.
const ROWS: usize = 65_536;

/// Taxi trips `from .. from + rows`: three `int64` columns, a `double`, a `double` with nulls
/// and a string of three bytes, 47.1 bytes a row.
fn trips(from: usize, rows: usize) -> RecordBatch {
    let at = |r: usize| (from + r) as i64;
    let ints = |scale: i64| -> ArrayRef {
        Arc::new(Int64Array::from(
            (0..rows).map(|r| at(r) * scale).collect::<Vec<_>>(),
        ))
    };
    let fare: Float64Array = (0..rows)
        .map(|r| (at(r) % 7 != 0).then_some(at(r) as f64 * 0.25))
        .collect();
    let airport: StringArray = (0..rows)
        .map(|r| Some(["JFK", "LGA", "EWR"][(from + r) % 3]))
        .collect();
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("pickup_at", ints(1_000_000)),
        ("dropoff_at", ints(1_000_003)),
        ("pu_location_id", ints(1)),
        (
            "trip_distance",
            Arc::new(Float64Array::from(
                (0..rows).map(|r| at(r) as f64 * 0.5).collect::<Vec<_>>(),
            )),
        ),
        ("fare_amount", Arc::new(fare)),
        ("airport", Arc::new(airport)),
    ];
    batch(columns)
}

fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    let arrays = columns.into_iter().map(|(_, array)| array).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("a batch")
}

/// The function: its input's columns and one `double` more, 1.17 times its input's bytes.
fn enrich(input: &RecordBatch) -> RecordBatch {
    let pickup = input
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("pickup_at");
    let dropoff = input
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("dropoff_at");
    let duration: Float64Array = pickup
        .values()
        .iter()
        .zip(dropoff.values())
        .map(|(p, d)| Some((d - p) as f64 / 1e6))
        .collect();
    let schema = input.schema();
    let mut fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    let mut arrays = input.columns().to_vec();
    fields.push(Arc::new(Field::new("duration_s", DataType::Float64, true)));
    arrays.push(Arc::new(duration));
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).expect("the answer")
}

/// `batch` as the record a task writes in its window: its framing, then each body buffer on
/// its page, in one stretch of memory.
fn record(batch: &RecordBatch, alloc: &dyn Allocator) -> ArrowBuffer {
    let (framing, placed) =
        ipc::encode_framing(batch, PAGE as usize, 0, alloc).expect("a record's framing");
    let spans: Vec<(u64, u64)> = placed
        .iter()
        .map(|(at, b)| (*at as u64, b.len() as u64))
        .collect();
    let len = layout::record_len(framing.len() as u64, &spans).expect("a record's length");
    let body: Vec<(u64, &[u8])> = placed
        .iter()
        .map(|(at, b)| (*at as u64, b.as_slice()))
        .collect();
    let mut bytes = MutableBuffer::from_len_zeroed(len as usize);
    for piece in layout::pieces(len, &framing, &body).expect("the buffers fit the record") {
        if let Piece::Bytes(at, b) = piece {
            bytes.as_slice_mut()[at as usize..at as usize + b.len()].copy_from_slice(b);
        }
    }
    bytes.into()
}

/// The record kept in place, as `TaskState::output` hands it on (`rings::payload_over`).
fn kept(record: ArrowBuffer) -> Payload {
    Payload::Table(
        in_place::decode(record).expect("the record decodes in place"),
        Tier::Host,
    )
}

/// The record copied into the run's arena, as `TaskState::land` does.
fn landed(record: ArrowBuffer, alloc: &Arc<dyn Allocator>) -> Payload {
    let decoded = ipc::decode(record, PAGE as usize).expect("the record decodes");
    land_table(decoded, alloc).expect("the record lands").0
}

/// Several records answering one batch, joined and landed, as `TaskState::join` does.
fn joined(parts: &[Payload], alloc: &Arc<dyn Allocator>) -> Payload {
    let batches: Vec<RecordBatch> = parts
        .iter()
        .map(|p| match p {
            Payload::Table(b, _) => b.clone(),
            Payload::Tensor(..) => panic!("a record is a table"),
        })
        .collect();
    let one = concat_batches(&batches[0].schema(), &batches).expect("the records join");
    land_table(one, alloc).expect("the joined batch lands").0
}

fn table(payload: &Payload) -> &RecordBatch {
    match payload {
        Payload::Table(b, _) => b,
        Payload::Tensor(..) => panic!("a record is a table"),
    }
}

#[test]
fn p_an_answer_of_1_17_times_its_input_counts_that_in_place_copied_or_joined() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let input = trips(0, ROWS);
    let answer = enrich(&input);
    // What the stage is charged for its input: the batch in the run's arena, as a source
    // lands it.
    let bytes_in = land_table(input, &alloc)
        .expect("the input lands")
        .0
        .bytes();
    let whole = record(&answer, alloc.as_ref());

    // One record, kept in place.
    let in_place = kept(whole.clone());
    // The same record, copied: the task had no window to spare.
    let copied = landed(whole.clone(), &alloc);
    // The same answer in three records (an answer bigger than a window, or three pieces'
    // answers taken together): the first kept, each after it landed, then joined.
    let cuts = [0, ROWS / 3, 2 * ROWS / 3, ROWS];
    let parts: Vec<Payload> = cuts
        .windows(2)
        .enumerate()
        .map(|(i, cut)| {
            let part = record(&enrich(&trips(cut[0], cut[1] - cut[0])), alloc.as_ref());
            if i == 0 {
                kept(part)
            } else {
                landed(part, &alloc)
            }
        })
        .collect();
    let several = joined(&parts, &alloc);

    let ways = [
        ("kept in place", &in_place),
        ("copied into the arena", &copied),
        ("in three records, joined", &several),
    ];
    for (how, payload) in ways {
        eprintln!(
            "the answer {how}: {} bytes out for {bytes_in} bytes in, {:.2} times",
            payload.bytes(),
            payload.bytes() as f64 / bytes_in as f64
        );
    }
    for (how, payload) in ways {
        assert_eq!(table(payload), &answer, "the answer {how} is the answer");
        let ratio = payload.bytes() as f64 / bytes_in as f64;
        assert!(
            (ratio - 1.17).abs() < 0.01,
            "the answer {how} counts {} bytes for {bytes_in} bytes in, {ratio:.2} times, \
             where the function's output is 1.17 times its input (the record is {} bytes)",
            payload.bytes(),
            whole.len(),
        );
    }
    // The first of several records is one the stage may also hand on by itself.
    let first = parts[0].bytes() as f64 / several.bytes() as f64;
    assert!(
        (first - 1.0 / 3.0).abs() < 0.01,
        "a third of the answer, kept in place, counts {first:.2} of the whole"
    );
}

#[test]
fn p_a_record_kept_in_place_is_not_copied_and_is_held_until_its_last_buffer_goes() {
    let alloc = FakeAllocator::new();
    let answer = enrich(&trips(0, 1000));
    let whole = record(&answer, &alloc);
    let (start, end) = (
        whole.as_ptr() as usize,
        whole.as_ptr() as usize + whole.len(),
    );
    let in_place = in_place::decode(whole.clone()).expect("the record decodes in place");
    assert_eq!(in_place, answer);
    let mut buffers = Vec::new();
    for column in in_place.columns() {
        let data = column.to_data();
        buffers.extend(data.buffers().iter().cloned());
        buffers.extend(data.nulls().map(|n| n.buffer().clone()));
    }
    // 3 int64, a double, a double with its validity, a string's offsets and bytes, a double.
    assert_eq!(buffers.len(), 9);
    for b in &buffers {
        let at = b.as_ptr() as usize;
        assert!(
            start <= at && at + b.len() <= end,
            "a buffer of the batch lies in the record: nothing was copied"
        );
    }
    // The record's memory stays while any buffer of the batch does, and no longer.
    drop(in_place);
    assert!(
        whole.strong_count() > 1,
        "one buffer still holds the record"
    );
    let last = buffers.pop().expect("a buffer");
    drop(buffers);
    assert!(whole.strong_count() > 1, "the last buffer holds the record");
    drop(last);
    assert_eq!(whole.strong_count(), 1, "the record is the caller's alone");
}
