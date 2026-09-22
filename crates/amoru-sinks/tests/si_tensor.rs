//! `TensorSink`: SI-T1, SI-T4, SI-T6 and SI-T10 (08 k).

mod common;

use std::sync::Arc;

use amoru_kernel::{Allocator, AmoruError, DType, Sink, Tier, amb1};
use amoru_sinks::{TensorFormat, TensorSink, TensorSinkConfig};
use amoru_testkit::{FakeAllocator, FakeReactor, OpKind};
use common::{CountingAlloc, Scratch, arena_tensor, block_on, tensor_source_schema, written};

fn new_sink(
    scratch: &Scratch,
    format: TensorFormat,
    per_morsel: bool,
) -> (TensorSink, FakeReactor, Arc<CountingAlloc>) {
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format,
            one_file_per_morsel: per_morsel,
            name: "weights".to_string(),
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("tensor sink");
    (sink, reactor, alloc)
}

/// SI-T1 and SI-T4 for `TensorSink`. The tensor's bytes go back to the arena when the writes
/// over them resolve, and nothing is copied on the way. SI-I1, SI-I4.
#[test]
fn si_t1_and_t4_tensor() {
    let scratch = Scratch::new("tensor-own");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    let baseline = alloc.fake().in_use(Tier::Host);

    let payload = arena_tensor(alloc.fake(), 64, 4, 0.0);
    let bytes = payload.bytes();
    assert!(alloc.fake().in_use(Tier::Host) > baseline);
    block_on(sink.write(0, payload)).expect("write");
    assert_eq!(alloc.fake().in_use(Tier::Host), baseline);

    assert_eq!(alloc.payload_copies(), 0, "the tensor sink copies nothing");
    assert_eq!(sink.stats().encode_bytes, 0);
    let lengths: Vec<u64> = reactor
        .ops()
        .into_iter()
        .filter(|op| op.kind == OpKind::WriteFile)
        .map(|op| op.len)
        .collect();
    assert!(
        lengths.contains(&bytes),
        "the tensor's {bytes} bytes were written in one piece, saw {lengths:?}"
    );
    sink.finish().expect("finish");
}

/// SI-T6. (reference host, E1) A payload that arrives in device memory is refused rather than
/// copied: the placement engine demotes it before a sink sees one. SI-I6.
#[test]
#[ignore = "reference host, E1: no GPU host exists, so a device payload cannot be produced here"]
fn si_t6_device_rejected() {
    use amoru_kernel::{DeviceId, Payload};
    let scratch = Scratch::new("t6");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    let batch = common::arena_batch(alloc.fake(), 8, 0);
    // E9: test code may use `unsafe` to construct a state a test needs.
    let payload = unsafe { Payload::table_in(batch, Tier::Device(DeviceId(0))) };
    let outcome = block_on(sink.write(0, payload));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("a device payload must be refused, got {outcome:?}");
    };
    assert_eq!(msg, "device payload");
}

/// SI-T10. The byte-level half, which needs no other component: a per-morsel AMB1 file parses
/// with the contracts reader and its payload is the tensor's bytes, and the safetensors header
/// is valid JSON whose offsets match what was written. e.4.
#[test]
fn si_t10_amb1_and_safetensors() {
    // AMB1, one file per morsel: the sequence number is the file name (e.4).
    let scratch = Scratch::new("t10-amb1");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_tensor(alloc.fake(), 8, 4, seq as f32))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(
        summary.files,
        vec![
            "weights-000000000000.amb1".to_string(),
            "weights-000000000001.amb1".to_string(),
            "weights-000000000002.amb1".to_string(),
        ]
    );
    assert_eq!(sink.committed_seq(), Some(2));
    for (seq, name) in summary.files.iter().enumerate() {
        let bytes = written(&reactor, scratch.path(), name);
        let header = amb1::Header::read(&bytes).expect("the contracts reader parses the header");
        assert_eq!(header.dtype, DType::F32);
        assert_eq!(header.shape, vec![8, 4]);
        assert_eq!(header.payload_len(), 8 * 4 * 4);
        let payload = header.payload(&bytes).expect("payload");
        let first = f32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        assert_eq!(first, seq as f32);
    }

    // AMB1, one file for the run: the header is written at `finish`, with the total shape.
    let scratch = Scratch::new("t10-amb1-run");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, false);
    sink.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_tensor(alloc.fake(), 8, 4, seq as f32))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["weights.amb1".to_string()]);
    let bytes = written(&reactor, scratch.path(), "weights.amb1");
    let header = amb1::Header::read(&bytes).expect("the contracts reader parses the header");
    assert_eq!(header.shape, vec![24, 4]);
    assert_eq!(header.payload_len(), 24 * 4 * 4);

    // safetensors: the header is JSON the crate itself reads back, and its offsets are the
    // bytes that were written (e.4).
    let scratch = Scratch::new("t10-safetensors");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, false);
    sink.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_tensor(alloc.fake(), 8, 4, seq as f32))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["weights.safetensors".to_string()]);
    let bytes = written(&reactor, scratch.path(), "weights.safetensors");
    let file = safetensors::SafeTensors::deserialize(&bytes).expect("a valid safetensors file");
    let view = file.tensor("weights").expect("the tensor entry");
    assert_eq!(view.shape(), &[24, 4]);
    assert_eq!(view.data().len(), 24 * 4 * 4);
    let first = f32::from_le_bytes([
        view.data()[0],
        view.data()[1],
        view.data()[2],
        view.data()[3],
    ]);
    assert_eq!(first, 0.0);
}

/// SI-T10. (integration, closes in wave 3) Both formats round-trip through `TensorSource`. e.4.
#[test]
#[ignore = "integration, closes in wave 3: TensorSource is component 7"]
fn si_t10_amb1_and_safetensors_roundtrip() {
    unimplemented!("needs amoru-sources TensorSource, which lands in the same wave");
}

/// A tensor whose dtype or shape is not the one the sink opened with is refused by name (h).
#[test]
fn tensor_schema_drift_is_refused() {
    let scratch = Scratch::new("tensor-drift");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    let outcome = block_on(sink.write(0, arena_tensor(alloc.fake(), 8, 5, 0.0)));
    let Err(AmoruError::Sink(msg)) = outcome else {
        panic!("shape drift must be refused, got {outcome:?}");
    };
    assert!(msg.contains("schema drift"), "{msg}");

    // A table payload is not a tensor, and a table schema is not a tensor schema.
    let scratch = Scratch::new("tensor-kind");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::Amb1, true);
    assert!(sink.open(&common::table_source_schema()).is_err());
    sink.open(&tensor_source_schema(4)).expect("open");
    let outcome = block_on(sink.write(0, common::arena_payload(alloc.fake(), 8, 0)));
    assert!(matches!(outcome, Err(AmoruError::Sink(_))), "{outcome:?}");
}

/// A tensor sink with zero writes still produces a header-only file (h).
#[test]
fn an_empty_tensor_run_writes_a_header() {
    let scratch = Scratch::new("tensor-empty");
    let (mut sink, reactor, _alloc) = new_sink(&scratch, TensorFormat::Amb1, false);
    sink.open(&tensor_source_schema(4)).expect("open");
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["weights.amb1".to_string()]);
    let bytes = written(&reactor, scratch.path(), "weights.amb1");
    let header = amb1::Header::read(&bytes).expect("header");
    assert_eq!(header.shape, vec![0, 4]);
}
