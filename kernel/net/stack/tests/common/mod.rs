//! The network server on a host (SDD 31, k): its stack and dispatcher on a thread of
//! their own, run as the server's loop runs them, with its cards joined to whatever
//! a test puts at the other end of their wire.

#![allow(dead_code)]

use std::{collections::BTreeMap, rc::Rc, thread};

use moruna_netstack::{Card, Shared, Stack, after_turn, factory, parse};
use moruna_svc::{Channel, Dispatcher, Turn, now};

pub const MAC_A: u64 = 0x5254_004b_4502;
pub const MAC_B: u64 = 0x5254_004b_4503;

/// The configuration of the suites' server: loopback for the one-server suite, and
/// two cards on one link, `a` and `b`, for the pair suite, with the resolver on `b`.
pub const PAIR: &str = "moruna-netstack 1
setting tcp-receive-buffer 131072
setting tcp-send-buffer 131072
setting connect-timeout-ms 10000
setting resolve-timeout-ms 10000
setting ping-timeout-ms 2000
interface a card=net2 ipv4=10.47.0.1/24 ipv6=fd4b:4532::1/64
interface b card=net3 ipv4=10.47.0.2/24 ipv6=fd4b:4532::2/64
dns 10.47.0.2
grant listen-suite rights=connect,listen,udp,ping,grant remote=127.0.0.0/8,::1/128 local=127.0.0.0/8,::1/128 sockets=32
grant listen-a rights=connect,listen,udp,ping remote=10.47.0.0/24,fd4b:4532::/64 local=10.47.0.1/32,fd4b:4532::1/128 names=*.suite.ke2.test sockets=32
grant listen-b rights=connect,listen,udp,ping remote=10.47.0.0/24,fd4b:4532::/64 local=10.47.0.2/32,fd4b:4532::2/128 sockets=32
";

/// A tiny deterministic generator, for the wire's flaws and the fuzzing.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// True `per_mille` times in a thousand.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.next() % 1000 < per_mille
    }
}

/// What a link between card 0 and card 1 does to frames: loses, duplicates and
/// reorders them, each so many times in a thousand.
#[derive(Clone, Copy, Default)]
pub struct Flaws {
    pub lose: u64,
    pub duplicate: u64,
    pub reorder: u64,
    /// Every frame is lost.
    pub cut: bool,
}

/// What stands at the other end of the cards' wires: called once a loop with the
/// stack, it takes what the cards sent and delivers what they receive.
pub type Wire = Box<dyn FnMut(&mut Stack, &mut Rng) + Send>;

/// No external NICs: the stack itself still moves every loopback IP packet.
/// Unlike `link`, this fixture must not address card zero or card one.
pub fn no_external_cards() -> Wire {
    Box::new(|stack, _| assert_eq!(stack.cards(), 0))
}

/// The pair's wire: card 0 and card 1 on one link, with `flaws`.
pub fn link(flaws: Flaws) -> Wire {
    let mut held: Vec<(usize, Vec<u8>)> = Vec::new();
    Box::new(move |st: &mut Stack, rng: &mut Rng| {
        for (from, to) in [(0, 1), (1, 0)] {
            for f in std::mem::take(&mut st.card_mut(from).port.tx) {
                if flaws.cut || rng.chance(flaws.lose) {
                    continue;
                }
                if rng.chance(flaws.reorder) {
                    held.push((to, f));
                    continue;
                }
                if rng.chance(flaws.duplicate) {
                    st.card_mut(to).port.deliver(f.clone());
                }
                st.card_mut(to).port.deliver(f);
                // A frame held back goes out after a later one.
                if let Some(k) =
                    (!held.is_empty() && rng.chance(500)).then(|| rng.next() as usize % held.len())
                {
                    let (t, h) = held.swap_remove(k);
                    st.card_mut(t).port.deliver(h);
                }
            }
        }
    })
}

