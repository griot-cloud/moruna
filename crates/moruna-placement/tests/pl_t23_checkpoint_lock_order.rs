//! PL-T23 checkpoint_lock_order (f.11, f.12, PL-I12): `checkpoint` and `set_committed` run
//! beside producers and consumers under a lock-order detector; the lineage lock is never held
//! while a queue lock or the segments lock is requested, the manifest write happens with no
//! engine lock held, and a reader never sees a partial manifest.

mod common;

use moruna_kernel::{CheckpointExtras, Locality, Placement, SourceCursor, TierKind};
use moruna_testkit::{FakeAllocator, FakeReactor};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[test]
fn pl_t23_checkpoint_lock_order() {
    let seconds: u64 = match std::env::var("MORUNA_PL_T23_SECONDS") {
        Ok(value) => value.parse().unwrap_or(3),
        Err(_) => 3,
    };
    let scratch = common::Scratch::new("t23");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_micros(200));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    let sample = common::table_morsel(&alloc, 0, 0, 64);
    let bytes = sample.bytes;
    drop(sample);
    engine.set_water(0, TierKind::Host, bytes * 16, bytes * 32);
    let manifest = engine.manifest_path().expect("a manifest path");

    let stop = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicU64::new(0));
    let committed = Arc::new(AtomicU64::new(0));
    let manifests = Arc::new(AtomicU64::new(0));
    let commits = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(seconds);

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let engine = Arc::clone(&engine);
            let alloc = alloc.clone();
            let next = Arc::clone(&next);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let seq = next.fetch_add(1, Ordering::Relaxed);
                    let morsel = common::table_morsel(&alloc, seq, 0, 64);
                    if engine.push(0, morsel).is_err() {
                        break;
                    }
                }
            });
        }
        for _ in 0..8 {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let committed = Arc::clone(&committed);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match engine.pop(0, common::want_host(), Locality::Any) {
                        // A consumer records what it consumed; it does not commit. f.11 gives
                        // the watermark one owner (10 f.11: the sink drive owns it) and ignores
                        // a value below it with a `debug_assert`, so eight threads reading the
                        // maximum and then calling `set_committed` would be a caller bug: the
                        // thread holding the lower value can make the later call.
                        Ok(Some(morsel)) => {
                            committed.fetch_max(morsel.seq, Ordering::AcqRel);
                        }
                        Ok(None) => std::thread::yield_now(),
                        Err(_) => break,
                    }
                }
            });
        }
        // The watermark's one owner: `set_committed` runs beside `checkpoint`, the producers and
        // the consumers (f.11, f.12), in the increasing order the engine is promised.
        {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let committed = Arc::clone(&committed);
            let commits = Arc::clone(&commits);
            scope.spawn(move || {
                let mut published: Option<u64> = None;
                while !stop.load(Ordering::Relaxed) {
                    let reached = committed.load(Ordering::Acquire);
                    if published.is_none_or(|last| reached > last) {
                        engine.set_committed(reached);
                        published = Some(reached);
                        commits.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::yield_now();
                }
            });
        }
        // The checkpoint thread of preamble 4.1: it writes the manifest on its own thread,
        // never on a worker and never under a queue lock.
        {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            let manifests = Arc::clone(&manifests);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let extras = CheckpointExtras {
                        kernel_states: Vec::new(),
                        sink_state: None,
                        committed_seq: None,
                        source_cursor: SourceCursor::default(),
                    };
                    if engine.checkpoint(&extras).is_ok() {
                        manifests.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        }
        // A reader parses the manifest in a loop and must never see a partial file (PL-I12).
        {
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            let manifest = manifest.clone();
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(text) = std::fs::read_to_string(&manifest) {
                        let parsed: Result<moruna_placement::manifest::Manifest, _> =
                            serde_json::from_str(&text);
                        assert!(
                            parsed.is_ok(),
                            "a reader saw a partial manifest of {} bytes (PL-I12)",
                            text.len()
                        );
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::yield_now();
                }
            });
        }
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
    });

    assert!(
        manifests.load(Ordering::Relaxed) > 1,
        "manifests were written"
    );
    assert!(
        reads.load(Ordering::Relaxed) > 1,
        "and read while being written"
    );
    assert!(
        commits.load(Ordering::Relaxed) > 1,
        "and the watermark moved beside them, so `set_committed` was under the detector too"
    );
    assert_eq!(
        moruna_placement::locks::violations(),
        0,
        "a lock was taken out of order (preamble 4.2, f.11, f.12)"
    );
    engine.shutdown();
}

#[test]
fn pl_t23_the_detector_itself_reports_the_order() {
    use moruna_placement::locks::{self, Held, LINEAGE, QUEUE, SEGMENTS};
    assert!(locks::none_held(), "a thread that holds nothing says so");
    let queue = Held::enter(QUEUE);
    assert_eq!(queue.position(), QUEUE);
    assert!(!locks::none_held());
    let lineage = Held::enter(LINEAGE);
    assert_eq!(lineage.position(), LINEAGE);
    let segments = Held::enter(SEGMENTS);
    assert_eq!(segments.position(), SEGMENTS);
    drop(segments);
    drop(lineage);
    drop(queue);
    assert!(locks::none_held(), "the guards released in reverse");
    assert_eq!(
        locks::violations(),
        0,
        "the engine took no lock out of order in this process"
    );
}
