//! CT-T15 resume_defaults: a kernel with the default `restore` returns `Resume`; a
//! `KernelState` with the default `checkpoint` returns `Ok(None)`; a sink with the default
//! `resume` returns `Resume` and `committed_seq() == None`; `ResumePolicy::default() == Reinit`.
//! Proves the resume defaults are refusals, not silent successes (l, anti-patterns).

mod common;

use std::sync::Arc;

use amoru_kernel::{
    AmoruError, BoxFuture, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState,
    NoState, Payload, PayloadKind, PayloadSpec, ResumePolicy, Seq, Sink, SinkSummary, Source,
    SourceSchema, TierPref,
};
use common::FakeAllocator;

/// A kernel that takes every default: it declares nothing about resume, so `restore` refuses.
struct DefaultKernel;

impl Kernel for DefaultKernel {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("ct-t15::DefaultKernel", b"")
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> amoru_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, _ctx: &InitCtx) -> amoru_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> amoru_kernel::Result<Payload> {
        Ok(input)
    }
}

/// A sink that takes every default: not resumable, and it says so.
struct DefaultSink;

impl Sink for DefaultSink {
    fn open(&mut self, _schema: &SourceSchema) -> amoru_kernel::Result<()> {
        Ok(())
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }
    fn write(&self, _seq: Seq, _payload: Payload) -> BoxFuture<'_, amoru_kernel::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn finish(&mut self) -> amoru_kernel::Result<SinkSummary> {
        Ok(SinkSummary::default())
    }
}

#[test]
fn ct_t15_resume_defaults() {
    let alloc = FakeAllocator::new();
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(alloc.clone()),
    };

    // A kernel that declares `Checkpoint` but does not implement `restore` is refused, by
    // name, rather than silently re-inited.
    let kernel = DefaultKernel;
    let err = match kernel.restore(&ctx, b"state") {
        Ok(_) => panic!("the default restore must refuse"),
        Err(e) => e,
    };
    assert!(
        matches!(err, AmoruError::Resume(_)),
        "expected Resume, got {err}"
    );
    assert!(err.to_string().contains("restore"));

    // The default hints declare `Reinit`, the default policy.
    assert_eq!(ResumePolicy::default(), ResumePolicy::Reinit);
    assert_eq!(KernelHints::default().resume, ResumePolicy::Reinit);
    assert_eq!(kernel.hints().resume, ResumePolicy::Reinit);
    assert_eq!(KernelHints::default().state_bytes, None);
    assert!(!KernelHints::default().uses_device_memory);

    // The default state has nothing to save and no known footprint.
    let mut state = NoState;
    assert!(
        state
            .checkpoint()
            .expect("the default checkpoint succeeds")
            .is_none()
    );
    assert_eq!(state.footprint(), None);
    assert!(state.as_any_mut().downcast_mut::<NoState>().is_some());

    // The default sink is not resumable and says so through `resume`, and reports no commit
    // watermark and no checkpoint (SC f.11 detects a non-resumable sink by exactly this).
    let mut sink = DefaultSink;
    let schema = SourceSchema::Table(common::mixed_batch(1).schema());
    let err = sink
        .resume(&schema, b"", None)
        .expect_err("the default resume must refuse");
    assert!(
        matches!(err, AmoruError::Resume(_)),
        "expected Resume, got {err}"
    );
    assert!(err.to_string().contains("sink"));
    assert_eq!(sink.committed_seq(), None);
    assert!(
        sink.checkpoint()
            .expect("the default checkpoint succeeds")
            .is_none()
    );
    assert!(!sink.requires_order());
    sink.skip(7);
    assert_eq!(
        sink.committed_seq(),
        None,
        "skip on a sink that tracks nothing changes nothing"
    );

    // A source that takes the default is repeatable (d.6), which is what CT-I12 assumes.
    struct DefaultSource;
    impl Source for DefaultSource {
        fn schema(&self) -> SourceSchema {
            SourceSchema::Tensor {
                dtype: amoru_kernel::DType::F32,
                shape: vec![-1],
            }
        }
        fn plan(&self) -> amoru_kernel::Result<Vec<amoru_kernel::Split>> {
            Ok(Vec::new())
        }
        fn read(
            &self,
            _split: &amoru_kernel::Split,
            _rows: Option<amoru_kernel::RowRange>,
            _alloc: &dyn amoru_kernel::Allocator,
            _tier: amoru_kernel::Tier,
        ) -> BoxFuture<'_, amoru_kernel::Result<Payload>> {
            Box::pin(async {
                Err(AmoruError::Source {
                    split: 0,
                    msg: "no splits".into(),
                })
            })
        }
    }
    assert!(DefaultSource.repeatable());
}
