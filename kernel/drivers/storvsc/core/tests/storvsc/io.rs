//! The operations a block driver needs: capacity and block size, reads and writes of a block
//! range by guest page numbers, flush, the serial, a target's LUNs, and requests in flight
//! together.

use std::collections::BTreeMap;

use moruna_storvsc::{
    AdditionalSenseCode, Address, ChannelError, Disk, Error, Event, Failure, Malformed, Operation,
    PAGE, SenseKey, SrbStatus, Storvsc, Wait, luns, scsi::SERIAL_BYTES, serial,
};

use crate::{
    device, disk,
    fake::{DISK, FakeDisk, Rng, pair, pair_sized},
    moved, next, pages_for, pattern, ready,
};

#[test]
fn capacity_and_block_size() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 9);
    assert_eq!(disk.address(), DISK);
    assert_eq!(disk.blocks(), 2048);
    assert_eq!(disk.block_size(), 512);

    // A second disk on the same controller, with 4096-byte blocks.
    let other = Address {
        path: 0,
        target: 1,
        lun: 0,
    };
    host.borrow_mut()
        .disks
        .insert((0, 1, 0), FakeDisk::new(4096, 100, b"other"));
    let id = device.read_capacity(other, 10).unwrap();
    let n = moved(&mut device, id);
    assert_eq!(n, Disk::CAPACITY_BYTES);
    let answer = host.borrow().memory.read(&[10], n as usize).unwrap();
    let other = Disk::from_capacity(other, &answer).unwrap();
    assert_eq!((other.blocks(), other.block_size()), (100, 4096));
}

#[test]
fn written_blocks_read_back_at_the_first_the_last_and_a_middle_block() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 0);
    let size = disk.block_size() as usize;
    let last = disk.blocks() - 1;
    let middle = disk.blocks() / 2;

    // One block at each end, and a run in the middle that is longer than a page and does
    // not end on one. The pages are not in order and not next to each other.
    let cases: [(u64, u32, &[u64], &[u64]); 3] = [
        (0, 1, &[20], &[40]),
        (last, 1, &[21], &[41]),
        (middle, 19, &[30, 22, 27], &[45, 44, 50]),
    ];
    for (n, (block, count, from, into)) in cases.into_iter().enumerate() {
        let bytes = count as usize * size;
        assert_eq!(from.len(), pages_for(bytes));
        let data = pattern(n as u64 + 1, bytes);
        host.borrow_mut().memory.write(from, &data).unwrap();
        let id = device.write(&disk, block, count, from).unwrap();
        assert_eq!(device.in_flight(), 1);
        assert_eq!(moved(&mut device, id) as usize, bytes);
        assert_eq!(device.in_flight(), 0);

        // The bytes are on the disk where the block says, and nowhere else.
        {
            let host = host.borrow();
            let on_disk = &host.disks[&(DISK.path, DISK.target, DISK.lun)].bytes;
            let at = block as usize * size;
            assert_eq!(&on_disk[at..at + bytes], &data[..]);
            if block > 0 {
                assert_eq!(on_disk[at - 1], 0, "the byte before block {block}");
            }
            if at + bytes < on_disk.len() {
                assert_eq!(on_disk[at + bytes], 0, "the byte after the run at {block}");
            }
        }

        // And they come back into other pages.
        let id = device.read(&disk, block, count, into).unwrap();
        assert_eq!(moved(&mut device, id) as usize, bytes);
        assert_eq!(host.borrow().memory.read(into, bytes).unwrap(), data);
    }

    // Each was one SCSI request of its own, after the capacity's.
    let host = host.borrow();
    let requests = host
        .seen
        .iter()
        .filter(|(operation, _)| *operation == Operation::EXECUTE_SRB)
        .count();
    assert_eq!(requests, 1 + 2 * 3);
}

