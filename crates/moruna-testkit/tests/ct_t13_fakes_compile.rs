//! CT-T13 fakes_compile: `moruna-testkit` implements every trait in contracts d.3 to d.13 with
//! the knobs in d.15, and this test exercises every method and every knob once. Proves the
//! contract is implementable.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::{
    Allocator, BufferView, CheckpointExtras, Completion, CopyDst, CopySrc, DType, DeviceId,
    Fingerprint, InitCtx, IoPaths, Kernel, KernelKind, Knob, Knobs, Locality, Morsel, MorunaError,
    NodeId, ObjectMetadata, Origin, Outcome, Payload, PayloadKind, PayloadSpec, Placement,
    ProbeResult, Prober, Reactor, ResumePolicy, RowRange, Sample, Sampler, SchedulerStats,
    SegmentRef, Sink, Source, SourceCursor, SourceSchema, StageStats, StatsSource, Tier,
    TierBudgets, TierKind, TierPref, TraceRecord, TraceSink, TraceTail,
};
use moruna_testkit::{
    FakeAllocator, FakeKernel, FakeKnobs, FakePlacement, FakeReactor, FakeSampler, FakeSink,
    FakeSource, FakeTrace, OpKind,
};

fn origin(split: u32) -> Origin {
    Origin {
        split,
        row_start: 0,
        row_end: 4,
        node: NodeId::default(),
    }
}

fn spec() -> PayloadSpec {
    PayloadSpec {
        kind: PayloadKind::Either,
        tier: TierPref::Any,
    }
}

fn table_schema() -> SourceSchema {
    SourceSchema::Table(Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        false,
    )])))
}

fn morsel(alloc: &FakeAllocator, seq: u64, split: u32) -> Morsel {
    let buffer = alloc.buffer(64, Tier::Host);
    let tensor = moruna_kernel::ManagedTensor::from_buffer(buffer, 0, DType::I64, vec![8])
        .expect("a tensor over a buffer");
    let payload = Payload::tensor(tensor).expect("a tensor payload");
    Morsel::new(seq, 0, payload, origin(split))
}

#[test]
fn ct_t13_fakes_compile() {
    fake_allocator();
    fake_reactor();
    fake_placement();
    fake_source();
    fake_sink();
    fake_kernel();
    fake_sampler();
    fake_trace();
    fake_knobs();
}

/// `FakeAllocator`: knobs `with_limit`, `pinned`, `page_bytes`, `fail_next`; observables
/// `allocations_total`, `in_use`, `AllocStats`; and the `Allocator` trait's own methods.
fn fake_allocator() {
    let alloc = FakeAllocator::new()
        .with_limit(Tier::Host, 8192)
        .pinned(false)
        .page_bytes(4096)
        .fail_next(1);

    // The knob fails exactly one allocation, and the next one succeeds.
    let err = alloc.alloc(16, Tier::Host).expect_err("fail_next(1)");
    assert!(matches!(err, MorunaError::Alloc { .. }), "got {err}");
    let buffer = alloc.buffer(4096, Tier::Host);
    assert_eq!(buffer.len(), 4096);
    assert_eq!(buffer.tier(), Tier::Host);
    assert_eq!(alloc.allocations_total(), 1);
    assert_eq!(alloc.in_use(Tier::Host), 4096);
    assert_eq!(Allocator::page_bytes(&alloc), 4096);
    assert!(!alloc.is_pinned());
    assert_eq!(alloc.host_tier(), Tier::Host);
    let stats = alloc.stats();
    assert_eq!(stats.host_in_use, 4096);
    assert_eq!(stats.allocations_total, 1);
    assert_eq!(stats.payload_copies_total, 0);
    assert_eq!(stats.boundary_copies_total, 0);

    // The limit refuses what would exceed it, and the accounting comes back on drop.
    assert!(alloc.alloc(8192, Tier::Host).is_err());
    let host_ptr = buffer.host_ptr().expect("a host buffer");
    assert!(alloc.contains(host_ptr.cast_const()));
    assert_eq!(alloc.tier_of(host_ptr.cast_const()), Some(Tier::Host));
    drop(buffer);
    assert_eq!(alloc.in_use(Tier::Host), 0);
    assert!(!alloc.contains(host_ptr.cast_const()));

    // The buffers are real allocations tagged with their tier, so tier inference,
    // `into_arrow_buffer` and `BufferView::of_arrow` all work over them (d.15).
    let pinned = FakeAllocator::new().pinned(true);
    assert!(pinned.is_pinned());
    assert_eq!(pinned.host_tier(), Tier::PinnedHost);
    let arrow = pinned.arrow_buffer(&[1u8, 2, 3, 4, 5, 6, 7, 8], Tier::PinnedHost);
    let view = BufferView::of_arrow(&arrow, &pinned).expect("a view over an arena buffer");
    assert_eq!(view.tier(), Tier::PinnedHost);
    assert_eq!(view.len(), 8);
    assert_eq!(pinned.in_use(Tier::PinnedHost), 8);
    let device = FakeAllocator::new().buffer(32, Tier::Device(DeviceId(1)));
    assert_eq!(device.tier(), Tier::Device(DeviceId(1)));
    let seg = SegmentRef {
        segment: 0,
        offset: 0,
        len: 32,
    };
    assert!(
        FakeAllocator::new().alloc(32, Tier::Disk(seg)).is_err(),
        "a tier with no bytes"
    );
}

