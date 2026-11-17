//! The initialisation: its four steps in order, the version step-down, the in-flight limit it
//! sets, and every way the host can spoil it.

use moruna_storvsc::{
    ChannelError, Error, Event, Malformed, NtStatus, Operation, PAGE, Storvsc, VERSIONS, Wait,
};
use storvsp_protocol::{VERSION_BLUE, VERSION_THRESHOLD, VERSION_WIN6, VERSION_WIN7, VERSION_WIN8};

use crate::{
    device,
    fake::{DISK, Host, Link, PACKET, Raw, pair, pair_sized},
    ready,
};

/// After a failed initialisation every call answers `Failed`.
fn assert_failed(device: &mut Storvsc<Link>) {
    assert!(matches!(device.poll(), Err(Error::Failed)));
    assert!(matches!(device.wait(None), Err(Error::Failed)));
    assert!(matches!(device.flush(DISK), Err(Error::Failed)));
    assert!(matches!(device.read_capacity(DISK, 1), Err(Error::Failed)));
    assert_eq!(device.in_flight(), 0);
}

#[test]
fn the_versions_offered_are_all_the_protocol_defines_newest_first() {
    let mut defined = [
        VERSION_WIN6,
        VERSION_WIN7,
        VERSION_WIN8,
        VERSION_BLUE,
        VERSION_THRESHOLD,
    ];
    defined.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(VERSIONS, defined);
    assert_eq!(VERSIONS[0], VERSION_THRESHOLD);
}

#[test]
fn initialises_in_order_with_a_version_step_down() {
    let (device, host) = device();
    let host = host.borrow();
    // The fake speaks what OpenVMM's storvsp speaks, which is not the newest: one step down.
    assert_eq!(host.offered, [VERSION_THRESHOLD, VERSION_BLUE]);
    assert_eq!(host.version, Some(VERSION_BLUE));
    assert_eq!(device.version(), Some(VERSION_BLUE));
    let operations: Vec<Operation> = host.seen.iter().map(|(operation, _)| *operation).collect();
    assert_eq!(
        operations,
        [
            Operation::BEGIN_INITIALIZATION,
            Operation::QUERY_PROTOCOL_VERSION,
            Operation::QUERY_PROTOCOL_VERSION,
            Operation::QUERY_PROPERTIES,
            Operation::END_INITIALIZATION,
        ]
    );
    // Every step is its own transaction, none of them 0.
    let ids: Vec<u64> = host.seen.iter().map(|(_, id)| *id).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5]);
    // The properties answer's largest transfer, the limit that follows, and no request in
    // flight.
    assert_eq!(device.max_transfer_bytes(), 0x40000);
    assert_eq!(device.limit(), 26);
    assert_eq!(device.in_flight(), 0);
    // It all happened as the guest signalled: it never had to wait.
    assert_eq!(host.signals, 5);
    assert_eq!(host.idles, 0);
}

#[test]
fn the_limit_is_what_the_ring_holds_of_the_largest_request() {
    // A request's packet is 104 bytes and 8 for each page it names; with no pages it is the
    // in-band one, 88 bytes.
    for (ring, max_transfer, limit) in [
        // 64 pages of 256 KiB: 616 bytes into 16 KiB.
        (4 * PAGE, 0x40000, 26),
        // 51 pages: 512 bytes, exactly 32 into 16 KiB.
        (4 * PAGE, 51 * PAGE as u32, 32),
        // A transfer that ends inside a page names that page too: 2 pages, 120 bytes.
        (4 * PAGE, 5000, 136),
        // No transfer at all: the in-band request.
        (4 * PAGE, 0, 186),
        (PAGE, 0, 46),
        // No more than 256, however small the requests and large the ring.
        (64 * PAGE, PAGE as u32, 256),
        (64 * PAGE, 0, 256),
        // At least 1: a request that names all the ring can.
        (PAGE, u32::MAX, 1),
    ] {
        let (link, host) = pair_sized(ring);
        host.borrow_mut().max_transfer_bytes = max_transfer;
        let mut device = Storvsc::new(link).unwrap();
        assert_eq!(device.limit(), 0, "before the properties answer");
        ready(&mut device);
        assert_eq!(
            device.limit(),
            limit,
            "a ring of {ring} bytes and transfers of {max_transfer}"
        );
    }
}