#[test]
fn a_range_past_the_end_is_refused() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 0);
    let sent = host.borrow().seen.len();
    let pages = [1, 2, 3, 4];
    let blocks = disk.blocks();
    for (block, count) in [
        (blocks, 1),
        (blocks - 1, 2),
        (blocks - 7, 8),
        (blocks + 1000, 1),
        (u64::MAX, 1),
        (u64::MAX - 1, 4),
    ] {
        assert!(
            matches!(
                device.read(&disk, block, count, &pages),
                Err(Error::OutOfRange)
            ),
            "a read of {count} at {block}"
        );
        assert!(
            matches!(
                device.write(&disk, block, count, &pages),
                Err(Error::OutOfRange)
            ),
            "a write of {count} at {block}"
        );
    }
    // A request for no blocks is refused too, wherever it points.
    assert!(matches!(
        device.read(&disk, 0, 0, &pages),
        Err(Error::Empty)
    ));
    assert!(matches!(
        device.write(&disk, blocks, 0, &pages),
        Err(Error::Empty)
    ));
    // None of them reached the host, and none is in flight.
    assert_eq!(host.borrow().seen.len(), sent);
    assert_eq!(device.in_flight(), 0);
    // The last block itself is fine.
    let id = device.read(&disk, blocks - 1, 1, &pages).unwrap();
    assert_eq!(moved(&mut device, id), 512);
}

#[test]
fn flush() {
    let (mut device, host) = device();
    let flushes = |host: &std::cell::RefCell<crate::fake::Host>| {
        host.borrow().disks[&(DISK.path, DISK.target, DISK.lun)].flushes
    };
    assert_eq!(flushes(&host), 0);
    let id = device.flush(DISK).unwrap();
    assert_eq!(moved(&mut device, id), 0);
    assert_eq!(flushes(&host), 1);
    let id = device.flush(DISK).unwrap();
    assert_eq!(moved(&mut device, id), 0);
    assert_eq!(flushes(&host), 2);
}

#[test]
fn a_transfer_starts_at_a_byte_offset_in_its_first_page() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 0);
    let data = pattern(7, 3 * 512);
    // Written from byte 1000 of page 10, so across pages 10 and 11.
    host.borrow_mut()
        .memory
        .write_at(&[10, 11], 1000, &data)
        .unwrap();
    let id = device.write_offset(&disk, 4, 3, 1000, &[10, 11]).unwrap();
    assert_eq!(moved(&mut device, id), 1536);
    // Read back into byte 3000 of page 20, across pages 20 and 21; nothing before it moves.
    let id = device
        .read_offset(&disk, 4, 3, 3000, &[20, 21, 22])
        .unwrap();
    assert_eq!(moved(&mut device, id), 1536);
    let memory = &host.borrow().memory;
    assert_eq!(memory.read_from(&[20, 21], 3000, 1536).unwrap(), data);
    assert_eq!(memory.read(&[20], 3000).unwrap(), vec![0; 3000]);
    assert_eq!(memory.read(&[22], PAGE).unwrap(), vec![0; PAGE]);
}

#[test]
fn an_offset_is_within_the_first_page_and_its_pages_cover_the_transfer() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 0);
    assert!(matches!(
        device.read_offset(&disk, 0, 1, PAGE, &[1, 2]),
        Err(Error::Offset)
    ));
    // 512 bytes from byte 3700 reach into a second page.
    assert!(matches!(
        device.write_offset(&disk, 0, 1, 3700, &[1]),
        Err(Error::ShortBuffer)
    ));
    assert_eq!(device.in_flight(), 0);

    // A ring of one page names fewer pages than the host would move: the largest transfer
    // is the ring's, and an offset would make it name one page more.
    let (link, host) = pair_sized(PAGE);
    host.borrow_mut().max_transfer_bytes = 0x40_0000;
    host.borrow_mut().disks.insert(
        (DISK.path, DISK.target, DISK.lun),
        FakeDisk::new(512, 16384, b""),
    );
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    let disk = crate::disk(&mut device, &host, 0);
    let count = device.max_transfer_bytes() / disk.block_size();
    let pages: Vec<u64> = (0..=count as u64).collect();
    assert!(matches!(
        device.read_offset(&disk, 0, count, 512, &pages),
        Err(Error::TooLarge)
    ));
    assert_eq!(device.in_flight(), 0);
}

