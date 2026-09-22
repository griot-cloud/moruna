//! CT-T7 send_sync: static assertions that every trait object is `Send + Sync` and every value
//! type is `Send`. Proves CT-I6.

use amoru_kernel::*;

/// Fails to compile unless `T` is `Send`.
const fn assert_send<T: Send>() {}
/// Fails to compile unless `T` is `Send + Sync`.
const fn assert_send_sync<T: Send + Sync + ?Sized>() {}

#[test]
fn ct_t7_send_sync() {
    // Every trait in the crate is `Send + Sync` as a trait object (CT-I6).
    assert_send_sync::<dyn Allocator>();
    assert_send_sync::<dyn ArenaHandle>();
    assert_send_sync::<dyn Source>();
    assert_send_sync::<dyn Kernel>();
    assert_send_sync::<dyn Sink>();
    assert_send_sync::<dyn Reactor>();
    assert_send_sync::<dyn ObjectMetadata>();
    assert_send_sync::<dyn Placement>();
    assert_send_sync::<dyn Knobs>();
    assert_send_sync::<dyn StatsSource>();
    assert_send_sync::<dyn Prober>();
    assert_send_sync::<dyn Sampler>();
    assert_send_sync::<dyn TraceSink>();
    assert_send_sync::<dyn TraceTail>();
    // `KernelState` is per instance and moves between threads with its instance, so it is
    // `Send` and deliberately not `Sync` (d.7).
    assert_send::<Box<dyn KernelState>>();

    // Every value type a trait exchanges is `Send` (CT-I6).
    assert_send::<Buffer>();
    assert_send::<BufferView>();
    assert_send::<ManagedTensor>();
    assert_send::<Payload>();
    assert_send::<Morsel>();
    assert_send::<MorselFeatures>();
    assert_send::<Origin>();
    assert_send::<Split>();
    assert_send::<RowRange>();
    assert_send::<SourceSchema>();
    assert_send::<PayloadSpec>();
    assert_send::<Tier>();
    assert_send::<TierKind>();
    assert_send::<StagingCodec>();
    assert_send::<SegmentRef>();
    assert_send::<RemoteRef>();
    assert_send::<AllocStats>();
    assert_send::<AmoruError>();
    assert_send::<ConvertError>();
    assert_send::<Fingerprint>();
    assert_send::<KernelHints>();
    assert_send::<KernelKind>();
    assert_send::<ResumePolicy>();
    assert_send::<GilState>();
    assert_send::<InitCtx>();
    assert_send::<SinkSummary>();
    assert_send::<IoPaths>();
    assert_send::<ObjectMeta>();
    assert_send::<CopySrc>();
    assert_send::<CopyDst>();
    assert_send::<TierBudgets>();
    assert_send::<QueueStats>();
    assert_send::<PlacementStats>();
    assert_send::<Locality>();
    assert_send::<CheckpointExtras>();
    assert_send::<SourceCursor>();
    assert_send::<ResumePoint>();
    assert_send::<Knob>();
    assert_send::<KnobSnapshot>();
    assert_send::<StageStats>();
    assert_send::<SchedulerStats>();
    assert_send::<ProbeResult>();
    assert_send::<ErrorPolicy>();
    assert_send::<SizerKind>();
    assert_send::<CancelToken>();
    assert_send::<RecordHook>();
    assert_send::<Limits>();
    assert_send::<Device>();
    assert_send::<Guarantee>();
    assert_send::<HostProfile>();
    assert_send::<LimitSource>();
    assert_send::<Sample>();
    assert_send::<TraceRecord>();
    assert_send::<Outcome>();
    assert_send::<RunId>();
    assert_send::<NodeId>();
    assert_send::<DeviceId>();
    assert_send::<Completion<Buffer>>();
    assert_send::<CompletionSender<Buffer>>();

    // A `CancelToken` is shared between the drives and the workers, so it is `Sync` too, and
    // the sharing is what the type is for: a clone cancelled on another thread is seen here.
    assert_send_sync::<CancelToken>();
    assert_send_sync::<BufferView>();
    let token = CancelToken::new();
    assert!(!token.is_cancelled());
    let clone = token.clone();
    std::thread::spawn(move || clone.cancel())
        .join()
        .expect("the cancelling thread finished");
    assert!(token.is_cancelled());
    token.cancel();
    assert!(token.is_cancelled(), "cancelling twice is the same as once");
    assert!(!CancelToken::default().is_cancelled());
}
