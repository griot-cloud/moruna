//! What the host can do wrong once the device is ready: SCSI errors, completions that are
//! not completions, packets it has no business sending, and rings it has scribbled on. Each
//! is an error to the caller, and none of them a panic.

use std::sync::atomic::Ordering;

use moruna_storvsc::{
    AdditionalSenseCode, Address, Error, Event, Failure, Malformed, NtStatus, Operation, PAGE,
    ScsiError, ScsiStatus, Sense, SenseKey, SrbStatus, Storvsc,
};
use scsi_defs::SenseData;
use vmbus_ring::RingMem;

use crate::{
    device, disk,
    fake::{DISK, Fault, Host, Link, PACKET, Raw, pair_sized},
    moved, next, ready,
};

/// The failure of the next completion, which must be `id`'s.
fn failure(device: &mut Storvsc<Link>, id: u64) -> Failure {
    let completion = next(device);
    assert_eq!(completion.id, id);
    completion.result.expect_err("a failure")
}

/// Once a ring is corrupt every call answers `Failed`.
fn assert_failed(device: &mut Storvsc<Link>) {
    assert!(matches!(device.poll(), Err(Error::Failed)));
    assert!(matches!(device.wait(), Err(Error::Failed)));
    assert!(matches!(device.flush(DISK), Err(Error::Failed)));
    assert!(matches!(device.read_serial(DISK, 1), Err(Error::Failed)));
}

#[test]
fn a_scsi_error_is_surfaced_with_its_sense_data() {
    let (mut device, host, _) = device();
    let disk = disk(&mut device, &host, 0);
    host.borrow_mut().faults.push_back(Fault::Scsi {
        srb: SrbStatus::ERROR,
        scsi: ScsiStatus::CHECK_CONDITION,
        sense: Some(SenseData::new(
            SenseKey::MEDIUM_ERROR,
            AdditionalSenseCode::UNRECOVERED_ERROR,
            2,
        )),
    });
    let id = device.read(&disk, 5, 1, &[1]).unwrap();
    assert_eq!(
        failure(&mut device, id),
        Failure::Scsi(ScsiError {
            status: NtStatus::IO_DEVICE_ERROR,
            srb: SrbStatus::ERROR,
            scsi: ScsiStatus::CHECK_CONDITION,
            sense: Some(Sense {
                key: SenseKey::MEDIUM_ERROR,
                code: AdditionalSenseCode::UNRECOVERED_ERROR,
                qualifier: 2,
            }),
        })
    );
    assert_eq!(device.in_flight(), 0);
    // The device goes on: the same read succeeds when the disk does.
    let id = device.read(&disk, 5, 1, &[1]).unwrap();
    assert_eq!(moved(&mut device, id), 512);
}

#[test]
fn a_disk_that_is_not_there_is_a_scsi_error() {
    let (mut device, host, _) = device();
    let nowhere = Address {
        path: 0,
        target: 0,
        lun: 9,
    };
    let expected = Failure::Scsi(ScsiError {
        status: NtStatus::DEVICE_DOES_NOT_EXIST,
        srb: SrbStatus::INVALID_LUN,
        scsi: ScsiStatus::CHECK_CONDITION,
        sense: None,
    });
    let id = device.read_capacity(nowhere, 1).unwrap();
    assert_eq!(failure(&mut device, id), expected);
    let id = device.read_serial(nowhere, 1).unwrap();
    assert_eq!(failure(&mut device, id), expected);
    let id = device.flush(nowhere).unwrap();
    assert_eq!(failure(&mut device, id), expected);
    assert_eq!(host.borrow().held(), 0);
}

#[test]
fn a_disk_with_no_serial_is_a_scsi_error() {
    let (mut device, host, _) = device();
    host.borrow_mut()
        .disks
        .get_mut(&(DISK.path, DISK.target, DISK.lun))
        .unwrap()
        .serial
        .clear();
    let id = device.read_serial(DISK, 1).unwrap();
    assert_eq!(
        failure(&mut device, id),
        Failure::Scsi(ScsiError {
            status: NtStatus::IO_DEVICE_ERROR,
            srb: SrbStatus::INVALID_REQUEST,
            scsi: ScsiStatus::CHECK_CONDITION,
            sense: Some(Sense {
                key: SenseKey::ILLEGAL_REQUEST,
                code: AdditionalSenseCode::INVALID_CDB,
                qualifier: 0,
            }),
        })
    );
}