#[test]
fn report_luns_lists_the_disks_of_a_target() {
    let (mut device, host) = device();
    {
        let mut host = host.borrow_mut();
        host.disks
            .insert((0, 0, 0), FakeDisk::new(512, 64, b"root"));
        host.disks.insert((0, 1, 5), FakeDisk::new(512, 64, b""));
    }
    let listed = |device: &mut Storvsc<_>, target: u8| {
        let id = device
            .report_luns(
                Address {
                    path: 0,
                    target,
                    lun: 9,
                },
                4,
            )
            .unwrap();
        let completion = next(device);
        assert_eq!(completion.id, id);
        completion.result.map(|n| {
            let answer = host.borrow().memory.read(&[4], n as usize).unwrap();
            luns(&answer).unwrap()
        })
    };
    // Sent to LUN 0 of the target whatever the address says, which the fake checks.
    assert_eq!(listed(&mut device, 0), Ok(vec![0, DISK.lun]));
    assert_eq!(listed(&mut device, 1), Ok(vec![5]));
    // A target with no disk is not there.
    let Err(Failure::Scsi(none)) = listed(&mut device, 2) else {
        panic!("a target with no disk answered");
    };
    assert_eq!(none.srb, SrbStatus::INVALID_TARGET_ID);
    // A device that does not know the command refuses it.
    host.borrow_mut().refuse_report_luns = true;
    let Err(Failure::Scsi(refused)) = listed(&mut device, 0) else {
        panic!("a refused REPORT LUNS answered");
    };
    assert_eq!(
        refused.sense.map(|s| (s.key, s.code)),
        Some((
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_COMMAND
        ))
    );
}

#[test]
fn a_lun_list_is_checked_and_holds_what_a_request_can_name() {
    let list = |entries: &[[u8; 8]]| {
        let mut bytes = ((entries.len() * 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0; 4]);
        for entry in entries {
            bytes.extend_from_slice(entry);
        }
        bytes
    };
    assert_eq!(luns(&list(&[])).unwrap(), Vec::<u8>::new());
    let named = luns(&list(&[
        [0, 3, 0, 0, 0, 0, 0, 0],
        // Flat addressing.
        [0x40, 7, 0, 0, 0, 0, 0, 0],
        // Again: once.
        [0, 3, 0, 0, 0, 0, 0, 0],
        // Flat, above 255.
        [0x41, 0, 0, 0, 0, 0, 0, 0],
        // On bus 1.
        [0x01, 2, 0, 0, 0, 0, 0, 0],
        // Two levels.
        [0, 1, 0, 2, 0, 0, 0, 0],
        // Logical unit addressing.
        [0x80, 1, 0, 0, 0, 0, 0, 0],
    ]))
    .unwrap();
    assert_eq!(named, [3, 7]);
    // A list cut short at the bytes asked for: what arrived is used.
    let mut short = list(&[[0, 1, 0, 0, 0, 0, 0, 0], [0, 2, 0, 0, 0, 0, 0, 0]]);
    short[3] = 64;
    assert_eq!(luns(&short).unwrap(), [1, 2]);
    assert_eq!(
        luns(&[0; 7]),
        Err(Malformed("a LUN list shorter than its header"))
    );
    let mut ragged = list(&[[0, 1, 0, 0, 0, 0, 0, 0]]);
    ragged[3] = 9;
    assert_eq!(
        luns(&ragged),
        Err(Malformed("a LUN list whose length is not whole entries"))
    );
}

