//! `TensorSink`: SI-T1, SI-T4, SI-T6 and SI-T10 (08 k).

mod common;

use std::sync::Arc;

use common::{CountingAlloc, Scratch, arena_tensor, block_on, tensor_source_schema, written};
use moruna_kernel::{Allocator, MorunaError, Sink, Tier};
use moruna_sinks::{TensorFormat, TensorSink, TensorSinkConfig};
use moruna_testkit::{FakeAllocator, FakeReactor, OpKind};

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
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, true);
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
    use moruna_kernel::{DeviceId, Payload};
    let scratch = Scratch::new("t6");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    let batch = common::arena_batch(alloc.fake(), 8, 0);
    // E9: test code may use `unsafe` to construct a state a test needs.
    let payload = unsafe { Payload::table_in(batch, Tier::Device(DeviceId(0))) };
    let outcome = block_on(sink.write(0, payload));
    let Err(MorunaError::Sink(msg)) = outcome else {
        panic!("a device payload must be refused, got {outcome:?}");
    };
    assert_eq!(msg, "device payload");
}

/// The header length a file declares and where its data section starts.
fn data_start(bytes: &[u8]) -> u64 {
    let mut len = [0u8; 8];
    len.copy_from_slice(&bytes[..8]);
    8 + u64::from_le_bytes(len)
}

/// SI-T10. The byte-level half, which needs no other component: every file is safetensors the
/// crate itself reads back, its offsets are the bytes that were written, and its data section
/// starts on a page boundary. e.4.
#[test]
fn si_t10_safetensors() {
    // One file per morsel: the sequence number is the file name (e.4).
    let scratch = Scratch::new("t10-per-morsel");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_tensor(alloc.fake(), 8, 4, seq as f32))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(
        summary.files,
        vec![
            "weights-000000000000.safetensors".to_string(),
            "weights-000000000001.safetensors".to_string(),
            "weights-000000000002.safetensors".to_string(),
        ]
    );
    assert_eq!(sink.committed_seq(), Some(2));
    for (seq, name) in summary.files.iter().enumerate() {
        let bytes = written(&reactor, scratch.path(), name);
        assert_eq!(data_start(&bytes), 4096, "the data section is page aligned");
        let file = safetensors::SafeTensors::deserialize(&bytes).expect("a valid safetensors file");
        let view = file.tensor("weights").expect("the tensor entry");
        assert_eq!(view.dtype(), safetensors::Dtype::F32);
        assert_eq!(view.shape(), &[8, 4]);
        assert_eq!(view.data().len(), 8 * 4 * 4);
        let first = f32::from_le_bytes([
            view.data()[0],
            view.data()[1],
            view.data()[2],
            view.data()[3],
        ]);
        assert_eq!(first, seq as f32);
    }

    // One file for the run: the header is written at `finish`, with the total shape.
    let scratch = Scratch::new("t10-run");
    let (mut sink, reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, false);
    sink.open(&tensor_source_schema(4)).expect("open");
    for seq in 0..3 {
        block_on(sink.write(seq, arena_tensor(alloc.fake(), 8, 4, seq as f32))).expect("write");
    }
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["weights.safetensors".to_string()]);
    let bytes = written(&reactor, scratch.path(), "weights.safetensors");
    assert_eq!(data_start(&bytes), 4096, "the data section is page aligned");
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

/// The name is the user's: a quote in it stays inside the JSON key, and a name that is a path
/// rather than a file name is refused before anything is created.
#[test]
fn the_tensor_name_is_escaped_and_must_be_a_file_name() {
    let scratch = Scratch::new("tensor-name");
    let reactor = FakeReactor::new();
    let alloc = CountingAlloc::new(FakeAllocator::new());
    let mut sink = TensorSink::new(
        TensorSinkConfig {
            path: scratch.path().to_path_buf(),
            format: TensorFormat::SafeTensors,
            one_file_per_morsel: false,
            name: "a\"b".to_string(),
        },
        Arc::new(reactor.clone()),
        Arc::clone(&alloc) as Arc<dyn Allocator>,
    )
    .expect("a quote is a legal file name");
    sink.open(&tensor_source_schema(4)).expect("open");
    block_on(sink.write(0, arena_tensor(alloc.fake(), 2, 4, 1.0))).expect("write");
    let summary = sink.finish().expect("finish");
    let bytes = written(&reactor, scratch.path(), &summary.files[0]);
    let file = safetensors::SafeTensors::deserialize(&bytes).expect("a valid safetensors file");
    assert!(
        file.tensor("a\"b").is_ok(),
        "the key is the name, quote included"
    );

    for name in ["", ".", "..", "../up", "a/b", "a\\b", "nul\0"] {
        let refused = TensorSink::new(
            TensorSinkConfig {
                path: scratch.path().to_path_buf(),
                format: TensorFormat::SafeTensors,
                one_file_per_morsel: true,
                name: name.to_string(),
            },
            Arc::new(FakeReactor::new()),
            Arc::new(FakeAllocator::new()) as Arc<dyn Allocator>,
        );
        assert!(refused.is_err(), "{name:?} must be refused");
    }
}

/// SI-T10. (integration, closes in wave 3) Both shapes round-trip through `TensorSource`. e.4.
#[test]
#[ignore = "integration, closes in wave 3: TensorSource is component 7"]
fn si_t10_safetensors_roundtrip() {
    unimplemented!("needs moruna-sources TensorSource, which lands in the same wave");
}

/// A tensor whose dtype or shape is not the one the sink opened with is refused by name (h).
#[test]
fn tensor_schema_drift_is_refused() {
    let scratch = Scratch::new("tensor-drift");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, true);
    sink.open(&tensor_source_schema(4)).expect("open");
    let outcome = block_on(sink.write(0, arena_tensor(alloc.fake(), 8, 5, 0.0)));
    let Err(MorunaError::Sink(msg)) = outcome else {
        panic!("shape drift must be refused, got {outcome:?}");
    };
    assert!(msg.contains("schema drift"), "{msg}");

    // A table payload is not a tensor, and a table schema is not a tensor schema.
    let scratch = Scratch::new("tensor-kind");
    let (mut sink, _reactor, alloc) = new_sink(&scratch, TensorFormat::SafeTensors, true);
    assert!(sink.open(&common::table_source_schema()).is_err());
    sink.open(&tensor_source_schema(4)).expect("open");
    let outcome = block_on(sink.write(0, common::arena_payload(alloc.fake(), 8, 0)));
    assert!(matches!(outcome, Err(MorunaError::Sink(_))), "{outcome:?}");
}

/// A tensor sink with zero writes still produces a header-only file (h).
#[test]
fn an_empty_tensor_run_writes_a_header() {
    let scratch = Scratch::new("tensor-empty");
    let (mut sink, reactor, _alloc) = new_sink(&scratch, TensorFormat::SafeTensors, false);
    sink.open(&tensor_source_schema(4)).expect("open");
    let summary = sink.finish().expect("finish");
    assert_eq!(summary.files, vec!["weights.safetensors".to_string()]);
    let bytes = written(&reactor, scratch.path(), "weights.safetensors");
    let file = safetensors::SafeTensors::deserialize(&bytes).expect("a valid safetensors file");
    let view = file.tensor("weights").expect("the tensor entry");
    assert_eq!(view.shape(), &[0, 4]);
    assert!(view.data().is_empty());
}
