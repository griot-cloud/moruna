//! Serving the disks as `moruna.driver.block`.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::Ordering,
    thread::{self, JoinHandle},
};

use block_protocol::{Info, block};
use moruna_storvsc::{PAGE, ScsiStatus, SrbStatus};
use moruna_storvsc_server::{
    Driver, READ_TO, WRITE_FROM, factory,
    naming::{DATA, ROOT},
};
use moruna_svc::{
    CallError, Channel, ClientCore, Dispatcher, Encoder, Memory, Rights, Status, Wire, deadline_in,
    map,
};
use vmbus_protocol::Guid;
use vmbus_ring::RingMem;

use crate::{
    FakeNode, Page, controller, fake::Fault, fake::Host, fake::Link, gpt, plan_of, scanned,
};

const SECONDS: u64 = 1_000_000_000;

type Shared = Rc<RefCell<Driver<Link, FakeNode>>>;

/// The driver over one controller of `instance` with `disks`, and the dispatcher serving
/// its clients.
struct World {
    driver: Shared,
    host: Rc<RefCell<Host>>,
    dispatcher: Dispatcher,
}

impl World {
    fn new(instance: Guid, disks: &[(u8, u32, u64)]) -> World {
        let (link, host) = controller(disks);
        let (device, scan) = scanned(link, &host);
        let plan = plan_of(instance, &scan);
        let node = FakeNode::new(host.clone());
        World {
            driver: Rc::new(RefCell::new(Driver::new(node, vec![device], plan.served))),
            host,
            dispatcher: Dispatcher::new(),
        }
    }

    /// A client's connection to served disk `disk`.
    fn client(&mut self, disk: usize) -> Channel {
        let (mine, theirs) = Channel::create().unwrap();
        let handler = factory(self.driver.clone(), disk)();
        self.dispatcher.serve(theirs, handler).unwrap();
        mine
    }

    /// One turn of the process's loop: the dispatcher, then the driver, then its answers.
    fn turn(&mut self) {
        self.dispatcher.turn(deadline_in(5_000_000)).unwrap();
        let ready = {
            let mut driver = self.driver.borrow_mut();
            driver.service();
            driver.take_ready()
        };
        for (pending, outcome) in ready {
            let _ = self.dispatcher.complete(pending, outcome);
        }
    }

    /// Serves until `client` ends, doing `between` after each turn; then until its
    /// connections are gone.
    fn serve<T>(&mut self, client: JoinHandle<T>, mut between: impl FnMut(&World)) -> T {
        while !client.is_finished() {
            self.turn();
            between(self);
        }
        for _ in 0..4 {
            self.turn();
        }
        client.join().expect("the client's thread")
    }

    /// The bytes of the fake's disk at LUN `lun`, from byte `at`.
    fn disk(&self, lun: u8, at: usize, len: usize) -> Vec<u8> {
        self.host.borrow().disks[&(0, 0, lun)].bytes[at..at + len].to_vec()
    }
}

fn open(channel: Channel) -> block::Client {
    let mut client = block::Client::connect(channel, deadline_in(10 * SECONDS)).expect("hello");
    client.core().set_timeout(10 * SECONDS);
    client
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(7) ^ seed).collect()
}

/// Memory of `pages` pages holding `bytes` at its start.
fn filled(pages: u64, bytes: &[u8]) -> Memory {
    let memory = Memory::create(pages * PAGE as u64).unwrap();
    memory
        .map(0, pages * PAGE as u64, map::WRITE)
        .unwrap()
        .write(0, bytes)
        .unwrap();
    memory
}

fn bytes(memory: &Memory, at: usize, len: usize) -> Vec<u8> {
    let pages = (at + len).div_ceil(PAGE) as u64;
    let mut out = vec![0; len];
    memory
        .map(0, pages * PAGE as u64, 0)
        .unwrap()
        .read(at, &mut out)
        .unwrap();
    out
}

fn remote<T>(error: block::Error) -> Result<T, CallError<block::Error>> {
    Err(CallError::Remote(error))
}

