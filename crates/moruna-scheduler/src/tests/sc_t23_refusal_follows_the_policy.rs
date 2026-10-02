//! SC-T23 (E13): a refused request (`MorunaError::Refused` from `apply`, 05 f.12) is enriched
//! with the morsel and follows the error policy like a kernel error (f.8, 05 f.13), and the
//! counts a kernel leaves on its thread reach that call's record (contracts d.7).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moruna_kernel::{
    AllocCounts, CancelToken, ErrorPolicy, Fingerprint, InitCtx, Kernel, KernelAlloc, KernelKind,
    KernelState, MorunaError, Outcome, Payload, PayloadSpec, Result, Seq, SourceSchema,
    set_kernel_alloc,
};
use moruna_testkit::{FakeKernel, FakeSource};

use super::common::RigBuilder;

/// Refuses on the given applies (1-based; one worker, so apply n is morsel n - 1), counts each call.
struct RefusingKernel {
    inner: FakeKernel,
    refuse_on: Vec<usize>,
    applies: AtomicUsize,
}

impl Kernel for RefusingKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint()
    }

    fn kind(&self) -> KernelKind {
        self.inner.kind()
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        self.inner.output_schema(input)
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.inner.init(ctx)
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let n = self.applies.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        let refused = self.refuse_on.contains(&(n as usize));
        set_kernel_alloc(KernelAlloc {
            measured: true,
            refusal_on: true,
            numpy: AllocCounts {
                bytes: n,
                requests: 1,
                largest: n,
                peak: n,
                refused: u64::from(refused),
            },
            ..KernelAlloc::default()
        });
        if refused {
            return Err(MorunaError::Refused {
                stage: u16::MAX,
                seq: u64::MAX,
                kernel: "jobs.greedy".into(),
                requested: 1 << 30,
                in_use: 100,
                ceiling: 200,
                features: None,
            });
        }
        self.inner.apply(state, input)
    }
}

fn run(policy: ErrorPolicy) -> (Vec<moruna_kernel::TraceRecord>, Vec<Seq>, crate::RunOutcome) {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.read_ahead = 1;
            cfg.initial_morsel_target = 8;
            cfg.error_policy = policy;
        })
        .source(FakeSource::new().splits(1, 30, 240))
        .kernel(Arc::new(RefusingKernel {
            inner: FakeKernel::new(),
            refuse_on: vec![6, 10],
            applies: AtomicUsize::new(0),
        }))
        .go();
    let outcome = match rig.scheduler.run(CancelToken::new()) {
        Ok(outcome) => outcome,
        Err(e) => panic!("run: {e}"),
    };
    (rig.trace.records(), rig.sink.skipped(), outcome)
}

#[test]
fn sc_t23_refusal_terminates_with_the_morsel() {
    let (records, _, outcome) = run(ErrorPolicy::Terminate);
    let crate::RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    match diagnostic {
        MorunaError::Refused {
            stage,
            seq,
            kernel,
            features,
            ..
        } => {
            assert_eq!((stage, seq), (1, 5), "the scheduler named the morsel");
            assert_eq!(kernel, "jobs.greedy");
            assert!(features.is_some(), "and its features");
        }
        other => panic!("expected a Refused diagnostic, got {other}"),
    }
    let refused = records
        .iter()
        .find(|r| r.seq == 5)
        .expect("the refused record");
    assert_eq!(refused.outcome, Outcome::Error);
    assert_eq!(
        refused.alloc.numpy.refused, 1,
        "its counts reached its record"
    );
    assert!(refused.alloc.measured);
}

#[test]
fn sc_t23_refusal_skipped() {
    let (records, skipped, outcome) = run(ErrorPolicy::Skip);
    assert!(
        matches!(outcome, crate::RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(skipped, vec![5, 9]);
    for r in &records {
        // One worker: apply n is morsel n - 1, and each record carries its own call's counts.
        assert_eq!(
            r.alloc.numpy.bytes,
            r.seq + 1,
            "record {} carries its call's counts",
            r.seq
        );
    }
    let error = records
        .iter()
        .find(|r| r.seq == 9)
        .and_then(|r| r.error.clone())
        .unwrap_or_default();
    assert!(
        error.starts_with("budget: kernel jobs.greedy stage 1 morsel 9"),
        "{error}"
    );
}

#[test]
fn sc_t23_refusal_counts_toward_the_budget() {
    let (records, skipped, outcome) = run(ErrorPolicy::Budget(2));
    assert_eq!(skipped, vec![5]);
    assert!(
        matches!(outcome, crate::RunOutcome::Terminated { .. }),
        "{outcome:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| r.seq == 9 && r.outcome == Outcome::Error)
    );
}