#[test]
fn the_serial() {
    let (mut device, host) = device();
    let id = device.read_serial(DISK, 12).unwrap();
    let n = moved(&mut device, id) as usize;
    // The page's four-byte header and the serial: less than was asked for.
    assert_eq!(n, 4 + 16);
    assert!(n < SERIAL_BYTES as usize);
    let answer = host.borrow().memory.read(&[12], n).unwrap();
    assert_eq!(serial(&answer).unwrap(), b"6002248042d1c9a3");

    // The device's padding is the device's: nothing is trimmed.
    host.borrow_mut()
        .disks
        .get_mut(&(DISK.path, DISK.target, DISK.lun))
        .unwrap()
        .serial = b"  lun-3 ".to_vec();
    let id = device.read_serial(DISK, 13).unwrap();
    let n = moved(&mut device, id) as usize;
    let answer = host.borrow().memory.read(&[13], n).unwrap();
    assert_eq!(serial(&answer).unwrap(), b"  lun-3 ");

    // The longest serial the request can bring back, and one byte more, which arrives cut.
    for (len, whole) in [(251, true), (252, false)] {
        host.borrow_mut()
            .disks
            .get_mut(&(DISK.path, DISK.target, DISK.lun))
            .unwrap()
            .serial = vec![b'7'; len];
        let id = device.read_serial(DISK, 14).unwrap();
        let n = moved(&mut device, id) as usize;
        assert_eq!(n, 255);
        let answer = host.borrow().memory.read(&[14], n).unwrap();
        match serial(&answer) {
            Ok(bytes) => assert!(whole && bytes == vec![b'7'; len]),
            Err(Malformed(what)) => assert!(!whole && what.contains("longer"), "{what}"),
        }
    }
}

#[test]
fn thirty_two_requests_in_flight_complete_out_of_order() {
    // Transfers of up to 51 pages make the largest request 512 bytes, and the ring of 16 KiB
    // holds 32 of those: the limit is 32.
    let (link, host) = pair();
    host.borrow_mut().max_transfer_bytes = 51 * PAGE as u32;
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    let limit = device.limit();
    assert_eq!(limit, 32);
    let disk = disk(&mut device, &host, 0);
    let size = disk.block_size() as usize;
    let mut rng = Rng::new(0x1_10a);

    // Thirty-two writes, each of two blocks from its own page, none answered yet.
    host.borrow_mut().hold = true;
    let mut writes = BTreeMap::new();
    for n in 0..limit as u64 {
        let data = pattern(100 + n, 2 * size);
        host.borrow_mut().memory.write(&[60 + n], &data).unwrap();
        let block = 10 * n + 3;
        let id = device.write(&disk, block, 2, &[60 + n]).unwrap();
        assert!(writes.insert(id, (block, data)).is_none(), "ID {id} twice");
    }
    assert_eq!(device.in_flight(), limit);
    device.commit().unwrap();
    assert_eq!(host.borrow().held(), limit);
    assert!(matches!(device.poll(), Ok(None)));
    // The thirty-third has no room, and the host never sees it.
    assert!(matches!(device.flush(DISK), Err(Error::Full)));
    assert!(matches!(device.read(&disk, 0, 1, &[1]), Err(Error::Full)));
    assert_eq!(device.in_flight(), limit);

    host.borrow_mut().shuffle(&mut rng);
    host.borrow_mut().release();
    let mut order = Vec::new();
    while let Some(event) = device.poll().unwrap() {
        let Event::Completed(completion) = event else {
            panic!("{event:?}")
        };
        assert_eq!(completion.result, Ok(2 * size as u32));
        assert!(writes.contains_key(&completion.id));
        assert!(!order.contains(&completion.id), "{} twice", completion.id);
        order.push(completion.id);
    }
    assert_eq!(order.len(), limit);
    assert_eq!(device.in_flight(), 0);
    let submitted: Vec<u64> = writes.keys().copied().collect();
    assert_ne!(order, submitted, "the completions came in the order sent");

    // Thirty-two reads of the same blocks, again answered in another order: each brings
    // back what its own write put there.
    host.borrow_mut().hold = true;
    let mut reads = BTreeMap::new();
    for (n, (block, data)) in writes.values().enumerate() {
        let page = 120 + n as u64;
        let id = device.read(&disk, *block, 2, &[page]).unwrap();
        assert!(!writes.contains_key(&id), "ID {id} used again");
        reads.insert(id, (page, data.clone()));
    }
    assert_eq!(device.in_flight(), limit);
    host.borrow_mut().shuffle(&mut rng);
    host.borrow_mut().release();
    let mut seen = 0;
    while let Some(event) = device.poll().unwrap() {
        let Event::Completed(completion) = event else {
            panic!("{event:?}")
        };
        assert_eq!(completion.result, Ok(2 * size as u32));
        let (page, data) = reads.remove(&completion.id).expect("a read in flight");
        assert_eq!(host.borrow().memory.read(&[page], 2 * size).unwrap(), data);
        seen += 1;
    }
    assert_eq!(seen, limit);
    assert_eq!(device.in_flight(), 0);
}