/// `FakeReactor`: knobs `with_latency`, `fail_next`, `cancel_on_shutdown`, `with_paths`,
/// `with_file`; observables `ops`, `in_flight`, `shutdown_calls`, `paths`; and both traits.
fn fake_reactor() {
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new()
        .with_paths(IoPaths {
            direct_io: true,
            ..IoPaths::default()
        })
        .cancel_on_shutdown(true)
        .with_file("/staging/seg-000000.seg", vec![7u8; 64]);
    assert!(reactor.paths().direct_io);

    // read_file over an in-memory file.
    let dst = alloc.buffer(64, Tier::Host);
    let bytes = reactor
        .read_file(Path::new("/staging/seg-000000.seg"), 0, dst)
        .wait();
    let bytes = bytes.expect("the read resolved");
    assert_eq!(bytes[..4], [7, 7, 7, 7]);
    assert_eq!(reactor.in_flight(), 0);

    // read_file_opt with a short read allowed.
    let dst = alloc.buffer(128, Tier::Host);
    let (_, taken) = reactor
        .read_file_opt(Path::new("/staging/seg-000000.seg"), 0, dst, true)
        .wait()
        .expect("the short read resolved");
    assert_eq!(taken, 64);

    // write_file, then read the bytes back through the observable.
    let source = Arc::new(alloc.buffer(16, Tier::Host));
    let view = source.view();
    reactor
        .write_file(Path::new("/staging/out"), 0, view)
        .wait()
        .expect("the write resolved");
    assert_eq!(reactor.file("/staging/out").map(|f| f.len()), Some(16));

    // Objects, their metadata and a prefix listing.
    let payload = Arc::new(alloc.buffer(8, Tier::Host));
    reactor
        .write_object("s3://bucket/a", payload.view())
        .wait()
        .expect("the object write");
    let meta = reactor
        .head_object("s3://bucket/a")
        .wait()
        .expect("the head resolved");
    assert_eq!(meta.size, 8);
    assert_eq!(meta.url, "s3://bucket/a");
    assert!(meta.e_tag.is_none() && meta.last_modified_ns.is_none());
    let listed = reactor
        .list_prefix("s3://bucket/")
        .wait()
        .expect("the listing resolved");
    assert_eq!(listed.len(), 1);
    let dst = alloc.buffer(8, Tier::Host);
    assert!(reactor.read_object("s3://bucket/a", 0, dst).wait().is_ok());

    // copy between tiers, and the segment registry.
    let src = Arc::new(alloc.buffer(8, Tier::Host));
    let dst = alloc.buffer(8, Tier::Host);
    let copied = reactor
        .copy(CopySrc::View(src.view()), CopyDst::Buffer(dst))
        .wait()
        .expect("the copy resolved");
    assert!(copied.is_some());
    let seg = SegmentRef {
        segment: 3,
        offset: 0,
        len: 8,
    };
    let staged = reactor
        .copy(CopySrc::Disk(seg), CopyDst::Disk(seg))
        .wait()
        .expect("a disk to disk copy resolved");
    assert!(staged.is_none());
    reactor
        .register_segment(3, Path::new("/staging/seg-000003.seg"))
        .expect("registered");
    assert_eq!(reactor.segments(), vec![3]);
    reactor.unregister_segment(3);
    assert!(reactor.segments().is_empty());

    // fail_next fails exactly one operation of that kind.
    let reactor = reactor.fail_next(OpKind::ReadFile, 1);
    let dst = alloc.buffer(64, Tier::Host);
    let err = reactor
        .read_file(Path::new("/staging/seg-000000.seg"), 0, dst)
        .wait()
        .expect_err("fail_next");
    assert!(matches!(err, MorunaError::Io { .. }), "got {err}");

    // Every operation is recorded, with the tiers it moved bytes between.
    let ops = reactor.ops();
    assert!(ops.len() >= 9);
    assert!(
        ops.iter()
            .any(|op| op.kind == OpKind::WriteFile && op.len == 16)
    );
    assert!(
        ops.iter()
            .any(|op| op.kind == OpKind::Copy && op.src_tier == Some(Tier::Host))
    );
    assert!(
        ops.iter()
            .all(|op| op.t_resolve.is_some_and(|at| at >= op.t_submit))
    );
    assert!(ops.iter().any(|op| op.path_or_url == "s3://bucket/a"));
    assert!(ops.iter().any(|op| op.kind == OpKind::HeadObject));
    assert!(ops.iter().any(|op| op.kind == OpKind::ListPrefix));
    assert!(ops.iter().any(|op| op.kind == OpKind::ReadObject));
    assert!(ops.iter().any(|op| op.kind == OpKind::WriteObject));
    assert!(ops.iter().any(|op| op.offset == 0));

    // shutdown is idempotent and counted.
    reactor.shutdown();
    reactor.shutdown();
    assert_eq!(reactor.shutdown_calls(), 2);

    // A latency resolves from a thread of its own, so submission does not block (RE-I6).
    let slow = FakeReactor::new()
        .with_latency(Duration::from_millis(20))
        .with_file("/slow", vec![1u8; 8]);
    let dst = alloc.buffer(8, Tier::Host);
    let completion = slow.read_file(Path::new("/slow"), 0, dst);
    assert_eq!(
        slow.in_flight(),
        1,
        "the operation is in flight, the caller is not blocked"
    );
    assert!(completion.wait().is_ok());
    assert_eq!(slow.in_flight(), 0);
    let ops = slow.ops();
    assert!(ops[0].t_resolve.expect("resolved") > ops[0].t_submit);

    // cancel_on_shutdown(false) leaves what is in flight to resolve on its own.
    let patient = FakeReactor::new().cancel_on_shutdown(false);
    patient.shutdown();
    assert_eq!(patient.shutdown_calls(), 1);

    // The two removal calls d.9 added for a sink's resume (contracts d.15, 2026-09-22).
    let resuming = FakeReactor::new();
    let alloc = FakeAllocator::new();
    let mut buf = alloc.buffer(8, Tier::Host);
    buf.copy_from_slice(&[7u8; 8]);
    let owned = std::sync::Arc::new(buf);
    resuming
        .delete_object("s3://bucket/never-written")
        .wait()
        .expect("deleting what is not there is Ok");
    resuming
        .write_object("s3://bucket/uncommitted", owned.view())
        .wait()
        .expect("write");
    assert_eq!(
        resuming.object("s3://bucket/uncommitted"),
        Some(vec![7u8; 8]),
        "object(url) reads back what a sink wrote"
    );
    resuming
        .delete_object("s3://bucket/uncommitted")
        .wait()
        .expect("delete");
    assert_eq!(
        resuming.object("s3://bucket/uncommitted"),
        None,
        "what the resume discarded is gone"
    );
    let gone = resuming
        .read_object("s3://bucket/uncommitted", 0, alloc.buffer(8, Tier::Host))
        .wait();
    assert!(matches!(gone, Err(MorunaError::Io { .. })), "got {gone:?}");
    // A delete also removes the file of the same name, because the fake keys both maps by the
    // string it was given and a sink may have written through `write_file`.
    let files = FakeReactor::new().with_file("/out/part-0", vec![1u8; 4]);
    files
        .delete_object("/out/part-0")
        .wait()
        .expect("delete a file by its name");
    assert_eq!(files.file("/out/part-0"), None);

    // An abort is recorded and never removes an object that was completed.
    resuming
        .write_object("s3://bucket/done", owned.view())
        .wait()
        .expect("write");
    resuming
        .abort_multipart("s3://bucket/done", "upload-1")
        .wait()
        .expect("abort");
    assert_eq!(
        resuming.object("s3://bucket/done"),
        Some(vec![7u8; 8]),
        "an abort abandons parts, not a finished object"
    );
    let ops = resuming.ops();
    assert!(
        ops.iter().any(
            |op| op.kind == OpKind::DeleteObject && op.path_or_url == "s3://bucket/uncommitted"
        )
    );
    assert!(
        ops.iter()
            .any(|op| op.kind == OpKind::AbortMultipart && op.path_or_url == "s3://bucket/done")
    );
    let refused = resuming
        .fail_next(OpKind::DeleteObject, 1)
        .delete_object("s3://bucket/done")
        .wait();
    assert!(
        matches!(refused, Err(MorunaError::Io { .. })),
        "fail_next reaches the new calls too: {refused:?}"
    );
}