/// Serves `config` over cards of `macs`, their wires `wire`; returns the client end of
/// every granted slot's listener, and the server's thread, which ends when every
/// client and listener has gone.
pub fn serve(
    config: &str,
    macs: &[u64],
    wire: Wire,
) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    serve_with_tls(config, macs, wire, None)
}

pub fn serve_with_tls(
    config: &str,
    macs: &[u64],
    wire: Wire,
    trust: Option<moruna_netstack::tls::Trust>,
) -> (BTreeMap<String, Channel>, thread::JoinHandle<()>) {
    let config = parse(config).expect("the configuration");
    let mut ours = BTreeMap::new();
    let mut theirs = Vec::new();
    for (slot, policy) in &config.grants {
        let (o, t) = Channel::create().expect("a channel");
        ours.insert(slot.clone(), o);
        theirs.push((t, policy.clone()));
    }
    let cards: Vec<Card> = macs
        .iter()
        .map(|&mac| Card {
            mac,
            mtu: 1500,
            up: true,
        })
        .collect();
    let join = thread::spawn(move || {
        let mut wire = wire;
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut stack = Stack::new(&config, &cards, 0x1234_5678, now()).expect("the stack");
        if let Some(trust) = trust {
            stack.install_tls(trust).expect("TLS trust");
        }
        let shared = Shared::new(stack);
        let mut d = Dispatcher::new();
        for (listener, policy) in theirs {
            d.listen(listener, factory(Rc::clone(&shared), policy))
                .expect("listen");
        }
        loop {
            after_turn(&shared, &mut d, now());
            let deadline = {
                let mut st = shared.stack.borrow_mut();
                wire(&mut st, &mut rng);
                let t = now();
                st.poll_at()
                    .unwrap_or(t + 50_000_000)
                    .clamp(t, t + 50_000_000)
            };
            if d.channels() == 0 {
                break;
            }
            if d.turn(deadline).expect("a turn") == Turn::Empty {
                break;
            }
        }
    });
    (ours, join)
}

/// Test-only byte collection from the canonical caller-owned Memory operation.
/// Production clients retain and consume their charged destination directly.
pub fn receive(
    client: &mut netstack_protocol::net::Client,
    socket: u32,
    max: u32,
) -> Result<Vec<u8>, moruna_svc::CallError<netstack_protocol::net::Error>> {
    use moruna_svc::{CallError, Memory, Rights, map};
    let size = (u64::from(max.max(1)) + 4095) / 4096 * 4096;
    let memory = Memory::create_in(size, &client.account().0).map_err(CallError::Kernel)?;
    let loan = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)
            .map_err(CallError::Kernel)?,
    );
    let count = client.receive_into(socket, loan, 0, max)?;
    if count > max {
        return Err(CallError::Malformed);
    }
    let mut bytes = vec![0; count as usize];
    memory
        .map(0, size, map::READ)
        .map_err(CallError::Kernel)?
        .read(0, &mut bytes)
        .map_err(CallError::Kernel)?;
    Ok(bytes)
}

/// Test-only byte fixture staging into the sole net3 caller-owned TX operation.
pub fn send(
    client: &mut netstack_protocol::net::Client,
    socket: u32,
    bytes: &[u8],
) -> Result<u32, moruna_svc::CallError<netstack_protocol::net::Error>> {
    use moruna_svc::{CallError, Memory, Rights, map};
    let capacity = (bytes.len().max(1) as u64).div_ceil(4096) * 4096;
    let memory = Memory::create_in(capacity, &client.account().0).map_err(CallError::Kernel)?;
    memory
        .map(0, capacity, map::WRITE)
        .and_then(|m| m.write(0, bytes))
        .map_err(CallError::Kernel)?;
    let loan = Memory(
        memory
            .0
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .map_err(CallError::Kernel)?,
    );
    client.send_from(socket, loan, 0, bytes.len() as u32)
}