#[test]
fn the_pages_must_cover_the_transfer() {
    let (mut device, host) = device();
    let disk = disk(&mut device, &host, 0);
    // Nine blocks of 512 bytes are a page and a half.
    assert!(matches!(
        device.read(&disk, 0, 9, &[5]),
        Err(Error::ShortBuffer)
    ));
    assert!(matches!(
        device.write(&disk, 0, 9, &[]),
        Err(Error::ShortBuffer)
    ));
    assert_eq!(device.in_flight(), 0);
    // More pages than the transfer needs are fine: the first ones are used.
    let data = pattern(7, 9 * 512);
    host.borrow_mut().memory.write(&[5, 6], &data).unwrap();
    let id = device.write(&disk, 0, 9, &[5, 6, 7, 8]).unwrap();
    assert_eq!(moved(&mut device, id), 9 * 512);
    let id = device.read(&disk, 0, 9, &[15, 16, 17]).unwrap();
    assert_eq!(moved(&mut device, id), 9 * 512);
    assert_eq!(host.borrow().memory.read(&[15, 16], 9 * 512).unwrap(), data);
}

#[test]
fn a_transfer_larger_than_the_channel_allows_is_refused() {
    let (link, host) = pair();
    host.borrow_mut().max_transfer_bytes = 8 * 512;
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    let disk = disk(&mut device, &host, 0);
    let pages = [1, 2, 3];
    assert!(matches!(
        device.read(&disk, 0, 9, &pages),
        Err(Error::TooLarge)
    ));
    assert!(matches!(
        device.write(&disk, 0, 9, &pages),
        Err(Error::TooLarge)
    ));
    let id = device.read(&disk, 0, 8, &pages).unwrap();
    assert_eq!(moved(&mut device, id), 8 * 512);

    // A count whose bytes do not fit the request's 32-bit length at all.
    let mut capacity = [0u8; 12];
    capacity[..8].copy_from_slice(&(u64::MAX - 1).to_be_bytes());
    capacity[8..].copy_from_slice(&4096u32.to_be_bytes());
    let vast = Disk::from_capacity(DISK, &capacity).unwrap();
    assert_eq!(vast.blocks(), u64::MAX);
    assert!(matches!(
        device.read(&vast, 0, 1 << 20, &pages),
        Err(Error::TooLarge)
    ));
}

#[test]
fn the_largest_transfer_the_ring_can_name_goes_through() {
    // One page of ring, a host with no limit: 497 pages a request.
    let (link, host) = pair_sized(PAGE);
    {
        let mut host = host.borrow_mut();
        host.max_transfer_bytes = u32::MAX;
        host.memory = crate::fake::Memory::new(1024);
        host.disks
            .insert((0, 0, 3), FakeDisk::new(4096, 600, b"large"));
    }
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    let disk = disk(&mut device, &host, 0);
    let count = device.max_transfer_bytes() / disk.block_size();
    assert_eq!(count, 497);
    let from: Vec<u64> = (1..=count as u64).collect();
    let into: Vec<u64> = (501..=500 + count as u64).rev().collect();
    let data = pattern(3, count as usize * PAGE);
    host.borrow_mut().memory.write(&from, &data).unwrap();
    let id = device.write(&disk, 100, count, &from).unwrap();
    assert_eq!(moved(&mut device, id), count * 4096);
    let id = device.read(&disk, 100, count, &into).unwrap();
    assert_eq!(moved(&mut device, id), count * 4096);
    assert_eq!(host.borrow().memory.read(&into, data.len()).unwrap(), data);
    // One block more is one page more than a packet can name.
    assert!(matches!(
        device.read(&disk, 0, count + 1, &into),
        Err(Error::TooLarge)
    ));
}