/// `FakePlacement`: knobs `with_pressure`, `with_delay`, `with_manifest_store`; observables
/// `pushed`, `popped`, `committed`, `manifests_written`, `budgets_set`, `shutdown_calls`.
fn fake_placement() {
    let alloc = FakeAllocator::new();
    let placement = FakePlacement::new();

    // push, peek, pop and the queue's statistics.
    placement.set_consumer(0, spec());
    placement.push(0, morsel(&alloc, 0, 0)).expect("pushed");
    placement.push(0, morsel(&alloc, 1, 0)).expect("pushed");
    assert_eq!(placement.pushed(0), vec![0, 1]);
    assert!(placement.peek_resident(0, spec(), Locality::Any));
    let popped = placement
        .pop(0, spec(), Locality::Local)
        .expect("popped")
        .expect("a morsel");
    assert_eq!(popped.seq, 0);
    let (blocking, waited) = placement
        .pop_blocking(0, spec(), Locality::Any)
        .expect("popped")
        .expect("a morsel");
    assert_eq!(blocking.seq, 1);
    assert_eq!(waited, 0, "nothing waited on a resident head");
    assert_eq!(placement.popped(0), vec![0, 1]);
    let stats = placement.stats();
    assert_eq!(stats.queues.len(), 1);
    assert_eq!(stats.queues[0].stage, 0);
    assert_eq!(stats.queues[0].count, 0);
    assert_eq!(stats.in_flight_bytes, 0);

    // The knobs the controller writes through the scheduler.
    placement.set_budgets(TierBudgets {
        host: 1 << 20,
        ..TierBudgets::default()
    });
    placement.set_water(0, TierKind::Host, 512, 1024);
    placement.set_staging(0, true);
    placement.set_promotion_window(0, 4);
    assert_eq!(placement.budgets_set().len(), 1);
    assert_eq!(placement.budgets_set()[0].host, 1 << 20);
    placement.set_committed(1);
    assert_eq!(placement.committed(), Some(1));

    // is_full follows the water mark, and close drains the queue.
    assert!(!placement.is_full(0));
    placement.push(0, morsel(&alloc, 2, 0)).expect("pushed");
    placement.push(0, morsel(&alloc, 3, 0)).expect("pushed");
    placement.set_water(0, TierKind::Host, 8, 16);
    assert!(placement.is_full(0));
    placement.close(0);
    assert!(
        placement
            .pop(0, spec(), Locality::Any)
            .expect("popped")
            .is_some()
    );
    assert!(
        placement
            .pop(0, spec(), Locality::Any)
            .expect("popped")
            .is_some()
    );
    assert!(
        placement
            .pop_blocking(0, spec(), Locality::Any)
            .expect("closed")
            .is_none()
    );

    // with_pressure evicts what will not fit, and replace puts the bytes back (D5).
    let under_pressure = FakePlacement::new().with_pressure(0, 64);
    under_pressure
        .push(0, morsel(&alloc, 10, 1))
        .expect("pushed");
    under_pressure
        .push(0, morsel(&alloc, 11, 1))
        .expect("pushed");
    let evicted = under_pressure.evicted(0);
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].0, 11);
    assert_eq!(evicted[0].1.split, 1);
    under_pressure
        .replace(0, morsel(&alloc, 11, 1))
        .expect("replaced");
    assert!(under_pressure.evicted(0).is_empty());
    assert!(
        under_pressure.replace(0, morsel(&alloc, 99, 1)).is_err(),
        "no such evicted entry"
    );

    // with_delay makes a pop of a fresh entry wait, and counts the wait as a miss (PL-I9).
    let delayed = FakePlacement::new().with_delay(Duration::from_millis(15));
    delayed.push(0, morsel(&alloc, 20, 2)).expect("pushed");
    assert!(
        delayed
            .pop(0, spec(), Locality::Any)
            .expect("popped")
            .is_none(),
        "not yet resident"
    );
    let (morsel_out, waited) = delayed
        .pop_blocking(0, spec(), Locality::Any)
        .expect("popped")
        .expect("a morsel");
    assert_eq!(morsel_out.seq, 20);
    assert!(waited > 0, "the wait is reported to the caller");
    assert_eq!(delayed.stats().queues[0].misses, 1);
    assert!(delayed.stats().queues[0].miss_wait_us > 0);

    // with_manifest_store round-trips checkpoint and restore across engine instances.
    let writer = FakePlacement::new().with_manifest_store();
    writer.push(0, morsel(&alloc, 30, 3)).expect("pushed");
    let extras = CheckpointExtras {
        kernel_states: vec![(1, 0, vec![1, 2, 3])],
        sink_state: Some(vec![9]),
        committed_seq: Some(29),
        source_cursor: SourceCursor {
            split_index: 3,
            row_offset: 4,
            next_seq: 31,
        },
        issued: Vec::new(),
    };
    let path = writer.checkpoint(&extras).expect("a manifest");
    assert_eq!(writer.manifests_written(), vec![path.clone()]);
    let reader = FakePlacement::new().with_manifest_store();
    let point = reader.restore(&path, &[], &[]).expect("restored");
    assert_eq!(point.extras.committed_seq, Some(29));
    assert_eq!(point.extras.source_cursor.next_seq, 31);
    assert_eq!(point.extras.kernel_states.len(), 1);
    assert_eq!(point.extras.sink_state, Some(vec![9]));
    assert_eq!(
        point.to_recompute.len(),
        1,
        "a morsel with no disk copy is recomputed"
    );
    assert_eq!(point.to_recompute[0].0, 30);
    assert!(reader.restore(Path::new("/nowhere"), &[], &[]).is_err());
    assert!(
        FakePlacement::new().checkpoint(&extras).is_err(),
        "an engine with no staging directory refuses to checkpoint"
    );

    // shutdown is counted, and every call after it is cancelled.
    writer.shutdown();
    assert_eq!(writer.shutdown_calls(), 1);
    assert!(matches!(
        writer.push(0, morsel(&alloc, 31, 3)),
        Err(MorunaError::Cancelled)
    ));
    assert!(matches!(
        writer.pop(0, spec(), Locality::Any),
        Err(MorunaError::Cancelled)
    ));
    assert!(matches!(
        writer.pop_blocking(0, spec(), Locality::Any),
        Err(MorunaError::Cancelled)
    ));
}