#[test]
fn a_range_the_host_refuses_is_a_scsi_error() {
    let (mut device, host, _) = device();
    let disk = disk(&mut device, &host, 0);
    // The disk shrinks under a guest that still believes the capacity it read.
    host.borrow_mut()
        .disks
        .get_mut(&(DISK.path, DISK.target, DISK.lun))
        .unwrap()
        .bytes
        .truncate(100 * 512);
    for write in [false, true] {
        let id = if write {
            device.write(&disk, 99, 2, &[1]).unwrap()
        } else {
            device.read(&disk, 99, 2, &[1]).unwrap()
        };
        assert_eq!(
            failure(&mut device, id),
            Failure::Scsi(ScsiError {
                status: NtStatus::IO_DEVICE_ERROR,
                srb: SrbStatus::INVALID_REQUEST,
                scsi: ScsiStatus::CHECK_CONDITION,
                sense: Some(Sense {
                    key: SenseKey::ILLEGAL_REQUEST,
                    code: AdditionalSenseCode::ILLEGAL_BLOCK,
                    qualifier: 0,
                }),
            })
        );
    }
}

#[test]
fn sense_data_is_read_in_either_format_and_only_when_marked_valid() {
    let (mut device, host, _) = device();
    let scsi = |device: &mut Storvsc<Link>, id| match failure(device, id) {
        Failure::Scsi(error) => error,
        other => panic!("{other:?}"),
    };
    let mut fixed = [0u8; 20];
    // Fixed format, deferred, with the valid bit and the flag bits beside the key set.
    (fixed[0], fixed[2], fixed[12], fixed[13]) = (0xf1, 0xe3, 0x11, 0x04);
    let mut descriptor = [0u8; 20];
    (descriptor[0], descriptor[1], descriptor[2], descriptor[3]) = (0x72, 0x05, 0x21, 0x01);
    let mut deferred = descriptor;
    deferred[0] = 0x73;
    let mut unknown = fixed;
    unknown[0] = 0x7f;
    let expect = [
        (fixed, Some((SenseKey::MEDIUM_ERROR, 0x11, 0x04))),
        (descriptor, Some((SenseKey::ILLEGAL_REQUEST, 0x21, 0x01))),
        (deferred, Some((SenseKey::ILLEGAL_REQUEST, 0x21, 0x01))),
        (unknown, None),
    ];
    for (bytes, sense) in expect {
        host.borrow_mut().faults.push_back(Fault::Sense(bytes));
        let id = device.flush(DISK).unwrap();
        let error = scsi(&mut device, id);
        assert_eq!(
            (error.status, error.srb, error.scsi),
            (
                NtStatus::IO_DEVICE_ERROR,
                SrbStatus::ERROR,
                ScsiStatus::CHECK_CONDITION
            )
        );
        assert_eq!(
            error.sense,
            sense.map(|(key, code, qualifier)| Sense {
                key,
                code: AdditionalSenseCode(code),
                qualifier,
            }),
            "sense bytes {bytes:02x?}"
        );
    }

    // Sense bytes the completion does not mark as valid are not read.
    host.borrow_mut().faults.push_back(Fault::Scsi {
        srb: SrbStatus::ERROR,
        scsi: ScsiStatus::CHECK_CONDITION,
        sense: None,
    });
    let id = device.flush(DISK).unwrap();
    assert_eq!(scsi(&mut device, id).sense, None);
}