#[test]
fn a_full_ring_refuses_the_request_and_wakes_the_wait_when_it_has_room() {
    // A ring of one page and transfers of 51 pages: requests of 512 bytes, 8 in flight by
    // the rule, but the ring holds 7 of them, since 8 of its bytes are never used.
    let (link, host) = pair_sized(PAGE);
    host.borrow_mut().max_transfer_bytes = 51 * PAGE as u32;
    let mut device = Storvsc::new(link).unwrap();
    ready(&mut device);
    assert_eq!(device.limit(), 8);
    let disk = disk(&mut device, &host, 0);
    let count = device.max_transfer_bytes() / disk.block_size();
    let pages: Vec<u64> = (1..=51).collect();

    // The host stops reading. Writes go into the ring until one does not fit.
    host.borrow_mut().stalled = true;
    let mut sent = Vec::new();
    let refused = loop {
        match device.write(&disk, 0, count, &pages) {
            Ok(id) => sent.push(id),
            Err(e) => break e,
        }
    };
    assert!(matches!(refused, Error::Full));
    assert_eq!(sent.len(), 7);
    assert_eq!(device.in_flight(), 7);

    // Nothing wakes the wait while the host reads nothing.
    assert_eq!(device.wait(Some(0)).unwrap(), Wait::TimedOut);

    // The host reads again, and answers nothing yet: the signal that ends the wait is the
    // one the guest asked for, room in the ring.
    assert!(matches!(
        device.write(&disk, 0, count, &pages),
        Err(Error::Full)
    ));
    {
        let mut host = host.borrow_mut();
        host.stalled = false;
        host.hold = true;
    }
    assert_eq!(device.wait(None).unwrap(), Wait::Signalled);
    assert!(matches!(device.poll(), Ok(None)));
    sent.push(device.write(&disk, 0, count, &pages).unwrap());
    // Now the limit, not the ring, refuses the next one.
    assert_eq!(device.in_flight(), 8);
    assert!(matches!(device.flush(DISK), Err(Error::Full)));

    // With nothing refused for want of room, a wait has nothing to return for.
    assert_eq!(device.wait(Some(0)).unwrap(), Wait::TimedOut);

    // Every write is answered once the host lets its answers go, in the order sent. The
    // wait finds them without sleeping: with no deadline, a sleep here would never end.
    host.borrow_mut().release();
    let idles = host.borrow().idles;
    assert_eq!(device.wait(None).unwrap(), Wait::Signalled);
    assert_eq!(
        host.borrow().idles,
        idles,
        "a wait slept with packets to read"
    );
    for id in &sent {
        assert_eq!(moved(&mut device, *id), count * disk.block_size());
    }
    assert!(matches!(device.poll(), Ok(None)));
    assert_eq!(device.in_flight(), 0);
}

#[test]
fn a_wait_returns_at_once_when_a_packet_is_there() {
    let (mut device, host) = device();
    let id = device.flush(DISK).unwrap();
    // The answer is in the ring and the host did not signal: the guest masks the signal
    // between waits. A wait with no deadline that slept now would never end.
    let idles = host.borrow().idles;
    assert_eq!(device.wait(None).unwrap(), Wait::Signalled);
    assert_eq!(host.borrow().idles, idles);
    assert_eq!(next(&mut device).id, id);
    // With nothing in flight and nothing to read, the wait is the channel's.
    assert_eq!(device.wait(Some(0)).unwrap(), Wait::TimedOut);
    assert_eq!(host.borrow().idles, idles + 1);
}