#[test]
fn a_data_disk_says_its_size_and_reads_back_what_was_written() {
    // The root controller: the OS disk, read-only, and the resource disk, served 0 and 1.
    let mut world = World::new(ROOT, &[(0, 512, 64), (1, 512, 4096)]);
    let end = world.client(1);
    let data = pattern(0x5a, 3 * 512);
    let sent = data.clone();
    let (info, read) = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let info = disk.info().unwrap();
            let token = disk.acquire_write().unwrap();
            disk.write(token, 10, 3, filled(1, &sent)).unwrap();
            let back = disk
                .read(10, 3, Memory::create(PAGE as u64).unwrap())
                .unwrap();
            disk.flush(token).unwrap();
            disk.release_write(token).unwrap();
            (info, bytes(&back, 0, 3 * 512))
        }),
        |_| {},
    );
    // 256 KiB a transfer, less a page for a registration's offset.
    assert_eq!(
        info,
        Info {
            sector_size: 512,
            sectors: 4096,
            max_sectors: (0x40000 - PAGE as u32) / 512,
            read_only: false,
        }
    );
    assert_eq!(read, data);
    assert_eq!(world.disk(1, 10 * 512, 3 * 512), data);
    // The client's flush, and the release's final one.
    assert_eq!(world.host.borrow().disks[&(0, 0, 1)].flushes, 2);
    let driver = world.driver.borrow();
    assert_eq!(driver.served(), 4);
    assert_eq!(driver.busy(), 0);
    assert!(driver.node().releases > 0);
    let serials: Vec<_> = driver.disks().map(|d| d.name.serial.as_str()).collect();
    assert_eq!(serials, ["boot", "resource"]);
}

#[test]
fn the_os_disk_is_read_only() {
    let mut world = World::new(ROOT, &[(0, 512, 64)]);
    let end = world.client(0);
    let answers = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            (
                disk.info().unwrap().read_only,
                disk.acquire_write(),
                disk.write(0, 0, 1, filled(1, &[1; 512])).map(|_| ()),
                disk.read(0, 1, Memory::create(PAGE as u64).unwrap())
                    .map(|_| ()),
            )
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        (
            true,
            remote(block::Error::ReadOnly),
            remote(block::Error::NotWriter),
            Ok(())
        )
    );
}

#[test]
fn transfers_the_disk_cannot_make_are_refused() {
    let mut world = World::new(ROOT, &[(0, 512, 8), (1, 512, 64)]);
    let end = world.client(1);
    let answers = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let page = || Memory::create(PAGE as u64).unwrap();
            let max = disk.info().unwrap().max_sectors;
            let small = disk.read(0, 9, page()).map(|_| ());
            let lacking = Memory(
                page()
                    .0
                    .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                    .unwrap(),
            );
            let token = disk.acquire_write().unwrap();
            let answers = [
                disk.read(0, 0, page()).map(|_| ()),
                disk.read(0, max + 1, page()).map(|_| ()),
                disk.read(64, 1, page()).map(|_| ()),
                disk.read(u64::MAX, 1, page()).map(|_| ()),
                small,
                disk.read(0, 1, lacking).map(|_| ()),
                disk.write(token + 1, 0, 1, page()).map(|_| ()),
                disk.flush(token + 1),
                disk.acquire_write().map(|_| ()),
            ];
            disk.release_write(token).unwrap();
            answers
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        [
            remote(block::Error::BadCount),
            remote(block::Error::BadCount),
            remote(block::Error::OutOfRange),
            remote(block::Error::OutOfRange),
            remote(block::Error::BadBuffer),
            remote(block::Error::BadBuffer),
            remote(block::Error::NotWriter),
            remote(block::Error::NotWriter),
            remote(block::Error::LeaseInUse),
        ]
    );
}

/// A body for `read_to`, or `write_from` with `token`.
fn at_offset(token: Option<u64>, id: u32, offset: u64, sector: u64, count: u32) -> Encoder {
    let mut body = Encoder::new();
    if let Some(token) = token {
        token.encode(&mut body).unwrap();
    }
    (id, offset, sector, count).encode(&mut body).unwrap();
    body
}

