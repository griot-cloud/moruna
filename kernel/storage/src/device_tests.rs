//! The device's tickets on a host (work item 1.3c), against the driver's end of the channel
//! driven by hand: one turn reads the driver's answers once and then polls its tickets, and
//! an answer that comes while they are polled is never left filed with the channel empty.

extern crate std;

use alloc::rc::Rc;
use core::cell::RefCell;

use block_protocol::{Info, block};
use moruna_svc::{Account, Channel, Encoder, Kind, Memory, Wire, deadline_in, frame};

use super::{Controller, Device, Error, Observation, Poll, Ticket};

const SECOND: u64 = 1_000_000_000;
const INFO: Info = Info {
    sector_size: 512,
    sectors: 1024,
    max_sectors: 8,
    read_only: false,
};

fn body<T: Wire>(value: T) -> Encoder {
    let mut e = Encoder::new();
    value.encode(&mut e).expect("encode");
    e
}

/// A device over a connection whose driver's end the test holds: its geometry and its
/// writer lease answered before they are asked, as a driver answers them.
fn device() -> (Device, Channel) {
    let (mine, theirs) = Channel::create().expect("channel");
    let mut welcome = Encoder::new();
    block::ID.major.encode(&mut welcome).expect("major");
    block::ID.minor.encode(&mut welcome).expect("minor");
    frame::send(&theirs, Kind::Welcome, 0, 0, welcome).expect("welcome");
    frame::send(&theirs, Kind::Reply, 1, 1, body(INFO)).expect("info");
    frame::send(&theirs, Kind::Reply, 9, 2, body(7u64)).expect("lease");
    let client = block::Client::connect(mine, deadline_in(5 * SECOND)).expect("connect");
    let controller = Controller::over(client, 5 * SECOND).expect("controller");
    for asked in [Kind::Hello, Kind::Request, Kind::Request] {
        assert_eq!(
            frame::recv(&theirs).expect("read").expect("asked").kind,
            asked
        );
    }
    let device = Device {
        controller: Rc::new(RefCell::new(controller)),
        base_sector: 0,
        info: INFO,
    };
    device.configure_async(4).expect("configure");
    (device, theirs)
}

fn account() -> Rc<Account> {
    Rc::new(Account::carve(1 << 20, "storage-1-3c").expect("an account"))
}

/// A read of one sector into new memory of `account`'s, submitted: its ticket, and the
/// request as the driver read it.
fn read(device: &Device, driver: &Channel, account: &Rc<Account>) -> (Ticket, frame::Frame) {
    let memory = Memory::create_in(4096, &account.0).expect("memory");
    let ticket = device
        .submit_read(0, 1, memory, account.clone())
        .expect("submit");
    let asked = frame::recv(driver).expect("read").expect("the read");
    assert_eq!((asked.kind, asked.ordinal), (Kind::Request, 2));
    (ticket, asked)
}

/// The driver's answer to a read: the buffer it was lent, handed back.
fn answer(driver: &Channel, mut asked: frame::Frame) {
    let txid = asked.txid;
    let mut d = asked.decoder();
    u64::decode(&mut d).expect("sector");
    u32::decode(&mut d).expect("count");
    let buffer = Memory::decode(&mut d).expect("buffer");
    frame::send(driver, Kind::Reply, 2, txid, body(buffer)).expect("answer");
}

fn pending(poll: Result<Poll, Error>) -> bool {
    matches!(poll, Ok(Poll::Pending { .. }))
}

fn complete(poll: Result<Poll, Error>) -> bool {
    matches!(poll, Ok(Poll::Complete(c)) if c.result.is_ok() && c.memory.is_some())
}

/// Whether the driver's channel has something to read: the owner waits on it, so it is
/// woken for it.
fn readable(device: &Device) -> bool {
    let mut controller = device.controller.borrow_mut();
    let core = controller.client.as_mut().expect("a client").core();
    core.channel().needed().expect("needed").is_some()
}

#[test]
fn an_answer_that_comes_while_the_turn_s_tickets_are_polled_is_never_left_behind() {
    let (device, driver) = device();
    let account = account();
    let (a, asked_a) = read(&device, &driver, &account);
    let (b, asked_b) = read(&device, &driver, &account);
    // One turn: the answers read once, then each ticket polled. `a`'s answer comes once
    // `a` has been polled, while `b` is.
    device.file_answers().expect("the turn's read");
    assert!(pending(device.poll(&a)));
    answer(&driver, asked_a);
    assert!(pending(device.poll(&b)));
    // The turn is over. `a`'s answer keeps the channel readable, so the owner's watch
    // wakes for it. Read by `b`'s poll, it would sit filed with the channel empty.
    assert!(
        readable(&device),
        "a's answer was read by b's poll and left filed: nothing wakes the owner for it"
    );
    // The next turn takes it; `b` is still the driver's.
    device.file_answers().expect("the turn's read");
    assert!(!readable(&device));
    assert!(complete(device.poll(&a)));
    assert!(pending(device.poll(&b)));
    answer(&driver, asked_b);
    device.file_answers().expect("the turn's read");
    assert!(complete(device.poll(&b)));
}

#[test]
fn a_pump_reads_the_answers_first_and_a_driver_that_has_gone_quarantines() {
    let (device, driver) = device();
    let account = account();
    let (a, asked_a) = read(&device, &driver, &account);
    let (b, _asked_b) = read(&device, &driver, &account);
    answer(&driver, asked_a);
    // A pump with no turn's read before it reads the answers itself.
    let seen = device.pump().expect("pump");
    let ready: alloc::vec::Vec<u64> = seen
        .iter()
        .filter(|o| matches!(o.state, Observation::Ready))
        .map(|o| o.ticket)
        .collect();
    assert_eq!(ready, [a.id()]);
    assert!(complete(device.poll(&a)));
    // The driver gone: the read finds it closed, and the original still out is held.
    drop(driver);
    device.file_answers().expect("the turn's read");
    assert!(matches!(device.poll(&b), Ok(Poll::Quarantined(_))));
    assert!(matches!(device.last_error(), Some(Error::Driver(_))));
    // A device with no connection left has nothing to read.
    device.file_answers().expect("nothing to read");
}