#[test]
fn the_newest_version_is_taken_when_the_host_speaks_it() {
    let (link, host) = pair();
    host.borrow_mut().accepts = VERSIONS.to_vec();
    let mut device = Storvsc::new(link).unwrap();
    assert_eq!(device.version(), None);
    assert_eq!(device.max_transfer_bytes(), 0);
    ready(&mut device);
    assert_eq!(host.borrow().offered, [VERSION_THRESHOLD]);
    assert_eq!(device.version(), Some(VERSION_THRESHOLD));
}

#[test]
fn steps_down_to_the_oldest_version() {
    let (link, host) = pair();
    host.borrow_mut().accepts = vec![VERSION_WIN6];
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    assert_eq!(host.borrow().offered, VERSIONS);
    assert_eq!(device.version(), Some(VERSION_WIN6));
    // And the device works at that version.
    let id = device.flush(DISK).unwrap();
    assert_eq!(crate::moved(&mut device, id), 0);
}

#[test]
fn a_host_with_no_version_in_common_is_refused() {
    let (link, host) = pair();
    host.borrow_mut().accepts = Vec::new();
    let mut device = Storvsc::new(link).unwrap();
    assert!(matches!(device.poll(), Err(Error::NoVersion)));
    assert_eq!(host.borrow().offered, VERSIONS);
    assert_eq!(device.version(), None);
    assert_failed(&mut device);
}

#[test]
fn a_refused_step_ends_the_initialisation() {
    for (operation, status) in [
        (
            Operation::BEGIN_INITIALIZATION,
            NtStatus::INVALID_DEVICE_STATE,
        ),
        (Operation::QUERY_PROTOCOL_VERSION, NtStatus::UNSUCCESSFUL),
        (Operation::QUERY_PROPERTIES, NtStatus::INVALID_PARAMETER),
        (Operation::END_INITIALIZATION, NtStatus::DEVICE_BUSY),
    ] {
        let (link, host) = pair();
        host.borrow_mut().refuse = Some((operation, status));
        let mut device = Storvsc::new(link).unwrap();
        match device.poll() {
            Err(Error::Refused {
                operation: refused,
                status: with,
            }) => assert_eq!((refused, with), (operation, status)),
            other => panic!("{other:?} for a refused {operation:?}"),
        }
        assert_failed(&mut device);
    }
}

#[test]
fn nothing_is_asked_of_the_device_before_it_is_ready() {
    let (link, host) = pair();
    host.borrow_mut().stalled = true;
    let mut device = Storvsc::new(link).unwrap();
    assert!(matches!(device.poll(), Ok(None)));
    assert!(matches!(device.flush(DISK), Err(Error::NotReady)));
    assert!(matches!(
        device.read_capacity(DISK, 1),
        Err(Error::NotReady)
    ));
    assert!(matches!(device.read_serial(DISK, 1), Err(Error::NotReady)));
    assert_eq!(host.borrow().seen.len(), 0);

    // The host has nothing to say while it is stalled: a wait with a deadline times out.
    assert_eq!(device.wait(Some(0)).unwrap(), Wait::TimedOut);
    assert_eq!(host.borrow().idles, 1);

    // Once it reads the first step, its answer wakes the guest: the incoming ring starts
    // with the host's signal masked, and the wait unmasked it. The rest follows.
    host.borrow_mut().stalled = false;
    assert_eq!(device.wait(None).unwrap(), Wait::Signalled);
    assert_eq!(host.borrow().idles, 2);
    assert!(matches!(device.poll(), Ok(Some(Event::Ready))));
    assert!(matches!(device.poll(), Ok(None)));
    let id = device.flush(DISK).unwrap();
    assert_eq!(crate::moved(&mut device, id), 0);
}

/// A device whose first step the host has not read, so that a test can answer in its place.
fn stalled() -> (Storvsc<Link>, std::rc::Rc<std::cell::RefCell<Host>>) {
    let (link, host) = pair();
    host.borrow_mut().stalled = true;
    let device = Storvsc::new(link).unwrap();
    (device, host)
}

#[test]
fn a_packet_that_is_no_completion_ends_the_initialisation() {
    for raw in [
        Raw::InBand,
        Raw::InBandWithCompletion,
        Raw::GpaDirect,
        Raw::TransferPages,
    ] {
        let (mut device, host) = stalled();
        let packet = Host::packet(Operation::ENUMERATE_BUS, NtStatus::SUCCESS, &[]);
        assert!(host.borrow_mut().inject(raw, 1, &packet));
        assert!(
            matches!(device.poll(), Err(Error::Malformed(_))),
            "{raw:?} during the initialisation"
        );
        assert_failed(&mut device);
    }
}

