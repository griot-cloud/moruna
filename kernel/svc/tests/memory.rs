//! Host model proof only; actual kernel accounting/DMA requires boot acceptance.
#![cfg(not(target_os = "none"))]
use moruna_svc::{Channel, Error, Memory, Rights, map};
#[test]
fn memory_loans_share_backing_and_enforce_mapping_rights_and_bounds() {
    let original = Memory::create(4096).unwrap();
    let loan = Memory(
        original
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .unwrap(),
    );
    loan.map(0, 4096, map::WRITE)
        .unwrap()
        .write(4093, b"abc")
        .unwrap();
    let mut result = [0; 3];
    original
        .map(0, 4096, map::READ)
        .unwrap()
        .read(4093, &mut result)
        .unwrap();
    assert_eq!(&result, b"abc");
    assert!(original.map(0, 8192, map::READ).is_err());
    assert!(original.map(u64::MAX - 4095, 4096, map::READ).is_err());
    assert!(
        original
            .map(0, 4096, map::READ)
            .unwrap()
            .write(0, b"x")
            .is_err()
    );
    let read_only = Memory(original.0.duplicate(Rights::READ | Rights::MAP).unwrap());
    assert!(read_only.map(0, 4096, map::WRITE).is_err());
    assert!(read_only.0.duplicate(Rights::ALL).is_err());
}
#[test]
fn channel_transfer_cannot_upgrade_nontransferable_memory() {
    let memory = Memory::create(4096).unwrap();
    let loan = memory.0.duplicate(Rights::READ).unwrap();
    let (sender, _receiver) = Channel::create().unwrap();
    let mut handles = [Some(loan)];
    assert_eq!(
        sender.write_with(b"", &mut handles),
        Err(Error::AccessDenied)
    );
    assert!(handles[0].is_some());
}

#[test]
fn model_matches_native_page_rounding_and_range_refusal() {
    let memory = Memory::create(1).unwrap();
    memory
        .map(0, 4096, map::WRITE)
        .unwrap()
        .write(4095, &[1])
        .unwrap();
    assert!(matches!(
        memory.map(0, 8192, map::READ),
        Err(Error::OutOfRange)
    ));
    assert!(matches!(
        Memory::create(moruna_svc::limits::MEMORY_MAX_BYTES + 1),
        Err(Error::Limit)
    ));
}