/// `FakeSource`: knobs `splits`, `schema`, `sub_splittable`, `repeatable`, `fail_split`;
/// observable `reads`.
fn fake_source() {
    let alloc = FakeAllocator::new();
    let source = FakeSource::new()
        .splits(2, 4, 32)
        .schema(table_schema())
        .sub_splittable(true)
        .repeatable(true);
    let plan = source.plan().expect("a plan");
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].rows, 4);
    assert_eq!(plan[0].uncompressed_bytes, 32);
    assert!(plan[0].sub_splittable && !plan[0].estimated);
    assert_eq!(plan[0].column_bytes, vec![32]);
    assert_eq!(plan[0].null_counts, vec![Some(0)]);
    assert!(Source::repeatable(&source));
    assert!(matches!(Source::schema(&source), SourceSchema::Table(_)));

    // The content is deterministic: row i of split s holds s * 1_000_000 + i (d.15).
    let payload = futures_block_on(source.read(&plan[1], None, &alloc, Tier::Host))
        .expect("the read resolved");
    assert_eq!(payload.rows(), 4);
    match &payload {
        Payload::Table(batch, tier) => {
            assert_eq!(*tier, Tier::Host);
            let column = batch.column(0).to_data();
            let values: &[i64] = column.buffers()[0].typed_data();
            assert_eq!(values[0], FakeSource::value_at(1, 0));
            assert_eq!(values[3], FakeSource::value_at(1, 3));
        }
        Payload::Tensor(_, _) => panic!("a table schema produced a tensor"),
    }
    let rows = RowRange { start: 1, end: 3 };
    let sliced = futures_block_on(source.read(&plan[0], Some(rows), &alloc, Tier::Host))
        .expect("the sub-split read resolved");
    assert_eq!(sliced.rows(), 2);
    let reads = source.reads();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0].0, 1);
    assert!(reads[0].1.is_none());
    assert_eq!(reads[1].0, 0);
    let recorded = reads[1].1.expect("the sub-split range was recorded");
    assert_eq!((recorded.start, recorded.end), (rows.start, rows.end));

    // A tensor schema produces a tensor payload.
    let tensors = FakeSource::new()
        .schema(SourceSchema::Tensor {
            dtype: DType::I64,
            shape: vec![-1],
        })
        .splits(1, 4, 32);
    let plan = Source::plan(&tensors).expect("a plan");
    let payload = futures_block_on(tensors.read(&plan[0], None, &alloc, Tier::Host))
        .expect("the read resolved");
    assert_eq!(payload.kind(), PayloadKind::Tensor);

    // The refusals: a source that does not sub-split, and a split that fails.
    let whole = FakeSource::new().sub_splittable(false).repeatable(false);
    assert!(!Source::repeatable(&whole));
    let plan = whole.plan().expect("a plan");
    assert!(!plan[0].sub_splittable);
    let err = futures_block_on(whole.read(&plan[0], Some(rows), &alloc, Tier::Host))
        .expect_err("this source does not sub-split");
    assert!(matches!(err, MorunaError::Source { .. }), "got {err}");
    let broken = FakeSource::new().fail_split(0);
    let plan = broken.plan().expect("a plan");
    let err =
        futures_block_on(broken.read(&plan[0], None, &alloc, Tier::Host)).expect_err("fail_split");
    assert!(
        matches!(err, MorunaError::Source { split: 0, .. }),
        "got {err}"
    );
}

