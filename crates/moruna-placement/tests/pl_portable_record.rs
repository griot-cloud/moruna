//! Actual native segment export/restore, not a cloud/device/performance proof.
mod common;
use moruna_kernel::{Allocator, Buffer, BufferView, DType, DeviceId, ManagedTensor, Payload, Tier};
use moruna_placement::{
    portable_record::{self, PortableRecord},
    staging::{read::decode_record, segment::RecordHeader},
    state::PayloadRef,
};
use moruna_testkit::FakeAllocator;
use std::sync::Arc;

fn collect(record: &PortableRecord, allocator: &FakeAllocator) -> Buffer {
    let mut complete = allocator
        .alloc(record.segment_bytes() as usize, Tier::Host)
        .unwrap();
    complete.as_mut().fill(0xcc);
    let mut offset = 0;
    let mut copied = 0;
    let mut zero = 0;
    while offset < record.segment_bytes() {
        let count = (record.segment_bytes() - offset).min(1237) as usize;
        let mut chunk = allocator.alloc(count, Tier::Host).unwrap();
        chunk.as_mut().fill(0xaa);
        let pointer = chunk.host_ptr();
        let stats = record.materialize(offset, &mut chunk).unwrap();
        assert_eq!(chunk.host_ptr(), pointer);
        assert_eq!(stats.copied_bytes + stats.zero_bytes, count as u64);
        complete.as_mut()[offset as usize..offset as usize + count].copy_from_slice(chunk.as_ref());
        copied += stats.copied_bytes;
        zero += stats.zero_bytes;
        offset += count as u64;
    }
    assert_eq!(copied + zero, record.segment_bytes());
    let mut end = 0;
    for piece in record.pieces() {
        assert!(
            complete.as_ref()[end as usize..piece.offset as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
        end = piece.offset + piece.len;
    }
    assert!(
        complete.as_ref()[end as usize..]
            .iter()
            .all(|byte| *byte == 0)
    );
    complete
}
#[test]
fn portable_table_chunks_restore_through_the_existing_native_reader() {
    let allocator = FakeAllocator::new();
    let batch = common::int_batch(&allocator, 23, Tier::Host);
    let payload = PayloadRef::Table(batch.clone());
    let record = portable_record::encode(&payload, 3, 41, 777, &allocator, 4096).unwrap();
    drop(payload);
    assert_eq!(record.disk().segment, 777);
    assert_eq!(record.disk().offset, 4096);
    let encoded = collect(&record, &allocator);
    let header = RecordHeader::read(&encoded.as_ref()[..4096]).unwrap();
    assert_eq!(header.stage, 3);
    assert_eq!(header.seq, 41);
    assert_eq!(header.body_offset, record.disk().offset);
    assert_eq!(header.payload_len, record.disk().len);
    let restored = decode_record(encoded, 4096).unwrap();
    let Payload::Table(actual, _) = restored.payload else {
        panic!("actual table required")
    };
    assert_eq!(actual, batch);
}
#[test]
fn portable_tensor_chunks_preserve_real_bytes_and_refuse_device_or_invalid_ranges() {
    let allocator = FakeAllocator::new();
    let mut buffer = allocator.alloc(60, Tier::Host).unwrap();
    let expected: Vec<u8> = (0..15u32).flat_map(u32::to_le_bytes).collect();
    buffer.as_mut().copy_from_slice(&expected);
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![3, 5]).unwrap();
    let payload = PayloadRef::Tensor(Arc::new(tensor));
    assert!(portable_record::encode(&payload, 4, 5, 9, &allocator, 0).is_err());
    let record = portable_record::encode(&payload, 4, 5, 9, &allocator, 4096).unwrap();
    drop(payload);
    let encoded = collect(&record, &allocator);
    let restored = decode_record(encoded, 4096).unwrap();
    let Payload::Tensor(actual, _) = restored.payload else {
        panic!("actual tensor required")
    };
    assert_eq!(actual.shape(), &[3, 5]);
    let view = BufferView::of_tensor(&Arc::new(actual)).unwrap();
    assert_eq!(view.as_host_slice().unwrap(), expected);
    let mut bad = allocator.alloc(8, Tier::Host).unwrap();
    bad.as_mut().fill(0xaa);
    assert!(
        record
            .materialize(record.segment_bytes(), &mut bad)
            .is_err()
    );
    assert_eq!(bad.as_ref(), &[0xaa; 8]);
    let fake_device = allocator.alloc(16, Tier::Device(DeviceId(0))).unwrap();
    let tensor = ManagedTensor::from_buffer(fake_device, 0, DType::F32, vec![4]).unwrap();
    assert!(
        portable_record::encode(
            &PayloadRef::Tensor(Arc::new(tensor)),
            4,
            6,
            10,
            &allocator,
            4096
        )
        .is_err()
    );
}

#[test]
fn portable_record_propagates_metadata_admission_refusal_without_fallback() {
    let allocator = FakeAllocator::new();
    let buffer = allocator.alloc(16, Tier::Host).unwrap();
    let tensor = ManagedTensor::from_buffer(buffer, 0, DType::F32, vec![4]).unwrap();
    let payload = PayloadRef::Tensor(Arc::new(tensor));
    let before = allocator.allocations_total();
    let refusing = allocator.clone().fail_next(1);
    assert!(matches!(
        portable_record::encode(&payload, 4, 5, 9, &refusing, 4096),
        Err(moruna_kernel::MorunaError::Alloc { .. })
    ));
    assert_eq!(
        allocator.allocations_total(),
        before,
        "no alternate allocator or hidden metadata buffer"
    );
}

#[test]
fn portable_short_prefix_reuses_full_admitted_chunk_without_touching_suffix() {
    let allocator = FakeAllocator::new();
    let payload = PayloadRef::Table(common::int_batch(&allocator, 3, Tier::Host));
    let record = portable_record::encode(&payload, 3, 1, 77, &allocator, 4096).unwrap();
    let mut upload = allocator.alloc(4096, Tier::Host).unwrap();
    upload.as_mut().fill(0x5a);
    let pointer = upload.host_ptr();
    let before = allocator.allocations_total();
    let stats = record
        .materialize_prefix(record.segment_bytes() - 17, 17, &mut upload)
        .unwrap();
    assert_eq!(stats.copied_bytes + stats.zero_bytes, 17);
    assert_eq!(upload.host_ptr(), pointer);
    assert_eq!(upload.len(), 4096);
    assert!(upload.as_ref()[17..].iter().all(|b| *b == 0x5a));
    assert_eq!(allocator.allocations_total(), before);
    assert!(record.materialize_prefix(0, 4097, &mut upload).is_err());
    assert_eq!(upload.host_ptr(), pointer);
}