#[test]
fn a_registered_buffer_moves_sectors_at_a_page_or_at_any_byte() {
    let mut world = World::new(ROOT, &[(0, 512, 8), (1, 512, 256)]);
    let end = world.client(1);
    let first = pattern(1, 1024);
    let second = pattern(2, 1536);
    let (sent_first, sent_second) = (first.clone(), second.clone());
    let (pages, read_page, read_byte) = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let token = disk.acquire_write().unwrap();
            let buffer = Memory::create(4 * PAGE as u64).unwrap();
            let view = Memory(
                buffer
                    .0
                    .duplicate(Rights::READ | Rights::WRITE | Rights::MAP)
                    .unwrap(),
            );
            let (id, pages) = disk.register(buffer).unwrap();
            // Two sectors from page 1, written at sector 4.
            view.map(0, 4 * PAGE as u64, map::WRITE)
                .unwrap()
                .write(PAGE, &sent_first)
                .unwrap();
            disk.write_at(token, id, 1, 4, 2).unwrap();
            // Three sectors from byte 5000, across pages 1 and 2, written at sector 20.
            view.map(0, 4 * PAGE as u64, map::WRITE)
                .unwrap()
                .write(5000, &sent_second)
                .unwrap();
            disk.core()
                .call(WRITE_FROM, at_offset(Some(token), id, 5000, 20, 3))
                .unwrap();
            // Read back: sector 4's two to page 3, sector 20's three to byte 1000.
            disk.read_at(id, 3, 4, 2).unwrap();
            disk.core()
                .call(READ_TO, at_offset(None, id, 1000, 20, 3))
                .unwrap();
            disk.unregister(id).unwrap();
            disk.release_write(token).unwrap();
            (
                pages,
                bytes(&view, 3 * PAGE, 1024),
                bytes(&view, 1000, 1536),
            )
        }),
        |_| {},
    );
    assert_eq!(pages, 4);
    assert_eq!(world.disk(1, 4 * 512, 1024), first);
    assert_eq!(world.disk(1, 20 * 512, 1536), second);
    assert_eq!(read_page, first);
    assert_eq!(read_byte, second);
}

#[test]
fn a_registration_refuses_what_it_cannot_hold() {
    let mut world = World::new(DATA[0], &[(0, 512, 64)]);
    let end = world.client(0);
    let answers = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let (id, _) = disk
                .register(Memory::create(2 * PAGE as u64).unwrap())
                .unwrap();
            let lacking = Memory(
                Memory::create(PAGE as u64)
                    .unwrap()
                    .0
                    .duplicate(Rights::READ | Rights::MAP | Rights::DUPLICATE | Rights::TRANSFER)
                    .unwrap(),
            );
            let mut answers = vec![
                disk.register(lacking).map(|_| ()),
                disk.read_at(id + 1, 0, 0, 1),
                disk.read_at(id, 2, 0, 1),
                disk.read_at(id, 1, 0, 9),
                disk.core()
                    .call(READ_TO, at_offset(None, id, 2 * PAGE as u64 - 100, 0, 1))
                    .map(|_| ())
                    .map_err(|e| e.typed()),
                disk.core()
                    .call(READ_TO, at_offset(None, id + 1, 0, 0, 1))
                    .map(|_| ())
                    .map_err(|e| e.typed()),
                disk.core()
                    .call(WRITE_FROM, at_offset(Some(0), id, 0, 0, 1))
                    .map(|_| ())
                    .map_err(|e| e.typed()),
                disk.write_at(0, id + 1, 0, 0, 1),
            ];
            disk.unregister(id).unwrap();
            answers.push(disk.unregister(id));
            for _ in 0..block_protocol::REGISTRATIONS_MAX {
                disk.register(Memory::create(PAGE as u64).unwrap()).unwrap();
            }
            answers.push(
                disk.register(Memory::create(PAGE as u64).unwrap())
                    .map(|_| ()),
            );
            answers
        }),
        |_| {},
    );
    assert_eq!(
        answers,
        [
            remote(block::Error::BadBuffer),
            remote(block::Error::NoRegistration),
            remote(block::Error::BadBuffer),
            remote(block::Error::BadBuffer),
            remote(block::Error::BadBuffer),
            remote(block::Error::NoRegistration),
            remote(block::Error::NotWriter),
            remote(block::Error::NoRegistration),
            remote(block::Error::NoRegistration),
            remote(block::Error::TooManyRegistrations),
        ]
    );
}