/// `FakeSink`: knobs `commit_every`, `resumable`, `fail_at`, `latency`, `requires_order`;
/// observables `written`, `skipped`, `committed_seq`, `open_calls`, `resume_calls`,
/// `finish_calls`, `shutdown_calls`.
fn fake_sink() {
    let alloc = FakeAllocator::new();
    let mut sink = FakeSink::new()
        .commit_every(2)
        .resumable(true)
        .latency(Duration::from_millis(1))
        .requires_order(true);
    assert!(Sink::requires_order(&sink));
    assert_eq!(sink.accepts().kind, PayloadKind::Either);
    sink.open(&table_schema()).expect("opened");
    assert_eq!(sink.open_calls(), 1);

    // A resumable sink checkpoints before its first write, which is how SC f.11 knows.
    assert!(sink.checkpoint().expect("a checkpoint").is_some());
    assert_eq!(sink.committed_seq(), None);

    for seq in 0..4u64 {
        let payload = morsel(&alloc, seq, 0).payload;
        futures_block_on(sink.write(seq, payload)).expect("the write resolved");
    }
    assert_eq!(sink.written(), vec![0, 1, 2, 3]);
    assert_eq!(
        sink.committed_seq(),
        Some(3),
        "commit_every(2) commits in blocks"
    );
    sink.skip(4);
    assert_eq!(sink.skipped(), vec![4]);
    let summary = sink.finish().expect("a summary");
    assert_eq!(sink.finish_calls(), 1);
    assert_eq!(summary.rows, 4 * 8);
    assert!(summary.bytes > 0);
    assert_eq!(summary.files.len(), 1);

    // resume discards output above the watermark and is counted.
    sink.resume(&table_schema(), &[1, 2, 3], Some(1))
        .expect("resumed");
    assert_eq!(sink.resume_calls(), 1);
    assert_eq!(sink.written(), vec![0, 1]);
    assert_eq!(sink.committed_seq(), Some(1));
    sink.shutdown();
    assert_eq!(sink.shutdown_calls(), 1);

    // fail_at fails that sequence number, and a sink that is not resumable says so.
    let failing = FakeSink::new().fail_at(7);
    let payload = morsel(&alloc, 7, 0).payload;
    let err = futures_block_on(failing.write(7, payload)).expect_err("fail_at");
    assert!(matches!(err, MorunaError::Sink(_)), "got {err}");
    let mut plain = FakeSink::new().resumable(false);
    assert!(plain.checkpoint().expect("a checkpoint").is_none());
    assert!(plain.resume(&table_schema(), &[], None).is_err());
    assert!(!Sink::requires_order(&plain));
}