#[test]
fn requests_made_back_to_back_go_to_the_host_with_one_signal() {
    let (mut device, host) = device();
    let (signals, seen) = (device.channel().host_signals(), host.borrow().seen.len());
    let ids: Vec<u64> = (0..8).map(|_| device.flush(DISK).unwrap()).collect();
    // Written, not committed: the host has seen none of them, and was not signalled.
    assert_eq!(device.channel().host_signals(), signals);
    assert_eq!(host.borrow().seen.len(), seen);
    device.commit().unwrap();
    assert_eq!(
        device.channel().host_signals(),
        signals + 1,
        "one signal for eight"
    );
    assert_eq!(host.borrow().seen.len(), seen + 8);
    for id in ids {
        assert_eq!(moved(&mut device, id), 0);
    }
    // Nothing left to commit: a commit signals nothing.
    device.commit().unwrap();
    assert_eq!(device.channel().host_signals(), signals + 1);
    // An arm commits what was written, with one signal.
    let id = device.flush(DISK).unwrap();
    assert!(device.arm().unwrap(), "the answer is in the ring");
    assert_eq!(device.channel().host_signals(), signals + 2);
    assert_eq!(moved(&mut device, id), 0);
}

#[test]
fn arm_leaves_the_hosts_signal_on_and_says_when_there_is_work() {
    let (mut device, host) = device();
    // Nothing to read and nothing refused: nothing to do, and nothing slept.
    let idles = host.borrow().idles;
    assert!(!device.arm().unwrap());
    assert_eq!(host.borrow().idles, idles);
    // With the signal on, the host signals as it answers.
    let id = device.flush(DISK).unwrap();
    assert!(device.arm().unwrap(), "the answer is in the ring");
    assert_eq!(next(&mut device).id, id);
    assert!(matches!(device.poll(), Ok(None)));
    assert!(!device.arm().unwrap());
    assert_eq!(
        device.wait(Some(0)).unwrap(),
        Wait::Signalled,
        "the host's signal, which the channel kept"
    );
    // A wait turns the signal off again: the next answer comes unsignalled.
    let id = device.flush(DISK).unwrap();
    assert_eq!(next(&mut device).id, id);
    assert_eq!(device.wait(Some(0)).unwrap(), Wait::TimedOut);
    // The channel the driver waits on is the device's.
    assert_eq!(device.channel().host_signals(), host.borrow().signals);
}

#[test]
fn a_closed_channel_is_an_error_to_the_caller() {
    let (mut device, host) = device();
    // The host takes the channel away; the guest learns of it at its next call on it.
    host.borrow_mut().rescind = true;
    assert!(matches!(
        device.wait(Some(0)),
        Err(Error::Channel(ChannelError::Closed))
    ));
    // The request is written to the ring, and the commit cannot tell the host.
    device.flush(DISK).unwrap();
    assert!(matches!(
        device.commit(),
        Err(Error::Channel(ChannelError::Closed))
    ));
    assert_eq!(device.in_flight(), 1);
    assert!(matches!(
        device.wait(None),
        Err(Error::Channel(ChannelError::Closed))
    ));
    // The device is not failed by it: the channel's owner decides what a closed channel
    // means.
    assert!(matches!(device.poll(), Ok(None)));
}

#[test]
fn transaction_ids_are_never_zero_and_never_used_twice() {
    let (mut device, host) = device();
    let mut ids = Vec::new();
    for _ in 0..200 {
        let id = device.flush(DISK).unwrap();
        assert_eq!(moved(&mut device, id), 0);
        ids.push(id);
    }
    let seen: Vec<u64> = host.borrow().seen.iter().map(|(_, id)| *id).collect();
    assert!(seen.iter().all(|id| *id != 0));
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(seen[seen.len() - ids.len()..], ids);
}