#[test]
fn requests_past_the_in_flight_limit_wait_their_turn() {
    let mut world = World::new(DATA[0], &[(0, 512, 64)]);
    world.host.borrow_mut().hold = true;
    let end = world.client(0);
    const READS: usize = 40;
    let mut released = false;
    let answered = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let mut sent = Vec::new();
            for i in 0..READS {
                let mut body = Encoder::new();
                (i as u64, 1u32, Memory::create(PAGE as u64).unwrap())
                    .encode(&mut body)
                    .unwrap();
                sent.push(disk.core().send(2, body).unwrap());
            }
            sent.into_iter()
                .filter(|id| disk.core().reply(*id, deadline_in(10 * SECONDS)).is_ok())
                .count()
        }),
        |world| {
            let busy = world.driver.borrow().busy();
            if busy == READS && !released {
                // Some wait for room: the limit is the ring's.
                assert!(world.host.borrow().held() < READS);
                world.host.borrow_mut().release();
                released = true;
            }
        },
    );
    assert!(released);
    assert_eq!(answered, READS);
    assert_eq!(world.driver.borrow().busy(), 0);
}

#[test]
fn a_transfer_the_disk_fails_is_an_io_error_and_said() {
    let mut world = World::new(DATA[0], &[(0, 512, 64)]);
    world.host.borrow_mut().faults.push_back(Fault::Scsi {
        srb: SrbStatus::ERROR,
        scsi: ScsiStatus::CHECK_CONDITION,
        sense: None,
    });
    let end = world.client(0);
    let answer = world.serve(
        thread::spawn(move || {
            open(end)
                .read(7, 1, Memory::create(PAGE as u64).unwrap())
                .map(|_| ())
        }),
        |_| {},
    );
    assert_eq!(answer, remote(block::Error::Io));
    assert!(
        world
            .driver
            .borrow()
            .node()
            .said("storvsc: lun-0: a transfer at sector 7 failed")
    );
}

#[test]
fn a_writer_that_goes_has_its_writes_flushed() {
    let mut world = World::new(ROOT, &[(0, 512, 8), (1, 512, 64)]);
    let end = world.client(1);
    let again = world.client(1);
    let answer = world.serve(
        thread::spawn(move || {
            {
                let mut disk = open(end);
                let token = disk.acquire_write().unwrap();
                disk.write(token, 1, 1, filled(1, &[9; 512])).unwrap();
            }
            // Gone with the lease: once the final flush is done, another may claim it.
            let mut other = open(again);
            let deadline = deadline_in(10 * SECONDS);
            loop {
                match other.acquire_write() {
                    Err(CallError::Remote(block::Error::LeaseInUse))
                        if moruna_svc::now() < deadline => {}
                    answer => return answer.map(|_| ()),
                }
            }
        }),
        |_| {},
    );
    assert_eq!(answer, Ok(()));
    assert_eq!(world.host.borrow().disks[&(0, 0, 1)].flushes, 1);
}

#[test]
fn a_controller_whose_ring_is_corrupt_ends_the_driver() {
    let world = World::new(DATA[0], &[(0, 512, 64)]);
    // The host's write pointer, past the ring.
    world.host.borrow().to_guest().control()[0].store(1, Ordering::Relaxed);
    let mut driver = world.driver.borrow_mut();
    driver.service();
    let failed = driver.failed().unwrap().to_string();
    assert!(
        failed.starts_with("controller 0: a ring is corrupt"),
        "{failed}"
    );
    // The first reason stays the reason, and nothing more is served.
    driver.service();
    assert_eq!(driver.failed(), Some(failed.as_str()));
}