/// `FakeKernel`: knobs `amplification`, `latency`, `stateful`, `resume`, `fail_on`, `panic_on`,
/// `grow_state_by`; observables `applies`, `init_calls`, `restore_calls`, `checkpoint_calls`.
fn fake_kernel() {
    let alloc = FakeAllocator::new();
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(alloc.clone()),
    };
    let kernel = FakeKernel::new()
        .amplification(2.0)
        .latency(Duration::from_millis(1))
        .stateful(2, 1024)
        .resume(ResumePolicy::Checkpoint)
        .grow_state_by(16);
    assert!(matches!(kernel.kind(), KernelKind::Stateful { .. }));
    assert_eq!(kernel.hints().expected_amplification, Some(2.0));
    assert_eq!(kernel.hints().state_bytes, Some(1024));
    assert_eq!(kernel.hints().resume, ResumePolicy::Checkpoint);
    assert_eq!(kernel.accepts().tier, TierPref::Any);
    assert!(matches!(
        kernel.output_schema(&table_schema()),
        Ok(SourceSchema::Table(_))
    ));
    assert_ne!(kernel.fingerprint(), Fingerprint::compute("other", b""));

    let mut state = kernel.init(&ctx).expect("an instance");
    assert_eq!(kernel.init_calls(), 1);
    assert_eq!(state.footprint(), Some(1024));
    let payload = morsel(&alloc, 0, 0).payload;
    let out = kernel.apply(state.as_mut(), payload).expect("applied");
    assert_eq!(out.rows(), 8);
    assert_eq!(state.footprint(), Some(1024 + 16), "grow_state_by(16)");
    let applies = kernel.applies();
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].0, 0, "the apply index");
    assert_eq!(applies[0].1, 0, "the instance");
    assert_eq!(applies[0].2, std::thread::current().id());

    // checkpoint and restore round-trip the instance's state.
    let bytes = state
        .checkpoint()
        .expect("a checkpoint")
        .expect("some state");
    assert_eq!(kernel.checkpoint_calls(), 1);
    let restored = kernel.restore(&ctx, &bytes).expect("restored");
    assert_eq!(kernel.restore_calls(), 1);
    assert_eq!(restored.footprint(), Some(1024 + 16));

    // A kernel that does not declare Checkpoint refuses to be restored.
    let reinit = FakeKernel::new().resume(ResumePolicy::Reinit);
    assert!(matches!(reinit.kind(), KernelKind::Stateless));
    assert!(reinit.restore(&ctx, &bytes).is_err());

    // fail_on and panic_on key on the apply call index.
    let failing = FakeKernel::new().fail_on(&[0usize]);
    let mut state = failing.init(&ctx).expect("an instance");
    let payload = morsel(&alloc, 1, 0).payload;
    let err = failing
        .apply(state.as_mut(), payload)
        .expect_err("fail_on(0)");
    assert!(matches!(err, MorunaError::Kernel { .. }), "got {err}");

    let panicking = FakeKernel::new().panic_on(&[0usize]);
    let mut state = panicking.init(&ctx).expect("an instance");
    let payload = morsel(&alloc, 2, 0).payload;
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = panicking.apply(state.as_mut(), payload);
    }));
    assert!(
        caught.is_err(),
        "panic_on(0) panics, and a worker may catch it"
    );
}

