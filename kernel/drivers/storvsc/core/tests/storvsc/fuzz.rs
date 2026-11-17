//! Fuzz-style runs with fixed seeds: random packets from the host while requests are in
//! flight, random packets during the initialisation, and random scribbles on the rings.
//! Whatever the host sends, the library answers with a value or an error, never a panic,
//! and its count of requests in flight stays true.

use std::{collections::BTreeSet, sync::atomic::Ordering};

use moruna_storvsc::{Disk, Error, Event, NtStatus, Operation, Storvsc};
use storvsp_protocol::ScsiRequest;
use vmbus_ring::RingMem;
use zerocopy::{FromZeros, IntoBytes};

use crate::{
    LIMIT, device, disk,
    fake::{DISK, Host, Link, PACKET, Raw, Rng, pair},
    moved, pattern,
};

const KINDS: [Raw; 5] = [
    Raw::Completion,
    Raw::InBand,
    Raw::InBandWithCompletion,
    Raw::GpaDirect,
    Raw::TransferPages,
];

/// A payload from the host: noise of any length, or something shaped like a storvsp
/// packet with noise where its fields are, or a completion that is nearly right.
fn payload(rng: &mut Rng) -> Vec<u8> {
    match rng.below(6) {
        // Noise of any length up to three packets.
        0 => {
            let n = rng.below(3 * PACKET as u64 + 1) as usize;
            rng.bytes(n)
        }
        // Noise of exactly a packet's length.
        1 => rng.bytes(PACKET),
        // A real header with any operation, then noise.
        2 => {
            let body = rng.bytes(52);
            Host::packet(Operation(rng.below(16) as u32), NtStatus(0), &body)
        }
        // A completion's header with any status, then noise.
        3 => {
            let status = if rng.below(2) == 0 {
                NtStatus::SUCCESS
            } else {
                NtStatus(rng.next() as u32)
            };
            let body = rng.bytes(52);
            Host::packet(Operation::COMPLETE_IO, status, &body)
        }
        // A good completion that moved a random number of bytes.
        4 => {
            let mut answer = ScsiRequest::new_zeroed();
            answer
                .srb_status
                .set_status(moruna_storvsc::SrbStatus::SUCCESS);
            answer.data_transfer_length = match rng.below(4) {
                0 => 0,
                1 => 512,
                2 => rng.below(8192) as u32,
                _ => rng.next() as u32,
            };
            Host::packet(Operation::COMPLETE_IO, NtStatus::SUCCESS, answer.as_bytes())
        }
        // The bus-changed notice, sometimes cut short.
        _ => {
            let mut notice = Host::packet(Operation::ENUMERATE_BUS, NtStatus::SUCCESS, &[]);
            notice.truncate(rng.below(PACKET as u64 + 1) as usize);
            notice
        }
    }
}

/// What the library made of everything in the ring, counted by kind.
#[derive(Default, Debug)]
struct Seen {
    done: usize,
    failed: usize,
    unknown: usize,
    malformed: usize,
    unexpected: usize,
}

/// Reads the ring dry. Every completion must be of a request in flight, and every
/// unknown transaction must not be.
fn drain(device: &mut Storvsc<Link>, in_flight: &mut BTreeSet<u64>, seen: &mut Seen) {
    loop {
        match device.poll() {
            Ok(None) => break,
            Ok(Some(Event::Completed(completion))) => {
                assert!(
                    in_flight.remove(&completion.id),
                    "{} completed and was not in flight",
                    completion.id
                );
                match completion.result {
                    Ok(_) => seen.done += 1,
                    Err(_) => seen.failed += 1,
                }
            }
            Ok(Some(Event::Ready)) => panic!("ready twice"),
            Err(Error::UnknownTransaction(id)) => {
                assert!(!in_flight.contains(&id), "{id} is in flight");
                seen.unknown += 1;
            }
            Err(Error::Malformed(_)) => seen.malformed += 1,
            Err(Error::Unexpected(_)) => seen.unexpected += 1,
            Err(e) => panic!("{e}"),
        }
        assert_eq!(device.in_flight(), in_flight.len());
    }
}

