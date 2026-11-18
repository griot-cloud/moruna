//! SC-T50 (item 6.3s, 2026-11-18): a read that met a full arena is never dropped. The plan's last
//! read meets the wall (RB5) once, after the cursor has already passed it: the source's queues stay
//! open until it is read again, and the run completes with every row. Before the fix, the drive
//! closed the source's queues as soon as no read was in flight, so the sink finished without the
//! waiting read and the run reported `Completed` one split short (a 768-column copy, 6.3d's wide
//! tests: 35,000 of 40,000 rows), or, when the retry landed after the close, ended "push: stage 0
//! is closed".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, MorunaError, Payload, RowRange, Source, SourceSchema, Split,
    Tier,
};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

/// Fails the first read of the plan's last split with an arena wall, then reads it.
struct LastReadMeetsTheWall {
    inner: FakeSource,
    last: u32,
    failed: AtomicBool,
}

impl Source for LastReadMeetsTheWall {
    fn schema(&self) -> SourceSchema {
        Source::schema(&self.inner)
    }

    fn plan(&self) -> moruna_kernel::Result<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, moruna_kernel::Result<Payload>> {
        if split.id == self.last && !self.failed.swap(true, Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(MorunaError::Alloc {
                bytes: split.uncompressed_bytes,
                tier,
                budget: 0,
                in_use: 0,
            })));
        }
        self.inner.read(split, rows, alloc, tier)
    }
}

#[test]
fn sc_t50_a_waiting_read_is_never_dropped() {
    const SPLITS: u32 = 4;
    const ROWS: u64 = 8;
    for read_ahead in [1u16, 2, 4] {
        let source = Arc::new(LastReadMeetsTheWall {
            inner: FakeSource::new()
                .splits(SPLITS, ROWS, 64)
                .sub_splittable(false),
            last: SPLITS - 1,
            failed: AtomicBool::new(false),
        });
        let rig = RigBuilder::new()
            .cfg(|cfg| {
                cfg.workers_max = 1;
                cfg.workers_active = 1;
                cfg.read_ahead = read_ahead;
                cfg.initial_morsel_target = 64;
            })
            .source_over(Arc::clone(&source) as Arc<dyn Source>)
            .kernel(Arc::new(FakeKernel::new()) as Arc<dyn moruna_kernel::Kernel>)
            .go();
        let outcome = rig.scheduler.run(CancelToken::new()).expect("run");
        assert!(
            matches!(outcome, crate::RunOutcome::Completed { .. }),
            "read-ahead {read_ahead}: the run completes: {outcome:?}"
        );
        assert!(
            source.failed.load(Ordering::SeqCst),
            "read-ahead {read_ahead}: the last read met the wall"
        );
        let written = rig.sink.written();
        assert_eq!(
            written.len(),
            SPLITS as usize,
            "read-ahead {read_ahead}: every split reached the sink, the one that waited too: {written:?}"
        );
    }
}
