//! TR-T2 order. Records per stage with random interleaving; read back sorted by
//! `(stage, seq)`: no gaps within a stage, no duplicates. Proves TR-I2.

mod common;

use std::sync::Arc;

use common::{TempDir, config, record};
use moruna_kernel::TraceSink;
use moruna_trace::TraceWriter;

const STAGES: u16 = 4;
const PER_STAGE: u64 = 5_000;

/// A deterministic interleaving: a linear congruential generator, so the test needs no
/// dependency and the same interleaving is replayed on every host.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0 >> 33
    }
}

#[test]
fn tr_t2_order() {
    let dir = TempDir::new("t2");
    let mut cfg = config(dir.path());
    cfg.channel_capacity = 512;
    // Small enough that most of the trace goes through the overflow file, so the read back
    // covers both halves of the view.
    cfg.memory_limit = 512 * 1024;
    let writer = TraceWriter::start(cfg).expect("start");

    std::thread::scope(|scope| {
        for stage in 0..STAGES {
            let sink: Arc<dyn TraceSink> = writer.clone();
            scope.spawn(move || {
                let mut rng = Lcg(0x5eed + stage as u64);
                for seq in 0..PER_STAGE {
                    // A pseudo-random pause scatters the threads against each other.
                    if rng.next().is_multiple_of(32) {
                        std::thread::yield_now();
                    }
                    sink.record(record(seq, stage));
                }
            });
        }
    });

    let view = writer.finish().expect("finish");
    let mut pairs: Vec<(u16, u64)> = view.records().iter().map(|r| (r.stage, r.seq)).collect();
    assert_eq!(pairs.len() as u64, STAGES as u64 * PER_STAGE);
    pairs.sort_unstable();
    pairs.dedup();
    assert_eq!(
        pairs.len() as u64,
        STAGES as u64 * PER_STAGE,
        "a (stage, seq) pair appears more than once"
    );
    for stage in 0..STAGES {
        for seq in 0..PER_STAGE {
            assert!(
                pairs.binary_search(&(stage, seq)).is_ok(),
                "stage {stage} has a gap at sequence {seq}"
            );
        }
    }

    // Every field survives the round trip, so "sorted by (stage, seq)" is over real records.
    let one = view
        .records()
        .into_iter()
        .find(|r| r.stage == 2 && r.seq == 17)
        .expect("a known record");
    let expected = record(17, 2);
    assert_eq!(one.worker, expected.worker);
    assert_eq!(one.feat_column_bytes, expected.feat_column_bytes);
    assert_eq!(one.q_bytes_after, expected.q_bytes_after);
    assert_eq!(one.feat_mean_string_len, expected.feat_mean_string_len);
    assert_eq!(one.error, expected.error);
}
