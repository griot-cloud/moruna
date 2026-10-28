//! SC-T27: `Kernel::finish` (contracts d.7, 10 f.6) is called once per live instance after the
//! last apply and before the sink finishes; a kernel whose `finish` fails ends the run as
//! `Terminated`, naming the stage, and the sink is not finished.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorunaError, Payload, PayloadSpec,
    Result, SourceSchema,
};

use super::common::{RigBuilder, StatefulKernel, run_within};
use crate::RunOutcome;

/// A stateful kernel that counts its finishes and fails them when asked to.
struct Finishing {
    inner: StatefulKernel,
    finishes: AtomicUsize,
    fail_with: Option<&'static str>,
    panic: bool,
}

impl Kernel for Finishing {
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
        self.inner.apply(state, input)
    }

    fn finish(&self, _state: &mut dyn KernelState) -> Result<()> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("the end of the input broke it");
        }
        match self.fail_with {
            Some(msg) => Err(MorunaError::Kernel {
                stage: moruna_kernel::StageId::MAX,
                seq: moruna_kernel::Seq::MAX,
                msg: msg.into(),
                features: None,
            }),
            None => Ok(()),
        }
    }
}

fn finishing(fail_with: Option<&'static str>, panic: bool) -> Arc<Finishing> {
    Arc::new(Finishing {
        inner: StatefulKernel::new(1),
        finishes: AtomicUsize::new(0),
        fail_with,
        panic,
    })
}

#[test]
fn sc_t27_a_finish_that_succeeds_is_called_once_per_instance() {
    let kernel = finishing(None, false);
    let rig = RigBuilder::new().kernel(kernel.clone()).go();
    rig.scheduler
        .init_instances()
        .expect("the instance is made");
    let outcome = run_within(&rig, Duration::from_secs(30), "SC-T27 success");
    assert!(
        matches!(outcome, RunOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(kernel.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.sink.finish_calls(),
        1,
        "the sink finished after the kernel"
    );
}

#[test]
fn sc_t27_a_finish_that_fails_ends_the_run_before_the_sink_finishes() {
    for (fail_with, panic, expect) in [
        (
            Some("the image exited with status 3"),
            false,
            "the image exited with status 3",
        ),
        (None, true, "panic in finish: the end of the input broke it"),
    ] {
        crate::tests::common::quiet_kernel_panics();
        let kernel = finishing(fail_with, panic);
        let rig = RigBuilder::new().kernel(kernel.clone()).go();
        rig.scheduler
            .init_instances()
            .expect("the instance is made");
        let outcome = run_within(&rig, Duration::from_secs(30), "SC-T27 failure");
        match outcome {
            RunOutcome::Terminated { diagnostic, .. } => {
                let text = diagnostic.to_string();
                assert!(text.contains(expect), "{text}");
                assert!(text.contains("stage 1"), "the stage is named: {text}");
            }
            other => panic!("expected the run to end Terminated, got {other:?}"),
        }
        assert_eq!(kernel.finishes.load(Ordering::SeqCst), 1);
        assert_eq!(rig.sink.finish_calls(), 0, "the sink was not finished");
    }
}