#[test]
fn a_status_that_is_not_success_fails_the_request() {
    let (mut device, host, _) = device();
    for status in [
        NtStatus::IO_TIMEOUT,
        NtStatus::DEVICE_BUSY,
        NtStatus::CANCELLED,
        NtStatus(0xdead_beef),
    ] {
        host.borrow_mut().faults.push_back(Fault::Status(status));
        let id = device.flush(DISK).unwrap();
        assert_eq!(
            failure(&mut device, id),
            Failure::Scsi(ScsiError {
                status,
                srb: SrbStatus::SUCCESS,
                scsi: ScsiStatus::GOOD,
                sense: None,
            })
        );
    }
    for (srb, scsi) in [
        (SrbStatus::SUCCESS, ScsiStatus::BUSY),
        (SrbStatus::BUSY, ScsiStatus::GOOD),
        (SrbStatus::DATA_OVERRUN, ScsiStatus::GOOD),
        (SrbStatus::ABORTED, ScsiStatus::TASK_ABORTED),
    ] {
        host.borrow_mut().faults.push_back(Fault::Scsi {
            srb,
            scsi,
            sense: None,
        });
        let id = device.flush(DISK).unwrap();
        match failure(&mut device, id) {
            Failure::Scsi(error) => assert_eq!((error.srb, error.scsi), (srb, scsi)),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_completion_for_an_unknown_transaction_is_an_error_and_the_device_goes_on() {
    let (mut device, host, _) = device();
    let packet = Host::packet(Operation::COMPLETE_IO, NtStatus::SUCCESS, &[]);
    for id in [0, 1, 5, 6, 7, u64::MAX] {
        assert!(host.borrow_mut().inject(Raw::Completion, id, &packet));
        match device.poll() {
            Err(Error::UnknownTransaction(got)) => assert_eq!(got, id),
            other => panic!("{other:?} for a completion of {id}"),
        }
        assert!(matches!(device.poll(), Ok(None)));
    }
    // A request in flight is not touched by a completion beside it.
    host.borrow_mut().hold = true;
    let id = device.flush(DISK).unwrap();
    assert!(host.borrow_mut().inject(Raw::Completion, id + 1, &packet));
    assert!(matches!(
        device.poll(),
        Err(Error::UnknownTransaction(got)) if got == id + 1
    ));
    assert_eq!(device.in_flight(), 1);
    host.borrow_mut().release();
    assert_eq!(moved(&mut device, id), 0);
}

#[test]
fn a_completion_sent_twice_completes_once() {
    let (mut device, host, _) = device();
    host.borrow_mut().faults.push_back(Fault::Twice);
    let id = device.flush(DISK).unwrap();
    assert_eq!(moved(&mut device, id), 0);
    assert!(matches!(
        device.poll(),
        Err(Error::UnknownTransaction(got)) if got == id
    ));
    // The ID is not given out again, so the late copy can never name a newer request.
    let again = device.flush(DISK).unwrap();
    assert_ne!(again, id);
    assert_eq!(moved(&mut device, again), 0);
}

#[test]
fn a_completion_of_the_wrong_size_fails_its_request() {
    let (mut device, host, _) = device();
    for size in [0, 8, 16, PACKET - 16, PACKET - 8, PACKET + 8, 3 * PACKET] {
        host.borrow_mut().faults.push_back(Fault::Size(size));
        let id = device.flush(DISK).unwrap();
        match failure(&mut device, id) {
            Failure::Malformed(Malformed(what)) => assert!(what.contains("size"), "{what}"),
            other => panic!("{other:?} for a completion of {size} bytes"),
        }
        assert_eq!(device.in_flight(), 0);
    }
    let id = device.flush(DISK).unwrap();
    assert_eq!(moved(&mut device, id), 0);
}

#[test]
fn a_completion_that_is_not_marked_as_one_fails_its_request() {
    let (mut device, host, _) = device();
    for operation in [
        Operation::EXECUTE_SRB,
        Operation::ENUMERATE_BUS,
        Operation(0),
        Operation(u32::MAX),
    ] {
        host.borrow_mut()
            .faults
            .push_back(Fault::Operation(operation));
        let id = device.flush(DISK).unwrap();
        match failure(&mut device, id) {
            Failure::Malformed(Malformed(what)) => {
                assert!(what.contains("COMPLETE_IO"), "{what}")
            }
            other => panic!("{other:?} for {operation:?}"),
        }
    }
}

#[test]
fn a_completion_that_moved_the_wrong_number_of_bytes_fails_its_request() {
    let (mut device, host, _) = device();
    let disk = disk(&mut device, &host, 0);

    // More than was asked for: no host moves that.
    for more in [513, 4096, u32::MAX] {
        host.borrow_mut().faults.push_back(Fault::Moved(more));
        let id = device.read(&disk, 0, 1, &[1]).unwrap();
        match failure(&mut device, id) {
            Failure::Malformed(Malformed(what)) => assert!(what.contains("more bytes"), "{what}"),
            other => panic!("{other:?} for {more} bytes"),
        }
    }
    host.borrow_mut().faults.push_back(Fault::Moved(1));
    let id = device.flush(DISK).unwrap();
    assert!(matches!(failure(&mut device, id), Failure::Malformed(_)));

    // Fewer, with a good status: a read or a write that did not finish.
    for write in [false, true] {
        host.borrow_mut().faults.push_back(Fault::Moved(511));
        let id = if write {
            device.write(&disk, 0, 1, &[1]).unwrap()
        } else {
            device.read(&disk, 0, 1, &[1]).unwrap()
        };
        assert_eq!(
            failure(&mut device, id),
            Failure::Short {
                asked: 512,
                moved: 511
            }
        );
    }

    // The capacity and the serial may be shorter than what was asked for: the decoder
    // judges what arrived.
    host.borrow_mut().faults.push_back(Fault::Moved(4));
    let id = device.read_capacity(DISK, 1).unwrap();
    assert_eq!(moved(&mut device, id), 4);
    host.borrow_mut().faults.push_back(Fault::Moved(0));
    let id = device.read_serial(DISK, 1).unwrap();
    assert_eq!(moved(&mut device, id), 0);
}

#[test]
fn the_hosts_own_packets_are_ignored_or_refused() {
    let (mut device, host, _) = device();
    let notice = Host::packet(Operation::ENUMERATE_BUS, NtStatus::SUCCESS, &[]);

    // The bus-changed notice asks nothing: it is read and dropped, whatever its length
    // past the header and whether or not it asks for a completion.
    assert!(host.borrow_mut().inject(Raw::InBand, 0, &notice));
    assert!(host.borrow_mut().inject(Raw::InBand, 0, &notice[..16]));
    assert!(
        host.borrow_mut()
            .inject(Raw::InBandWithCompletion, 77, &notice)
    );
    assert!(matches!(device.poll(), Ok(None)));

    // It does not hide the completion behind it.
    host.borrow_mut().hold = true;
    let id = device.flush(DISK).unwrap();
    assert!(host.borrow_mut().inject(Raw::InBand, 0, &notice));
    host.borrow_mut().release();
    assert_eq!(moved(&mut device, id), 0);
    assert!(matches!(device.poll(), Ok(None)));

    // Any other operation is one this driver does not serve.
    for operation in [
        Operation::REMOVE_DEVICE,
        Operation::EXECUTE_SRB,
        Operation::COMPLETE_IO,
        Operation::FC_HBA_DATA,
        Operation::CREATE_SUB_CHANNELS,
        Operation(999),
    ] {
        let packet = Host::packet(operation, NtStatus::SUCCESS, &[]);
        assert!(host.borrow_mut().inject(Raw::InBand, 0, &packet));
        match device.poll() {
            Err(Error::Unexpected(got)) => assert_eq!(got, operation),
            other => panic!("{other:?} for {operation:?}"),
        }
    }

    // One too short to say what it is.
    for short in [0, 8] {
        assert!(host.borrow_mut().inject(Raw::InBand, 0, &notice[..short]));
        match device.poll() {
            Err(Error::Malformed(Malformed(what))) => assert!(what.contains("header"), "{what}"),
            other => panic!("{other:?} for {short} bytes"),
        }
    }

    // And the kinds of packet that name memory outside the ring, which no storvsp sends.
    for raw in [Raw::GpaDirect, Raw::TransferPages] {
        assert!(host.borrow_mut().inject(raw, 3, &notice));
        assert!(
            matches!(device.poll(), Err(Error::Malformed(_))),
            "{raw:?} from the host"
        );
    }

    // After all of it the device still works.
    assert!(matches!(device.poll(), Ok(None)));
    let id = device.flush(DISK).unwrap();
    assert_eq!(moved(&mut device, id), 0);
}

#[test]
fn a_corrupt_ring_pointer_ends_the_device() {
    // The host's write pointer in the ring the guest reads: not a multiple of 8, and
    // past the ring's end.
    for pointer in [4, 12, 4 * PAGE as u32, u32::MAX] {
        let (mut device, host, _) = device();
        host.borrow().to_guest.control()[0].store(pointer, Ordering::Relaxed);
        assert!(matches!(device.poll(), Err(Error::Ring(_))), "{pointer}");
        assert_failed(&mut device);
    }
    // The same, found by a wait.
    let (mut device, host, _) = device();
    host.borrow().to_guest.control()[0].store(1, Ordering::Relaxed);
    assert!(matches!(device.wait(), Err(Error::Ring(_))));
    assert_failed(&mut device);
}

#[test]
fn a_corrupt_read_pointer_in_the_ring_to_the_host_ends_the_device() {
    let (link, host, _) = pair_sized(PAGE);
    let mut device = Storvsc::new(link, 10_000).unwrap();
    ready(&mut device);
    // The host stops reading and scribbles on its own read pointer. The guest looks at
    // that pointer when the ring seems full.
    host.borrow_mut().stalled = true;
    host.borrow().to_host.control()[1].store(5, Ordering::Relaxed);
    let error = loop {
        match device.flush(DISK) {
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    assert!(matches!(error, Error::Ring(_)), "{error:?}");
    assert_failed(&mut device);
}

#[test]
fn a_corrupt_read_pointer_found_while_waiting_for_room_ends_the_device() {
    let (link, host, _) = pair_sized(PAGE);
    let mut device = Storvsc::new(link, 10_000).unwrap();
    ready(&mut device);
    host.borrow_mut().stalled = true;
    while device.flush(DISK).is_ok() {}
    assert!(matches!(device.flush(DISK), Err(Error::Full)));
    host.borrow().to_host.control()[1].store(PAGE as u32, Ordering::Relaxed);
    assert!(matches!(device.wait(), Err(Error::Ring(_))));
    assert_failed(&mut device);
}

/// Writes a packet's descriptor straight into the ring the guest reads, and moves the
/// host's write pointer past it: what a host that ignores the ring's rules can do.
fn scribble(host: &Host, descriptor: [u16; 4], after: &[u8]) {
    let ring = &host.to_guest;
    let at = ring.control()[0].load(Ordering::Relaxed) as usize;
    let mut bytes = Vec::new();
    for field in descriptor {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(after);
    bytes.resize(bytes.len().next_multiple_of(8) + 8, 0);
    ring.write_at(at, &bytes);
    let end = (at + bytes.len()) % ring.len();
    ring.control()[0].store(end as u32, Ordering::Relaxed);
}

#[test]
fn a_corrupt_packet_descriptor_ends_the_device() {
    // The descriptor's fields: type, where the payload starts and the packet's length in
    // 8-byte units, and flags.
    let cases: [([u16; 4], &[u8], &str); 6] = [
        ([0x1234, 2, 10, 0], &[0; 64], "an unknown packet type"),
        (
            [6, 2, 1, 0],
            &[0; 64],
            "a packet shorter than its descriptor",
        ),
        ([6, 1, 10, 0], &[0; 64], "a payload inside the descriptor"),
        (
            [6, 2, 400, 0],
            &[0; 64],
            "a packet longer than what was written",
        ),
        ([6, 2, 10, 0x8000], &[0; 64], "unknown flags"),
        ([9, 3, 11, 1], &[0; 72], "a page-range packet with no range"),
    ];
    for (descriptor, after, what) in cases {
        let (mut device, host, _) = device();
        scribble(&host.borrow(), descriptor, after);
        assert!(matches!(device.poll(), Err(Error::Ring(_))), "{what}");
        assert_failed(&mut device);
    }

    // A packet that says it is no bigger than its descriptor is not corrupt, only empty,
    // and too short to be anything.
    let (mut device, host, _) = device();
    scribble(&host.borrow(), [6, 2, 2, 0], &[]);
    assert!(matches!(device.poll(), Err(Error::Malformed(_))));
    assert!(matches!(device.poll(), Ok(None)));
}

#[test]
fn completions_come_in_the_hosts_order_and_events_carry_their_ids() {
    let (mut device, host, _) = device();
    host.borrow_mut().hold = true;
    let ids: Vec<u64> = (0..4).map(|_| device.flush(DISK).unwrap()).collect();
    host.borrow_mut().release();
    for id in ids {
        match device.poll() {
            Ok(Some(Event::Completed(completion))) => {
                assert_eq!((completion.id, completion.result), (id, Ok(0)))
            }
            other => panic!("{other:?}"),
        }
    }
}