/// `FakeSampler`: knobs `scripted`, `live`; observables `samples_taken`, `peak_resets`.
fn fake_sampler() {
    let first = Sample {
        anon_bytes: 1,
        ..Sample::default()
    };
    let second = Sample {
        anon_bytes: 2,
        peak_anon_bytes: 9,
        ..Sample::default()
    };
    let sampler = FakeSampler::new().scripted(vec![first, second]);
    assert_eq!(sampler.sample().anon_bytes, 1);
    assert_eq!(sampler.sample().anon_bytes, 2);
    assert_eq!(sampler.sample().anon_bytes, 2, "the last sample repeats");
    assert_eq!(sampler.sample().peak_anon_bytes, 9);
    assert_eq!(sampler.samples_taken(), 4);
    sampler.reset_peak();
    assert_eq!(sampler.peak_resets(), 1);

    let live = FakeSampler::new().live();
    assert!(
        live.sample().at_ns > 0,
        "a live sample is stamped with the wall clock"
    );
    assert_eq!(live.samples_taken(), 1);
    live.reset_peak();
    assert_eq!(live.peak_resets(), 1);
    assert_eq!(
        FakeSampler::new().sample().anon_bytes,
        0,
        "no script is all zeroes"
    );
}

/// `FakeTrace`: knob `capacity`; observables `records`, `flush_calls`, `finish_calls`.
fn fake_trace() {
    let trace = FakeTrace::new().capacity(2);
    for seq in 0..3u64 {
        trace.record(record(seq, 1));
    }
    trace.record(record(9, 2));
    let kept = trace.records();
    assert_eq!(kept.len(), 2, "capacity(2) keeps the last two");
    assert_eq!(kept[0].seq, 2);
    assert_eq!(kept[1].seq, 9);
    assert_eq!(trace.tail(2, 4).len(), 1, "the tail of one stage");
    assert_eq!(trace.tail(1, 1)[0].seq, 2);
    trace.flush().expect("flushed");
    assert_eq!(trace.flush_calls(), 1);
    assert_eq!(trace.finish().len(), 2);
    assert_eq!(trace.finish_calls(), 1);
}

