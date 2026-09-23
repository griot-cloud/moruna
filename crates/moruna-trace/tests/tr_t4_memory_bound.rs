//! TR-T4 memory_bound. 10 M records with `memory_limit` 8 MiB: process resident set growth
//! under 16 MiB above baseline, overflow file present. Proves TR-I4.

mod common;

/// The bound TR-I4 claims holds in the runtime's process, which sets `mimalloc` as its
/// global allocator (12 l); this test binary sets the same one so the resident set it reads
/// is the resident set the runtime would have.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use common::{TempDir, config, lean_record, resident_bytes};
use moruna_kernel::TraceSink;
use moruna_trace::TraceWriter;

/// The record count the SDD names. A long run must not grow the process without limit, so
/// this test is the one that proves the bound at the scale the design claims it holds at.
const RECORDS: u64 = 10_000_000;
/// `trace.memory_limit` for this run.
const LIMIT: u64 = 8 * 1024 * 1024;
/// The growth the test allows above the baseline resident set.
const ALLOWED_GROWTH: u64 = 16 * 1024 * 1024;

#[test]
fn tr_t4_memory_bound() {
    let dir = TempDir::new("t4");
    let mut cfg = config(dir.path());
    cfg.memory_limit = LIMIT;
    cfg.channel_capacity = 4096;
    let writer = TraceWriter::start(cfg).expect("start");

    // The baseline is taken after the writer exists, so the thread, the channel and the
    // first builders are already counted and only the trace's own growth is measured.
    for seq in 0..4096 {
        writer.record(lean_record(seq, 1));
    }
    writer.flush().expect("flush");
    let baseline = resident_bytes().expect("a resident set reading on this platform");

    let sink: Arc<dyn TraceSink> = writer.clone();
    let mut worst_growth = 0i64;
    let mut worst_chunk_bytes = 0u64;
    for seq in 4096..RECORDS {
        sink.record(lean_record(seq, (seq % 3) as u16));
        if seq.is_multiple_of(10_000) {
            worst_chunk_bytes = worst_chunk_bytes.max(writer.memory_bytes());
        }
        if seq.is_multiple_of(500_000) {
            let now = resident_bytes().expect("resident set");
            if std::env::var_os("MORUNA_T4_TRACE").is_some() {
                println!(
                    "seq {seq} rss {now} growth {}",
                    now as i64 - baseline as i64
                );
            }
            worst_growth = worst_growth.max(now as i64 - baseline as i64);
        }
    }
    writer.flush().expect("flush");
    let after = resident_bytes().expect("resident set");
    worst_growth = worst_growth.max(after as i64 - baseline as i64);

    worst_chunk_bytes = worst_chunk_bytes.max(writer.memory_bytes());
    // The writer says what it holds, so a run that is losing the bound is diagnosable.
    let described = format!("{writer:?}");
    assert!(described.contains("memory_bytes"), "{described}");
    assert!(described.contains("overflow_failed: false"), "{described}");

    let view = writer.finish().expect("finish");
    assert_eq!(view.len(), RECORDS, "no record may be dropped");

    // TR-I4 itself: the bytes the writer holds in memory, which is the quantity it
    // controls, never passed the limit at any sample over the ten million records.
    assert!(
        worst_chunk_bytes <= LIMIT,
        "in-memory chunks reached {worst_chunk_bytes} bytes, above the {LIMIT} byte limit"
    );

    let overflow = dir
        .path()
        .join(format!("trace-overflow-{}.arrow", "ab".repeat(16)));
    assert!(
        overflow.exists(),
        "the overflow file must exist at {}: the in-memory chunks were bounded by {LIMIT} bytes",
        overflow.display()
    );
    let spilled = std::fs::metadata(&overflow)
        .expect("overflow metadata")
        .len();
    assert!(
        spilled > LIMIT,
        "the overflow file holds {spilled} bytes, which is not more than the memory limit"
    );

    assert!(
        worst_growth <= ALLOWED_GROWTH as i64,
        "resident set grew by {worst_growth} bytes above the baseline of {baseline}, \
         which is more than the {ALLOWED_GROWTH} the memory bound allows"
    );
}