#[test]
fn a_completion_of_another_transaction_ends_the_initialisation() {
    let (mut device, host) = stalled();
    let packet = Host::packet(Operation::COMPLETE_IO, NtStatus::SUCCESS, &[]);
    assert!(host.borrow_mut().inject(Raw::Completion, 2, &packet));
    assert!(matches!(device.poll(), Err(Error::UnknownTransaction(2))));
    assert_failed(&mut device);
}

#[test]
fn a_completion_of_the_wrong_size_ends_the_initialisation() {
    // The ring pads a payload to 8 bytes, so every size here arrives as itself.
    for size in [0, 8, PACKET - 16, PACKET - 8, PACKET + 8, 4 * PACKET] {
        let (mut device, host) = stalled();
        let mut packet = Host::packet(Operation::COMPLETE_IO, NtStatus::SUCCESS, &[]);
        packet.resize(size, 0);
        assert!(host.borrow_mut().inject(Raw::Completion, 1, &packet));
        match device.poll() {
            Err(Error::Malformed(Malformed(what))) => assert!(what.contains("size"), "{what}"),
            other => panic!("{other:?} for a completion of {size} bytes"),
        }
        assert_failed(&mut device);
    }
    // One a byte short of a whole packet is padded back up to it by the ring, and taken.
    let (mut device, host) = stalled();
    let mut packet = Host::packet(Operation::COMPLETE_IO, NtStatus::SUCCESS, &[]);
    packet.truncate(PACKET - 1);
    assert!(host.borrow_mut().inject(Raw::Completion, 1, &packet));
    assert!(matches!(device.poll(), Ok(None)));
    assert_eq!(device.in_flight(), 0);
}

#[test]
fn a_completion_that_is_not_marked_as_one_ends_the_initialisation() {
    let (mut device, host) = stalled();
    let packet = Host::packet(Operation::BEGIN_INITIALIZATION, NtStatus::SUCCESS, &[]);
    assert!(host.borrow_mut().inject(Raw::Completion, 1, &packet));
    match device.poll() {
        Err(Error::Malformed(Malformed(what))) => assert!(what.contains("COMPLETE_IO"), "{what}"),
        other => panic!("{other:?}"),
    }
    assert_failed(&mut device);
}

#[test]
fn one_request_carries_no_more_than_its_packet_can_name() {
    // A host that sets no limit of its own: the ring to the host does. A request's packet
    // is 104 bytes and 8 for each page, and must be smaller than the ring less 8.
    let (link, host) = pair_sized(PAGE);
    host.borrow_mut().max_transfer_bytes = u32::MAX;
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    let pages = (PAGE - 8 - 104 - 1) / 8;
    assert_eq!(device.max_transfer_bytes() as usize, pages * PAGE);
    assert_eq!(device.limit(), 1);
}

#[test]
fn a_host_that_allows_no_transfer_gets_no_data_requests() {
    let (link, host) = pair();
    host.borrow_mut().max_transfer_bytes = 0;
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    assert_eq!(device.max_transfer_bytes(), 0);
    assert!(matches!(
        device.read_capacity(DISK, 1),
        Err(Error::TooLarge)
    ));
    assert!(matches!(device.read_serial(DISK, 1), Err(Error::TooLarge)));
    // A flush moves nothing, so it still goes.
    let id = device.flush(DISK).unwrap();
    assert_eq!(crate::moved(&mut device, id), 0);
}

#[test]
fn a_channel_that_cannot_signal_cannot_start() {
    let (mut link, _) = pair();
    link.host().rescind();
    assert!(matches!(
        Storvsc::new(link),
        Err(Error::Channel(ChannelError::Closed))
    ));
}

#[test]
fn a_ring_that_is_corrupt_from_the_start_is_refused() {
    use std::sync::atomic::Ordering;
    use vmbus_ring::RingMem;
    for corrupt_incoming in [true, false] {
        let (link, host) = pair();
        {
            let host = host.borrow();
            let ring = if corrupt_incoming {
                host.to_guest()
            } else {
                host.to_host()
            };
            // A ring pointer that is not a multiple of 8.
            ring.control()[0].store(3, Ordering::Relaxed);
        }
        assert!(matches!(Storvsc::new(link), Err(Error::Ring(_))));
    }
}
