//! SC-T48 (item 6.3b, system design 3A): no morsel is larger than the smallest budget of the
//! tiers it must be placed in, and a morsel that would be is cut by rows, not stalled.
//!
//! Found by 3.8's proof: Enrich on a 4 GiB node ended after 30 s with "staging: morsel 0
//! (235894392 bytes) waited 30 s for room in [1] and none came: 235894392 bytes more than a
//! budget of 223379456 holds ... the run has no disk tier". A morsel larger than its queue's
//! whole host budget can never be placed. Today the push loop waits for room anyway
//! (`decide::admit_push_as`: `SPILL_AFTER`, the refused spill, then `PUSH_STALL`), and nothing
//! upstream keeps a morsel inside the budget it is headed for:
//!
//! - the drive sizes a read by its stage's morsel target and admits it against the arena's free
//!   room at twice its estimate, never against the destination queue's budget, which is half
//!   the arena less the sinks' file buffers (`source_drive::issue_reads`, `range_end`);
//! - a function that grows its morsel (Enrich adds four columns) pushes its output whole into
//!   the next queue, whatever that queue's budget (`decide::admit_push_on_worker`).
//!
//! Both cases below fail today, ending at `PUSH_STALL` on a clock the test steps. Both split
//! sizes are sub-splittable, so the rows could have been cut.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moruna_kernel::arrow::array::{Array, ArrayRef, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    NoState, Payload, PayloadKind, PayloadSpec, Result, SourceSchema, TierPref,
};
use moruna_testkit::{FakeKernel, FakePlacement, FakeSource};

use super::common::RigBuilder;
use crate::decide::Clock;

/// Rows of a split, eight bytes each.
const ROWS: u64 = 16;
/// A split's bytes.
const SPLIT: u64 = ROWS * 8;
/// The host tier's budget: less than a split, more than half of one.
const BUDGET: u64 = SPLIT * 3 / 4;

/// A clock that moves only when it is waited on, so `PUSH_STALL` passes at once.
#[derive(Debug, Default)]
struct Stepped(AtomicU64);

impl Clock for Stepped {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }

    fn pause(&self, pause: Duration) {
        self.0.fetch_add(pause.as_nanos() as u64, Ordering::SeqCst);
        std::thread::yield_now();
    }
}

/// A test-local stateless `Kernel` (k) that grows each morsel as Enrich does: it adds a column,
/// here a copy of the first, so its output holds twice its input's bytes.
struct Widening;

impl Kernel for Widening {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-scheduler::tests::sc_t48::Widening", &[])
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints::default()
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Any,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        let SourceSchema::Table(schema) = input else {
            return Err(MorunaError::Plan("a table".into()));
        };
        let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
        fields.push(Field::new("copy", DataType::Int64, false));
        Ok(SourceSchema::Table(Arc::new(Schema::new(fields))))
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = input else {
            return Err(MorunaError::Plan("a table".into()));
        };
        let first = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| MorunaError::Plan("an int64 column".into()))?;
        let copy: ArrayRef = Arc::new(Int64Array::from(first.values().to_vec()));
        let mut columns = batch.columns().to_vec();
        columns.push(copy);
        let SourceSchema::Table(schema) =
            self.output_schema(&SourceSchema::Table(batch.schema()))?
        else {
            return Err(MorunaError::Plan("a table".into()));
        };
        let widened = RecordBatch::try_new(schema, columns)
            .map_err(|e| MorunaError::Plan(format!("widening: {e}")))?;
        Payload::table(widened)
    }
}

/// Runs the rig to its end and says how: the rows the sink was given, or the diagnostic.
fn ended(rig: super::common::Rig) -> core::result::Result<u64, String> {
    match rig.scheduler.run(CancelToken::new()) {
        Ok(crate::RunOutcome::Completed { sinks }) => Ok(sinks[0].rows),
        Ok(crate::RunOutcome::Terminated { diagnostic, .. }) => Err(diagnostic.to_string()),
        other => Err(format!("{other:?}")),
    }
}

#[test]
fn sc_t48_a_read_is_never_larger_than_the_queue_it_goes_to() {
    // The stage's target is larger than a split, so the drive reads each split whole, and a
    // whole split is larger than the host tier's whole budget. The rows could be read in parts
    // that fit; a read is never larger than the budget of the queue it is pushed to.
    let clock = Arc::new(Stepped::default());
    let rig = RigBuilder::new()
        .cfg({
            let clock = Arc::clone(&clock);
            move |cfg| {
                cfg.clock = clock;
                cfg.initial_morsel_target = 1 << 20;
                cfg.morsel_min = 8;
            }
        })
        .source(FakeSource::new().splits(3, ROWS, SPLIT))
        .placement(FakePlacement::new().with_budget(BUDGET).without_disk())
        .kernel(Arc::new(FakeKernel::new()))
        .go();
    match ended(rig) {
        Ok(rows) => assert_eq!(rows, 3 * ROWS, "every row, once"),
        Err(diagnostic) => panic!(
            "a split of {SPLIT} bytes was read whole into a queue whose tier holds {BUDGET}, \
             and the run ended waiting for room that could not come (6.3b): {diagnostic}"
        ),
    }
}

#[test]
fn sc_t48_a_function_that_grows_a_morsel_cuts_it_by_rows() {
    // Each read is half a split, inside the budget; the function doubles it, past the budget of
    // the sink's queue. Its output is cut by rows into parts that fit, not stalled.
    let clock = Arc::new(Stepped::default());
    let rig = RigBuilder::new()
        .cfg({
            let clock = Arc::clone(&clock);
            move |cfg| {
                cfg.clock = clock;
                cfg.initial_morsel_target = SPLIT / 2;
                cfg.morsel_min = 8;
            }
        })
        .source(FakeSource::new().splits(3, ROWS, SPLIT))
        .placement(FakePlacement::new().with_budget(BUDGET).without_disk())
        .kernel(Arc::new(Widening))
        .go();
    match ended(rig) {
        Ok(rows) => assert_eq!(rows, 3 * ROWS, "every row, once"),
        Err(diagnostic) => panic!(
            "a function grew a morsel of {} bytes to {SPLIT}, past the {BUDGET} its next queue's \
             tier holds, and the run ended waiting for room that could not come (6.3b): \
             {diagnostic}",
            SPLIT / 2
        ),
    }
}