fn record(seq: u64, stage: u16) -> TraceRecord {
    TraceRecord {
        seq,
        stage,
        worker: 0,
        instance: u16::MAX,
        t_start_ns: 0,
        t_end_ns: 1,
        rows_in: 1,
        bytes_in: 8,
        rows_out: 1,
        bytes_out: 8,
        tier_in: Tier::Host.index() as u8,
        tier_out: Tier::Host.index() as u8,
        feat_mean_string_len: 0.0,
        feat_null_ratio: 0.0,
        feat_column_bytes: vec![8],
        knob_morsel_target: 16,
        knob_active_workers: 1,
        knob_read_ahead: 2,
        mem_anon_before: 0,
        mem_anon_peak: 0,
        dev_mem_peak: 0,
        cpu_time_us: 1,
        throttled_delta_us: 0,
        q_bytes_before: vec![0; moruna_kernel::TIER_COUNT],
        q_bytes_after: vec![0; moruna_kernel::TIER_COUNT],
        staging_bytes_delta: 0,
        placement_miss_wait_us: 0,
        state_bytes: 0,
        sizer: 0,
        outcome: Outcome::Ok,
        error: None,
    }
}

/// `FakeKnobs`: knobs `stats`, `probe_result`; observables `writes`, `terminated`, `snapshot`.
fn fake_knobs() {
    let stats = SchedulerStats {
        per_stage: vec![StageStats {
            stage: 1,
            tasks: 7,
            ..StageStats::default()
        }],
        workers_active: 4,
        ..SchedulerStats::default()
    };
    let probe = ProbeResult {
        bytes_in: 1024,
        rows_in: 8,
        peak_delta: 2048,
        dev_peak_delta: 0,
        wall_ns: 10,
        cpu_ns: 9,
    };
    let knobs = FakeKnobs::new().stats(stats).probe_result(1, probe);
    assert_eq!(knobs.scheduler_stats().workers_active, 4);
    assert_eq!(knobs.scheduler_stats().per_stage[0].tasks, 7);
    let measured = knobs.probe(1, 16).expect("a probe result");
    assert_eq!(measured.peak_delta, 2048);
    assert_eq!(measured.rows_in, 8);
    let unscripted = knobs.probe(2, 16).expect("a probe result");
    assert_eq!(
        unscripted.bytes_in, 16,
        "an unscripted stage echoes the request"
    );

    knobs.set(Knob::MorselTarget {
        stage: 0,
        bytes: 1 << 20,
    });
    knobs.set(Knob::ActiveWorkers(3));
    knobs.set(Knob::ReadAhead(4));
    knobs.set(Knob::StagingTrigger { stage: 0, on: true });
    knobs.set(Knob::HighWater {
        stage: 0,
        tier: TierKind::Host,
        bytes: 4096,
    });
    knobs.set(Knob::PromotionWindow {
        stage: 0,
        morsels: 2,
    });
    assert_eq!(knobs.writes().len(), 6);
    let snapshot = knobs.snapshot();
    assert_eq!(snapshot.morsel_target, vec![(0, 1 << 20)]);
    assert_eq!(snapshot.active_workers, 3);
    assert_eq!(snapshot.read_ahead, 4);
    assert_eq!(snapshot.staging, vec![(0, true)]);
    assert_eq!(snapshot.high_water, vec![(0, TierKind::Host, 4096)]);
    assert_eq!(snapshot.promotion_window, vec![(0, 2)]);

    assert!(knobs.terminated().is_none());
    knobs.terminate(MorunaError::Budget {
        seq: 3,
        stage: 1,
        footprint: 10,
        budget: 5,
        features: moruna_kernel::MorselFeatures::default(),
    });
    let diagnostic = knobs.terminated().expect("the run was terminated");
    assert!(diagnostic.contains("morsel 3"));
    knobs.terminate(MorunaError::Cancelled);
    assert!(
        knobs
            .terminated()
            .expect("still the first")
            .contains("morsel 3")
    );
}

/// Drive a `BoxFuture` to completion on this thread: the fakes resolve without a runtime, so a
/// poll with a no-op waker is enough and no async runtime enters the testkit.
fn futures_block_on<T>(mut future: moruna_kernel::BoxFuture<'_, T>) -> T {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A completion the testkit resolves at once, for a test that wants the value without a wait.
#[allow(dead_code)]
fn resolved_completion() -> Completion<u8> {
    Completion::resolved(Ok(1))
}
