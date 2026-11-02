//! SV-T5 (SDD 23, k), the driver's shape: a dispatcher that waits on a device's
//! interrupt (here a channel standing in for one) alongside its clients, and requests
//! answered when the device completes them, not when they arrive.

use std::{cell::RefCell, rc::Rc, thread};

use moruna_svc::{
    CallError, Channel, ClientCore, Completions, Context, DEADLINE_INFINITE, Decoder, Dispatcher,
    Encoder, Handle, Handler, Outcome, ProtocolId, Signals, Transport, Turn, Watch, Wire,
    deadline_in,
};

const BLOCK: ProtocolId = ProtocolId::new("moruna.test.block", 1, 0);

/// Requests waiting for the device.
#[derive(Default)]
struct Device {
    waiting: Vec<(moruna_svc::Pending, u32)>,
}

/// The device's interrupt: a message on `line` is one interrupt, acknowledged by
/// reading it; the requests waiting then complete.
struct Irq {
    line: Channel,
    device: Rc<RefCell<Device>>,
    interrupts: u32,
}

impl Watch for Irq {
    fn handle(&self) -> &Handle {
        &self.line.0
    }
    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }
    fn signaled(&mut self, seen: Signals, done: &mut Completions) -> bool {
        if !seen.contains(Signals::READABLE) {
            return false;
        }
        let mut ack = [0u8; 8];
        if self.line.read(&mut ack, &mut []).is_err() {
            return false;
        }
        self.interrupts += 1;
        for (pending, n) in self.device.borrow_mut().waiting.drain(..) {
            done.complete(pending, Outcome::reply(n * 10 + self.interrupts));
        }
        true
    }
}

/// Request 1 reads block `n` and is answered when the device interrupts; request 2
/// is answered at once; request 3 breaks the contract (`Deferred` without deferring);
/// request 4 defers and answers too.
struct Block(Rc<RefCell<Device>>);

impl Handler for Block {
    fn protocol(&self) -> ProtocolId {
        BLOCK
    }
    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        match ordinal {
            1 => match u32::decode(d) {
                Ok(n) => {
                    let pending = cx.defer();
                    assert_eq!(pending.connection(), cx.connection());
                    self.0.borrow_mut().waiting.push((pending, n));
                    Outcome::Deferred
                }
                Err(_) => Outcome::Status(moruna_svc::Status::BadMessage),
            },
            2 => Outcome::reply(7u32),
            3 => Outcome::Deferred,
            _ => {
                let _ = cx.defer();
                Outcome::reply(0u32)
            }
        }
    }
}

fn read(core: &mut ClientCore, n: u32) -> u32 {
    let mut e = Encoder::new();
    n.encode(&mut e).expect("encode");
    core.send(1, e).expect("send")
}

fn value(core: &mut ClientCore, txid: u32) -> u32 {
    let mut f = core.reply(txid, deadline_in(5_000_000_000)).expect("reply");
    let mut d = f.decoder();
    u32::decode(&mut d).expect("value")
}

#[test]
fn sv_t5_a_driver_answers_when_its_device_interrupts() {
    let (client_end, server_end) = Channel::create().expect("channel");
    let (irq_raise, irq_line) = Channel::create().expect("channel");
    let server = thread::spawn(move || {
        let device = Rc::new(RefCell::new(Device::default()));
        let mut d = Dispatcher::new();
        d.serve(server_end, Box::new(Block(device.clone())))
            .expect("serve");
        d.watch(Box::new(Irq {
            line: irq_line,
            device: device.clone(),
            interrupts: 0,
        }))
        .expect("watch");
        assert_eq!(d.len(), 2);
        // The watched line is not a channel: a server that watches objects for as long
        // as it serves counts only its connections and listeners.
        assert_eq!(d.channels(), 1);
        d.run().expect("run");
        device.borrow_mut().waiting.len()
    });
    let mut core =
        ClientCore::connect(client_end, BLOCK, deadline_in(5_000_000_000)).expect("hello");
    let a = read(&mut core, 1);
    let b = read(&mut core, 2);
    let c = core.send(2, Encoder::new()).expect("send");
    // The immediate answer arrives while the reads wait for the device.
    assert_eq!(value(&mut core, c), 7);
    assert_eq!(
        core.reply(a, moruna_svc::now() + 50_000_000).map(|_| ()),
        Err(CallError::TimedOut)
    );
    irq_raise.write_with(b"irq", &mut []).expect("interrupt");
    assert_eq!(value(&mut core, b), 21);
    assert_eq!(value(&mut core, a), 11);
    let late = read(&mut core, 3);
    irq_raise.write_with(b"irq", &mut []).expect("interrupt");
    assert_eq!(value(&mut core, late), 32);
    // The device goes away (its line closes) and then the client: the dispatcher
    // drops the watch and the connection, and ends.
    drop(irq_raise);
    drop(core);
    assert_eq!(server.join().expect("server"), 0);
}

#[test]
fn sv_t5_a_deferred_answer_is_given_once_and_only_to_a_live_connection() {
    let device = Rc::new(RefCell::new(Device::default()));
    let mut d = Dispatcher::new();
    let (client_end, server_end) = Channel::create().expect("channel");
    d.serve(server_end, Box::new(Block(device.clone())))
        .expect("serve");
    let client = thread::spawn(move || {
        let mut core = ClientCore::connect(client_end, BLOCK, DEADLINE_INFINITE).expect("hello");
        let a = read(&mut core, 4);
        let answer = value(&mut core, a);
        // Request 3 answers Deferred without deferring: the connection closes.
        assert_eq!(
            core.call(3, Encoder::new()).map(|_| ()),
            Err(CallError::PeerClosed)
        );
        answer
    });
    while device.borrow().waiting.is_empty() {
        assert_eq!(d.turn(deadline_in(5_000_000_000)), Ok(Turn::Served));
    }
    let (pending, n) = device.borrow_mut().waiting.remove(0);
    let again = pending.clone();
    assert_eq!(
        d.complete(pending, Outcome::Deferred),
        Err(Transport::Malformed)
    );
    assert_eq!(d.complete(again.clone(), Outcome::reply(n)), Ok(()));
    assert_eq!(
        d.complete(again.clone(), Outcome::reply(n)),
        Err(Transport::Malformed)
    );
    while d.turn(deadline_in(5_000_000_000)) != Ok(Turn::Empty) {}
    assert_eq!(client.join().expect("client"), 4);
    assert_eq!(
        d.complete(again, Outcome::reply(0u32)),
        Err(Transport::PeerClosed)
    );
}

#[test]
fn sv_t5_deferring_and_answering_closes_the_connection() {
    let device = Rc::new(RefCell::new(Device::default()));
    let mut d = Dispatcher::new();
    let (client_end, server_end) = Channel::create().expect("channel");
    d.serve(server_end, Box::new(Block(device))).expect("serve");
    let client = thread::spawn(move || {
        let mut core = ClientCore::connect(client_end, BLOCK, DEADLINE_INFINITE).expect("hello");
        core.call(4, Encoder::new()).map(|_| ())
    });
    while d.turn(deadline_in(5_000_000_000)) != Ok(Turn::Empty) {}
    assert_eq!(client.join().expect("client"), Err(CallError::PeerClosed));
}