#[test]
fn a_capacity_answer_is_checked() {
    let capacity = |last: u64, size: u32| {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&last.to_be_bytes());
        bytes[8..12].copy_from_slice(&size.to_be_bytes());
        bytes
    };
    let ok = Disk::from_capacity(DISK, &capacity(99, 512)).unwrap();
    assert_eq!((ok.blocks(), ok.block_size()), (100, 512));
    // The two fields are enough; what follows them is not read.
    assert!(Disk::from_capacity(DISK, &capacity(0, 4096)[..12]).is_ok());
    assert_eq!(
        Disk::from_capacity(DISK, &capacity(0, 1 << 31))
            .unwrap()
            .block_size(),
        1 << 31
    );

    for short in [0, 1, 8, 11] {
        let Err(Malformed(what)) = Disk::from_capacity(DISK, &capacity(99, 512)[..short]) else {
            panic!("a capacity of {short} bytes was taken")
        };
        assert!(what.contains("shorter"), "{what}");
    }
    let Err(Malformed(what)) = Disk::from_capacity(DISK, &capacity(u64::MAX, 512)) else {
        panic!("a last block of 2^64 - 1 was taken")
    };
    assert!(what.contains("last block"), "{what}");
    for size in [0, 1, 256, 511, 513, 768, 4095, u32::MAX] {
        let Err(Malformed(what)) = Disk::from_capacity(DISK, &capacity(99, size)) else {
            panic!("a block size of {size} was taken")
        };
        assert!(what.contains("block size"), "{what}");
    }
}

#[test]
fn a_serial_answer_is_checked() {
    assert_eq!(serial(&[0, 0x80, 0, 3, b'a', b'b', b'c']).unwrap(), b"abc");
    // Bytes past the page's own length are not the serial's.
    assert_eq!(serial(&[0, 0x80, 0, 2, b'a', b'b', b'c']).unwrap(), b"ab");
    assert_eq!(serial(&[0, 0x80, 0, 0]).unwrap(), b"");

    for short in [&[][..], &[0], &[0, 0x80, 0]] {
        let Err(Malformed(what)) = serial(short) else {
            panic!("{short:?} was taken")
        };
        assert!(what.contains("header"), "{what}");
    }
    let Err(Malformed(what)) = serial(&[0, 0x83, 0, 1, b'a']) else {
        panic!("page 0x83 was taken")
    };
    assert!(what.contains("0x80"), "{what}");
    let Err(Malformed(what)) = serial(&[0, 0x80, 0, 4, b'a', b'b', b'c']) else {
        panic!("a cut serial was taken")
    };
    assert!(what.contains("longer"), "{what}");
}

#[test]
fn every_error_says_what_it_is() {
    let errors: Vec<(Error, &str)> = vec![
        (
            Error::Channel(ChannelError::Closed),
            "the channel failed: the channel is closed",
        ),
        (
            Error::Ring(vmbus_ring::Error::InvalidRingPointer),
            "a ring is corrupt: invalid ring buffer pointer",
        ),
        (
            Error::Malformed(Malformed("a completion of the wrong size")),
            "the host sent a completion of the wrong size",
        ),
        (
            Error::Unexpected(Operation::REMOVE_DEVICE),
            "the host sent REMOVE_DEVICE, which this driver does not serve",
        ),
        (
            Error::UnknownTransaction(7),
            "the host completed transaction 7, which is not in flight",
        ),
        (
            Error::Refused {
                operation: Operation::QUERY_PROPERTIES,
                status: moruna_storvsc::NtStatus::INVALID_DEVICE_STATE,
            },
            "the host refused QUERY_PROPERTIES with INVALID_DEVICE_STATE",
        ),
        (
            Error::NoVersion,
            "the host accepts no protocol version of this driver",
        ),
        (Error::NotReady, "the initialisation has not finished"),
        (Error::Failed, "the device failed earlier and is unusable"),
        (
            Error::Full,
            "no room for another request: wait for completions",
        ),
        (Error::Empty, "a request for no blocks"),
        (Error::OutOfRange, "the blocks reach past the disk's end"),
        (Error::TooLarge, "more bytes than one request may carry"),
        (
            Error::ShortBuffer,
            "the pages given do not cover the transfer",
        ),
        (
            Error::Offset,
            "a byte offset that is not within the first page",
        ),
    ];
    for (error, text) in errors {
        assert_eq!(error.to_string(), text);
        assert!(!format!("{error:?}").is_empty());
    }
}
