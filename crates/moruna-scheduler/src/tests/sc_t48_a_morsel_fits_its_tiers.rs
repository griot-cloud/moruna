//! SC-T48 (item 6.3b, system design 3A): no morsel is larger than the smallest budget of the
//! tiers it must be placed in, and a morsel that would be is cut by rows, not stalled.
//!
//! Found by 3.8's proof: Enrich on a 4 GiB node ended after 30 s with "staging: morsel 0
//! (235894392 bytes) waited 30 s for room in [1] and none came: 235894392 bytes more than a
//! budget of 223379456 holds ... the run has no disk tier". A morsel larger than its queue's
//! whole host budget can never be placed, and the push loop waited for room anyway, while
//! nothing upstream kept a morsel inside the budget it was headed for:
//!
//! - the drive sized a read by its stage's morsel target and admitted it against the arena's
//!   free room, never against the destination tier's budget; it now cuts a read by rows so its
//!   charge, grown by what the stages it feeds grow a morsel to, fits that budget
//!   (`source_drive::tier_cap`, `grown`);
//! - a function that grows its morsel pushed its output whole into the next queue; each stage
//!   now learns its growth, which sizes the reads after its first morsel, and a morsel that is
//!   still larger than its tier's whole budget is admitted as the one morsel over it, counted
//!   as an oversize admission, rather than waited on (`decide::admit_push_as`).
//!
//! The cases below ended at `PUSH_STALL`, 30 s after the morsel's first refusal, before the
//! fix. They run on the run's own clock: a stepped one would also pass the 30 s while the sink
//! drains the room the reads after an oversize morsel wait for.

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, Int64Array, RecordBatch};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError,
    NoState, Payload, PayloadKind, PayloadSpec, Placement, Result, SourceSchema, TierKind,
    TierPref,
};
use moruna_testkit::{FakeKernel, FakePlacement, FakeSource};

use super::common::RigBuilder;

/// Rows of a split, eight bytes each: enough that a morsel's own bytes, not Arrow's few hundred
/// bytes of array headers, are what is counted.
const ROWS: u64 = 4096;
/// A split's bytes.
const SPLIT: u64 = ROWS * 8;
/// The host tier's budget: less than a split, more than half of one.
const BUDGET: u64 = SPLIT * 3 / 4;

/// A test-local stateless `Kernel` (k) that grows each morsel as Enrich does: it adds columns,
/// here `.0` copies of the first, so its output holds `.0 + 1` times its input's bytes.
struct Widening(usize);

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
        for copy in 0..self.0 {
            fields.push(Field::new(format!("copy_{copy}"), DataType::Int64, false));
        }
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
        let mut columns = batch.columns().to_vec();
        for _ in 0..self.0 {
            columns.push(Arc::new(Int64Array::from(first.values().to_vec())) as ArrayRef);
        }
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

/// Runs the rig to its end and says how: the rows the sink was given and the morsels admitted
/// over their tier's budget, or the diagnostic.
fn ended(rig: super::common::Rig) -> core::result::Result<(u64, u64), String> {
    let outcome = rig.scheduler.run(CancelToken::new());
    let oversize = rig
        .scheduler
        .shared()
        .decider
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .stats()
        .oversize_admissions;
    match outcome {
        Ok(crate::RunOutcome::Completed { sinks }) => Ok((sinks[0].rows, oversize)),
        Ok(crate::RunOutcome::Terminated { diagnostic, .. }) => Err(diagnostic.to_string()),
        other => Err(format!("{other:?}")),
    }
}

/// A rig of `splits` splits through `kernel`, reading `target` bytes at a time, into a host tier
/// of `budget` bytes with no disk. The reads' queue stops taking more at a quarter of the tier,
/// as the controller's high waters keep a queue from holding the room its consumer's output
/// needs; with none, small reads after a growing stage's first morsel filled the tier and no
/// unit's output could be claimed.
fn rig(splits: u32, target: u64, budget: u64, kernel: Arc<dyn Kernel>) -> super::common::Rig {
    let placement = FakePlacement::new().with_budget(budget).without_disk();
    Placement::set_water(&placement, 0, TierKind::Host, 0, budget / 4);
    RigBuilder::new()
        .cfg(move |cfg| {
            cfg.initial_morsel_target = target;
            cfg.morsel_min = 8;
        })
        .source(FakeSource::new().splits(splits, ROWS, SPLIT))
        .placement(placement)
        .kernel(kernel)
        .go()
}

#[test]
fn sc_t48_a_read_is_never_larger_than_the_queue_it_goes_to() {
    // The stage's target is larger than a split, so the drive would read each split whole, and
    // a whole split is larger than the host tier's whole budget. The rows are read in parts that
    // fit: a read is never larger than the budget of the queue it is pushed to.
    match ended(rig(3, 1 << 20, BUDGET, Arc::new(FakeKernel::new()))) {
        Ok((rows, oversize)) => {
            assert_eq!(rows, 3 * ROWS, "every row, once");
            assert_eq!(oversize, 0, "no read was larger than its tier");
        }
        Err(diagnostic) => panic!(
            "a split of {SPLIT} bytes was read whole into a queue whose tier holds {BUDGET}, \
             and the run ended waiting for room that could not come (6.3b): {diagnostic}"
        ),
    }
}

#[test]
fn sc_t48_a_function_that_grows_a_morsel_past_its_tier_is_admitted_alone() {
    // The first read fits the tier (cut to half its budget, at its charge); the function makes
    // four times what it is given, past the budget of the tier. That morsel is admitted alone,
    // not stalled; the reads after it are sized by the growth the stage has shown.
    match ended(rig(3, SPLIT / 2, BUDGET, Arc::new(Widening(3)))) {
        Ok((rows, oversize)) => {
            assert_eq!(rows, 3 * ROWS, "every row, once");
            assert!(oversize >= 1, "the first output was admitted alone");
        }
        Err(diagnostic) => panic!(
            "a function grew its morsels fourfold, past the {BUDGET} bytes their tier holds, and \
             the run ended waiting for room that could not come (6.3b): {diagnostic}"
        ),
    }
}

#[test]
fn sc_t48_after_its_first_morsel_a_growing_function_places_none_over_its_tier() {
    // The stage learns from its first morsel that it makes eight times what it is given, and
    // every read after it is cut so its output fits the tier, beside the reads behind it: of
    // twelve whole splits, only the morsels the drive read before it had learned are admitted
    // over the budget of four splits.
    match ended(rig(12, 2 * SPLIT, 4 * SPLIT, Arc::new(Widening(7)))) {
        Ok((rows, oversize)) => {
            assert_eq!(rows, 12 * ROWS, "every row, once");
            assert!(
                oversize <= 2,
                "{oversize} morsels went over the tier: more than the reads in flight when the \
                 stage first grew one"
            );
        }
        Err(diagnostic) => panic!("{diagnostic}"),
    }
}