#[test]
fn requests_that_are_no_requests_are_refused() {
    let mut world = World::new(DATA[0], &[(0, 512, 64)]);
    let end = world.client(0);
    let answers = world.serve(
        thread::spawn(move || {
            let mut core =
                ClientCore::connect(end, block::ID, deadline_in(10 * SECONDS)).expect("hello");
            let unknown = core.call(13, Encoder::new()).map(|_| ());
            let mut extra = Encoder::new();
            7u32.encode(&mut extra).unwrap();
            let info = core.call(1, extra).map(|_| ());
            let mut short = Encoder::new();
            0u64.encode(&mut short).unwrap();
            let read = core.call(2, short).map(|_| ());
            let claim = {
                let mut extra = Encoder::new();
                1u8.encode(&mut extra).unwrap();
                core.call(9, extra).map(|_| ())
            };
            let flush = core.call(4, Encoder::new()).map(|_| ());
            [unknown, info, read, claim, flush]
        }),
        |_| {},
    );
    let bad = || Err(CallError::Status(Status::BadMessage));
    assert_eq!(
        answers,
        [
            Err(CallError::Status(Status::UnknownMethod)),
            bad(),
            bad(),
            bad(),
            bad()
        ]
    );
}

#[test]
fn the_system_partition_is_a_read_only_window_of_the_boot_disk() {
    let (link, host) = controller(&[(0, 512, 4096)]);
    let data = pattern(0x33, 64 * 512);
    {
        let mut h = host.borrow_mut();
        let disk = &mut h.disks.get_mut(&(0, 0, 0)).unwrap().bytes;
        gpt(disk, &[("EFI system", 40, 99), ("moruna-system", 100, 163)]);
        disk[100 * 512..164 * 512].copy_from_slice(&data);
    }
    let page = Page(host.clone());
    let (mut device, scan) = scanned(link, &host);
    let window = moruna_storvsc_server::scan::system(&mut device, &page, &scan.disks[0])
        .unwrap()
        .unwrap();
    let mut served = plan_of(ROOT, &scan).served;
    let system = moruna_storvsc_server::system(&served[0], window);
    served.push(system.clone());
    let node = FakeNode::new(host.clone());
    let mut world = World {
        driver: Rc::new(RefCell::new(Driver::new(node, vec![device], served))),
        host,
        dispatcher: Dispatcher::new(),
    };
    let end = world.client(1);
    let one = || Memory::create(PAGE as u64).unwrap();
    let (info, first, last, answers) = world.serve(
        thread::spawn(move || {
            let mut disk = open(end);
            let first = disk.read(0, 2, one()).map(|m| bytes(&m, 0, 1024));
            let last = disk.read(63, 1, one()).map(|m| bytes(&m, 0, 512));
            (
                disk.info().unwrap(),
                first,
                last,
                (
                    disk.read(64, 1, one()).map(|_| ()),
                    disk.read(63, 2, one()).map(|_| ()),
                    disk.acquire_write(),
                    disk.write(0, 0, 1, filled(1, &[1; 512])).map(|_| ()),
                ),
            )
        }),
        |_| {},
    );
    assert_eq!((info.sectors, info.read_only), (64, true));
    assert_eq!(
        first.unwrap(),
        data[..1024],
        "its block 0 is the partition's first"
    );
    assert_eq!(
        last.unwrap(),
        data[63 * 512..],
        "and its last, the partition's last"
    );
    assert_eq!(
        answers,
        (
            remote(block::Error::OutOfRange),
            remote(block::Error::OutOfRange),
            remote(block::Error::ReadOnly),
            remote(block::Error::NotWriter),
        ),
        "a read past its end, one that runs past it, and every write are refused"
    );
    // The device manager is told it as `system`: its blocks, read only, never a volume.
    assert_eq!(
        moruna_storvsc_server::block(&system),
        devices_protocol::Block {
            serial: "system".into(),
            sector_size: 512,
            sectors: 64,
            read_only: true,
            boot: true,
        }
    );
    assert_eq!(
        moruna_storvsc_server::block(&world.driver.borrow().disks().next().unwrap().clone())
            .sectors,
        4096
    );
}
