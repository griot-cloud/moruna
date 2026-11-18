//! 6.3k: a table read back from staging counts its bytes once.
//!
//! A morsel's bytes are its payload's (`Payload::bytes`, contracts CT-I3: Arrow's
//! `get_array_memory_size` for a table), recomputed whenever the payload is replaced. A morsel
//! evicted to disk comes back as one read into one arena buffer, the record's header page and
//! its body, decoded in place (`staging::read::decode_record`): every array of the batch is a
//! slice of that one buffer. Arrow counts a buffer as the whole allocation it is a slice of, so
//! the batch that comes back counts the whole read once for each of its buffers, and `pop`
//! gives a morsel whose `bytes` are that (`Entry::into_morsel`): what the stage that takes it
//! reports as its bytes in, what it claims, and what its growth is learned against.
//!
//! The same defect as 6.3h's, found there for a function's answer kept in place on the Moruna
//! kernel. Here it needs no kernel: any run with a disk tier that evicts a table.

mod common;

use std::sync::Arc;

use moruna_kernel::arrow::array::{ArrayData, ArrayRef, make_array};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Locality, Morsel, Payload, Placement, Seq, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

/// Rows of each table: a record of 270 KiB, within the tests' 1 MiB segments.
const ROWS: usize = 8192;

/// A table of four `int64` columns, the last with nulls: five buffers, each the arena's own.
fn wide_batch(alloc: &FakeAllocator, seq: Seq) -> RecordBatch {
    let tier = alloc.host_tier();
    let mut fields = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();
    for c in 0..4i64 {
        let values: Vec<u8> = (0..ROWS as i64)
            .flat_map(|r| (r * 4 + c + seq as i64).to_le_bytes())
            .collect();
        let mut builder = ArrayData::builder(DataType::Int64)
            .len(ROWS)
            .add_buffer(alloc.arrow_buffer(&values, tier));
        if c == 3 {
            // Every eighth row null.
            let valid = vec![0b1111_1110u8; ROWS / 8];
            builder = builder.null_bit_buffer(Some(alloc.arrow_buffer(&valid, tier)));
        }
        let data = match builder.build() {
            Ok(data) => data,
            Err(e) => panic!("wide_batch: {e}"),
        };
        fields.push(Field::new(format!("c{c}"), DataType::Int64, c == 3));
        columns.push(make_array(data));
    }
    match RecordBatch::try_new(Arc::new(Schema::new(fields)), columns) {
        Ok(batch) => batch,
        Err(e) => panic!("wide_batch: {e}"),
    }
}

fn wide_morsel(alloc: &FakeAllocator, seq: Seq) -> Morsel {
    let payload = match Payload::table(wide_batch(alloc, seq)) {
        Ok(payload) => payload,
        Err(e) => panic!("wide_morsel: {e}"),
    };
    Morsel::new(seq, 0, payload, common::origin(seq))
}

#[test]
fn pl_a_table_read_back_from_staging_counts_its_bytes_once() {
    let scratch = common::Scratch::new("read_back_bytes");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new();
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 1 << 30, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    engine.set_promotion_window(0, 1);
    let pushed = wide_morsel(&alloc, 0).bytes;
    // Room for one table in memory: the others go to disk.
    engine.set_water(0, TierKind::PinnedHost, pushed, pushed);
    engine.set_water(0, TierKind::Host, pushed, pushed);
    for seq in 0..3u64 {
        let morsel = wide_morsel(&alloc, seq);
        assert_eq!(
            morsel.bytes, pushed,
            "each table is pushed with the same bytes"
        );
        engine.push(0, morsel).expect("push");
    }
    common::settle(&reactor);
    engine.close(0);

    let mut popped = Vec::new();
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        let Payload::Table(batch, _) = &morsel.payload else {
            panic!("morsel {} is not a table", morsel.seq);
        };
        assert_eq!(
            batch,
            &wide_batch(&alloc, morsel.seq),
            "morsel {} is the table that was pushed",
            morsel.seq
        );
        popped.push((morsel.seq, morsel.bytes, morsel.payload.bytes()));
    }
    let read_back = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::ReadFile)
        .count();
    assert!(read_back > 0, "a table went to disk and was read back");
    assert_eq!(popped.len(), 3, "every table is popped: {popped:?}");

    for (seq, bytes, payload_bytes) in &popped {
        assert_eq!(
            bytes, payload_bytes,
            "morsel {seq}'s bytes are its payload's"
        );
        // A table read back lies in one read, its buffers at page boundaries: the same bytes,
        // less nothing and more by no more than Arrow's own bookkeeping.
        let ratio = *bytes as f64 / pushed as f64;
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "morsel {seq} was pushed with {pushed} bytes and popped with {bytes}, {ratio:.2} \
             times: {read_back} of the tables were read back from disk, and all three as \
             (seq, bytes) are {:?}",
            popped.iter().map(|(s, b, _)| (*s, *b)).collect::<Vec<_>>()
        );
    }
}