/// One real request of a random kind, when there is room for it.
fn submit(device: &mut Storvsc<Link>, disk: &Disk, rng: &mut Rng, in_flight: &mut BTreeSet<u64>) {
    let page = 100 + rng.below(100);
    let block = rng.below(disk.blocks() - 8);
    let count = 1 + rng.below(8) as u32;
    let result = match rng.below(5) {
        0 => device.flush(DISK),
        1 => device.read_capacity(DISK, page),
        2 => device.read_serial(DISK, page),
        3 => device.read(disk, block, count, &[page]),
        _ => device.write(disk, block, count, &[page]),
    };
    match result {
        Ok(id) => assert!(in_flight.insert(id), "{id} given out twice"),
        Err(Error::Full) => {}
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn ten_thousand_random_host_packets_with_requests_in_flight() {
    let mut rng = Rng::new(0x0001_010a_5eed);
    let (mut device, host, _) = device();
    let disk = disk(&mut device, &host, 0);
    let mut in_flight = BTreeSet::new();
    let mut seen = Seen::default();
    let mut sent = 0;

    // The fake's own answers are kept back, so that the noise can reach requests in
    // flight first, and let go now and then.
    host.borrow_mut().hold = true;
    while sent < 10_000 {
        if in_flight.len() < LIMIT / 2 || rng.below(4) == 0 {
            submit(&mut device, &disk, &mut rng, &mut in_flight);
        }
        if rng.below(50) == 0 {
            host.borrow_mut().release();
            host.borrow_mut().hold = true;
        }

        let kind = KINDS[rng.below(KINDS.len() as u64) as usize];
        let id = match rng.below(4) {
            // A transaction in flight, when there is one.
            0 | 1 => {
                let nth = rng.below(in_flight.len().max(1) as u64) as usize;
                in_flight.iter().nth(nth).copied().unwrap_or(0)
            }
            // One near those given out so far.
            2 => rng.below(sent as u64 + 64),
            _ => rng.next(),
        };
        let bytes = payload(&mut rng);
        if host.borrow_mut().inject(kind, id, &bytes) {
            sent += 1;
        }
        // Sometimes the packets pile up before the guest looks.
        if rng.below(3) != 0 {
            drain(&mut device, &mut in_flight, &mut seen);
        }
        assert!(device.in_flight() <= LIMIT);
    }

    // Every kind of answer was reached, so the run tested what it claims to.
    drain(&mut device, &mut in_flight, &mut seen);
    assert!(seen.done > 100, "{seen:?}");
    assert!(seen.failed > 100, "{seen:?}");
    assert!(seen.unknown > 100, "{seen:?}");
    assert!(seen.malformed > 100, "{seen:?}");
    assert!(seen.unexpected > 100, "{seen:?}");

    // The fake's real answers to requests the noise already completed are late copies.
    host.borrow_mut().release();
    drain(&mut device, &mut in_flight, &mut seen);
    assert_eq!(in_flight.len(), 0, "requests the host never answered");
    assert_eq!(device.in_flight(), 0);
    assert_eq!(host.borrow().held(), 0);

    // After all of it the device still moves data.
    let data = pattern(9, 4096);
    host.borrow_mut().memory.write(&[50], &data).unwrap();
    let id = device.write(&disk, 64, 8, &[50]).unwrap();
    assert_eq!(moved(&mut device, id), 4096);
    let id = device.read(&disk, 64, 8, &[51]).unwrap();
    assert_eq!(moved(&mut device, id), 4096);
    assert_eq!(host.borrow().memory.read(&[51], 4096).unwrap(), data);
}

#[test]
fn random_host_packets_during_the_initialisation() {
    let mut rng = Rng::new(0x0002_010a_5eed);
    let (mut ready, mut failed) = (0, 0);
    for _ in 0..2_000 {
        let (link, host, _) = pair();
        // The fake reads nothing, so every answer the guest gets is noise.
        host.borrow_mut().stalled = true;
        let mut device = Storvsc::new(link, LIMIT).unwrap();
        for _ in 0..1 + rng.below(3) {
            let kind = KINDS[rng.below(KINDS.len() as u64) as usize];
            // The first step's transaction is 1; aim at it half the time.
            let id = if rng.below(2) == 0 { 1 } else { rng.below(8) };
            let bytes = payload(&mut rng);
            assert!(host.borrow_mut().inject(kind, id, &bytes));
        }
        loop {
            match device.poll() {
                Ok(None) => break,
                Ok(Some(Event::Ready)) => ready += 1,
                Ok(Some(event)) => panic!("{event:?} before the device was ready"),
                Err(Error::Failed) => {
                    failed += 1;
                    break;
                }
                Err(_) => {}
            }
        }
        // Whatever came of it, the device answers and does not panic.
        let _ = device.flush(DISK);
        let _ = device.read_capacity(DISK, 1);
        let _ = device.wait();
        assert_eq!(device.in_flight(), 0);
    }
    // Noise that is the right answer to the first step moves the guest to the second, and
    // no further; nearly all of it ends the initialisation.
    assert_eq!(ready, 0);
    assert!(failed > 1_000, "{failed}");
}

#[test]
fn random_scribbles_on_the_rings() {
    let mut rng = Rng::new(0x0003_010a_5eed);
    let mut failed = 0;
    for _ in 0..500 {
        let (mut device, host, _) = device();
        let disk = disk(&mut device, &host, 0);
        // The fake reads and writes nothing more: from here the rings are the test's.
        host.borrow_mut().stalled = true;
        for _ in 0..40 {
            {
                let host = host.borrow();
                let ring = if rng.below(2) == 0 {
                    &host.to_guest
                } else {
                    &host.to_host
                };
                match rng.below(3) {
                    // A control word: the pointers, the mask, the pending size, the
                    // feature bits.
                    0 => {
                        let word = [0, 1, 2, 3, 16][rng.below(5) as usize];
                        let value = match rng.below(3) {
                            0 => rng.next() as u32,
                            1 => (rng.below(ring.len() as u64 / 8) * 8) as u32,
                            _ => rng.below(64) as u32,
                        };
                        ring.control()[word].store(value, Ordering::Relaxed);
                    }
                    // Noise in the ring's data.
                    1 => {
                        let at = rng.below(ring.len() as u64 / 8) as usize * 8;
                        let n = rng.below(128) as usize;
                        ring.write_at(at, &rng.bytes(n));
                    }
                    // A plausible write pointer over whatever is there.
                    _ => {
                        let at = (rng.below(ring.len() as u64 / 8) * 8) as u32;
                        ring.control()[0].store(at, Ordering::Relaxed);
                    }
                }
            }
            let page = 1 + rng.below(200);
            let outcome = match rng.below(6) {
                0 => device.poll().map(drop),
                1 => device.wait(),
                2 => device.flush(DISK).map(drop),
                3 => device.read(&disk, rng.below(2000), 1, &[page]).map(drop),
                4 => device.write(&disk, rng.below(2000), 8, &[page]).map(drop),
                _ => device.read_serial(DISK, page).map(drop),
            };
            if matches!(outcome, Err(Error::Failed)) {
                failed += 1;
                break;
            }
            assert!(device.in_flight() <= LIMIT);
        }
    }
    // A scribbled ring is found out, nearly every time.
    assert!(failed > 250, "{failed}");
}
