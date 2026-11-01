//! The network server (SDD 31): serves `moruna.net` on the listener slots its
//! configuration grants, over the network cards its configuration names.
//!
//! At start it reads its configuration (the slot `config`), finds each card through
//! the device manager's directory (the slot `directory`), asks it for its address,
//! MTU and link and subscribes to its frames, and builds the stack over them. Then
//! one dispatcher serves everything: the clients on the granted listeners, each card's
//! connection (a watch on it takes the card's answers: frames received, sends done,
//! link changes), and a timer at the stack's next deadline. After every turn the stack
//! is polled, the answers it made are sent, and the frames it made go to the cards, at
//! most a setting's sends in flight a card. When every client and listener has gone
//! the server lets the sockets still closing finish, for at most a second, and ends,
//! closing its cards' connections so their drivers end too.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use core::cell::RefCell;

use devices_protocol::directory;
use moruna_netstack::{Card, Config, Shared, Stack, after_turn, factory, parse};
use moruna_svc::{
    CallError, Channel, ClientCore, Completions, Console, DEADLINE_INFINITE, Decoder, Encoder,
    Handle, Memory, Rights, Signals, Timer, Watch, Wire, deadline_in, limits, now, println, random,
    start,
};
use net_protocol::{Frame, Link, SEND_FRAMES_MAX, net as card};

moruna_svc::main!(run);

const SECOND: u64 = 1_000_000_000;
const PAGE: u64 = 4096;
/// The longest configuration.
const CONFIG_MAX: u64 = 64 << 10;
/// What one send to a card carries besides its frames: the frame header, the count,
/// each frame's length.
const SEND_OVERHEAD: usize = 64;
/// How long the server lets sockets still closing finish once its clients have gone.
const LINGER_NS: u64 = SECOND;

/// One card's connection and the requests outstanding on it.
struct CardLink {
    service: String,
    core: ClientCore,
    receive: Option<u32>,
    link: Option<u32>,
    generation: u32,
    sends: Vec<u32>,
    batch: u32,
    failed: Option<String>,
}

impl CardLink {
    /// Takes every answer the card has sent, without waiting.
    fn pump(&mut self, index: usize, shared: &Shared) -> Result<(), String> {
        if let Some(t) = self.receive {
            match self.core.reply(t, 0) {
                Ok(mut f) => {
                    let mut d = f.decoder();
                    let frames = Vec::<Frame>::decode(&mut d)
                        .map_err(|e| format!("a receive's frames: {e:?}"))?;
                    d.finish()
                        .map_err(|e| format!("a receive's frames: {e:?}"))?;
                    let mut st = shared.stack.borrow_mut();
                    let port = &mut st.card_mut(index).port;
                    for frame in frames {
                        port.deliver(frame.0);
                    }
                    self.receive = None;
                }
                Err(CallError::TimedOut) => {}
                Err(e) => return Err(format!("receive: {e:?}")),
            }
        }
        if let Some(t) = self.link {
            match self.core.reply(t, 0) {
                Ok(mut f) => {
                    let mut d = f.decoder();
                    let link = Link::decode(&mut d).map_err(|e| format!("a link: {e:?}"))?;
                    d.finish().map_err(|e| format!("a link trailer: {e:?}"))?;
                    self.generation = link.generation;
                    shared.stack.borrow_mut().set_link(index, link.up);
                    self.link = None;
                }
                Err(CallError::TimedOut) => {}
                Err(e) => return Err(format!("link_changed: {e:?}")),
            }
        }
        let mut k = 0;
        while k < self.sends.len() {
            match self.core.reply(self.sends[k], 0) {
                Ok(mut f) => {
                    let mut decoder = f.decoder();
                    <()>::decode(&mut decoder).map_err(|e| format!("send answer: {e:?}"))?;
                    decoder
                        .finish()
                        .map_err(|e| format!("send answer trailer: {e:?}"))?;
                    self.sends.swap_remove(k);
                }
                Err(CallError::TimedOut) => k += 1,
                Err(e) => return Err(format!("send: {e:?}")),
            }
        }
        self.ask()
    }

    /// Keeps one receive and one wait for the link outstanding.
    fn ask(&mut self) -> Result<(), String> {
        if self.receive.is_none() {
            let mut e = Encoder::new();
            self.batch.encode(&mut e).map_err(|e| format!("{e:?}"))?;
            self.receive = Some(
                self.core
                    .send(6, e)
                    .map_err(|e| format!("receive: {e:?}"))?,
            );
        }
        if self.link.is_none() {
            let mut e = Encoder::new();
            self.generation
                .encode(&mut e)
                .map_err(|e| format!("{e:?}"))?;
            self.link = Some(
                self.core
                    .send(3, e)
                    .map_err(|e| format!("link_changed: {e:?}"))?,
            );
        }
        Ok(())
    }

    /// Sends the frames the stack made for this card, in batches that fit a message,
    /// while fewer than `in_flight` sends are outstanding.
    fn flush(&mut self, index: usize, shared: &Shared, in_flight: usize) -> Result<(), String> {
        loop {
            if self.sends.len() >= in_flight {
                return Ok(());
            }
            let batch: Vec<Frame> = {
                let mut st = shared.stack.borrow_mut();
                let tx = &mut st.card_mut(index).port.tx;
                let (mut n, mut bytes) = (0, SEND_OVERHEAD);
                while n < tx.len()
                    && n < SEND_FRAMES_MAX
                    && bytes + 4 + tx[n].len() <= limits::CHANNEL_MAX_BYTES
                {
                    bytes += 4 + tx[n].len();
                    n += 1;
                }
                tx.drain(..n).map(Frame).collect()
            };
            if batch.is_empty() {
                return Ok(());
            }
            let mut e = Encoder::new();
            batch.encode(&mut e).map_err(|e| format!("{e:?}"))?;
            self.sends
                .push(self.core.send(4, e).map_err(|e| format!("send: {e:?}"))?);
        }
    }
}

type Cards = Rc<RefCell<Vec<CardLink>>>;

/// A card's connection, waited on beside the clients.
struct CardWatch {
    handle: Handle,
    index: usize,
    cards: Cards,
    shared: Rc<Shared>,
}

impl Watch for CardWatch {
    fn handle(&self) -> &Handle {
        &self.handle
    }
    fn signals(&self) -> Signals {
        Signals::READABLE | Signals::PEER_CLOSED
    }
    fn signaled(&mut self, _: Signals, _: &mut Completions) -> bool {
        let mut cards = self.cards.borrow_mut();
        let c = &mut cards[self.index];
        if let Err(e) = c.pump(self.index, &self.shared) {
            c.failed.get_or_insert(e);
            // The interface is down from now on; the loop says why.
            self.shared.stack.borrow_mut().set_link(self.index, false);
            return false;
        }
        true
    }
}

/// The stack's next deadline.
struct Clock(Handle);

impl Watch for Clock {
    fn handle(&self) -> &Handle {
        &self.0
    }
    fn signals(&self) -> Signals {
        Signals::SIGNALED
    }
    fn signaled(&mut self, _: Signals, _: &mut Completions) -> bool {
        // The loop polls after every turn and sets the timer again.
        true
    }
}

/// The configuration's text: the slot's memory, page by page, to its first zero byte
/// or its end.
fn read_config() -> Result<String, String> {
    let memory = Memory(start::take("config").ok_or("no config slot")?);
    let mut bytes = Vec::new();
    let mut offset = 0;
    while offset < CONFIG_MAX {
        let Ok(mapping) = memory.map(offset, PAGE, 0) else {
            if offset == 0 {
                return Err(String::from("the config slot's memory does not map"));
            }
            break;
        };
        let mut page = alloc::vec![0u8; PAGE as usize];
        mapping
            .read(0, &mut page)
            .map_err(|e| format!("reading the configuration: {}", e.name()))?;
        if let Some(end) = page.iter().position(|b| *b == 0) {
            bytes.extend_from_slice(&page[..end]);
            break;
        }
        bytes.extend_from_slice(&page);
        offset += PAGE;
    }
    if offset >= CONFIG_MAX {
        return Err(String::from("the configuration exceeds its 64 KiB bound"));
    }
    String::from_utf8(bytes).map_err(|_| String::from("the configuration is not UTF-8"))
}

/// A length-prefixed Wire Vec<Vec<u8>> of DER CA roots, supplied only by boot.
fn read_tls_roots(handle: Handle) -> Result<Vec<Vec<u8>>, String> {
    let memory = Memory(handle);
    let first = memory
        .map(0, PAGE, 0)
        .map_err(|e| format!("TLS roots: {}", e.name()))?;
    let mut prefix = [0u8; 4];
    first
        .read(0, &mut prefix)
        .map_err(|e| format!("TLS roots length: {}", e.name()))?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len == 0 || len > 1 << 20 {
        return Err(String::from("TLS roots bundle is absent or over 1 MiB"));
    }
    let mut bytes = alloc::vec![0u8; len];
    let mut copied = 0;
    while copied < len {
        let offset = copied + 4;
        let page = (offset as u64 / PAGE) * PAGE;
        let within = offset % PAGE as usize;
        let n = (PAGE as usize - within).min(len - copied);
        let mapping = memory
            .map(page, PAGE, 0)
            .map_err(|e| format!("TLS roots page: {}", e.name()))?;
        mapping
            .read(within, &mut bytes[copied..copied + n])
            .map_err(|e| format!("TLS roots read: {}", e.name()))?;
        copied += n;
    }
    let mut handles = [];
    let mut decoder = Decoder::new(&bytes, &mut handles);
    let roots =
        Vec::<Vec<u8>>::decode(&mut decoder).map_err(|e| format!("TLS roots encoding: {e:?}"))?;
    decoder
        .finish()
        .map_err(|e| format!("TLS roots trailer: {e:?}"))?;
    Ok(roots)
}

/// Opens each card the configuration names: its connection, and what its driver says
/// of it.
fn open_cards(
    config: &Config,
    dir: &mut directory::Client,
    batch: u32,
) -> Result<(Vec<CardLink>, Vec<Card>), String> {
    let mut links = Vec::new();
    let mut cards = Vec::new();
    for iface in &config.interfaces {
        let channel = dir
            .lookup(iface.card.clone())
            .map_err(|e| format!("looking up {}: {e:?}", iface.card))?;
        let mut client = card::Client::connect(channel, deadline_in(60 * SECOND))
            .map_err(|e| format!("hello to {}: {e:?}", iface.card))?;
        client.core().set_timeout(60 * SECOND);
        let info = client
            .info()
            .map_err(|e| format!("{}: info: {e:?}", iface.card))?;
        let link = client
            .link()
            .map_err(|e| format!("{}: link: {e:?}", iface.card))?;
        client
            .subscribe()
            .map_err(|e| format!("{}: subscribe: {e:?}", iface.card))?;
        if info.offloads != 0 {
            return Err(format!(
                "{} offloads {:#x}, which this server does not take",
                iface.card, info.offloads
            ));
        }
        cards.push(Card {
            mac: info.mac,
            mtu: info.mtu,
            up: link.up,
        });
        let mut core_link = CardLink {
            service: iface.card.clone(),
            core: client.into_core(),
            receive: None,
            link: None,
            generation: link.generation,
            sends: Vec::new(),
            batch,
            failed: None,
        };
        core_link.ask()?;
        links.push(core_link);
    }
    Ok((links, cards))
}

fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 90;
    };
    match serve(&out) {
        Ok(()) => 0,
        Err(e) => {
            println!(out, "KE2 netstack: FAIL: {}", e);
            1
        }
    }
}

fn mac_text(mac: u64) -> String {
    let m = mac.to_be_bytes();
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[2], m[3], m[4], m[5], m[6], m[7]
    )
}

fn serve(out: &Console) -> Result<(), String> {
    let text = read_config()?;
    let config = parse(&text).map_err(|e| format!("the configuration: {e}"))?;
    let mut seed = [0u8; 8];
    random(&mut seed).map_err(|e| format!("no random source to seed the stack: {}", e.name()))?;
    // The listener slots, every one granted, every grant given its slot.
    let mut slots = Vec::new();
    for i in 0..start::count() {
        if let Some((_, _, name)) = start::entry(i).filter(|(_, _, n)| n.starts_with("listen")) {
            slots.push(name.to_string());
        }
    }
    for (slot, _) in &config.grants {
        if !slots.contains(slot) {
            return Err(format!("grant {slot} names a slot the boot did not give"));
        }
    }
    let mut listeners = Vec::new();
    for slot in slots {
        let Some((_, policy)) = config.grants.iter().find(|(s, _)| *s == slot) else {
            return Err(format!("slot {slot} has no grant"));
        };
        let channel = start::take(&slot)
            .map(Channel)
            .ok_or_else(|| format!("slot {slot} was taken"))?;
        listeners.push((channel, policy.clone()));
    }
    let (mut links, cards) = if config.interfaces.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let d = start::take("directory")
            .ok_or("no directory slot, and the configuration names cards")?;
        let mut dir = devices_protocol::open_directory(Channel(d), deadline_in(300 * SECOND))
            .map_err(|e| format!("the directory: {e:?}"))?;
        dir.core().set_timeout(300 * SECOND);
        open_cards(
            &config,
            &mut dir,
            config.settings.card_receive_frames as u32,
        )?
    };
    let mut stack = Stack::new(&config, &cards, u64::from_le_bytes(seed), now())?;
    match (start::take("tls-roots"), config.settings.tls_unix_seconds) {
        (None, 0) => {}
        (Some(roots), seconds) if seconds > 0 => {
            let time = moruna_netstack::tls::TrustedTime::new(
                seconds,
                now(),
                config.settings.tls_time_valid_ms * 1_000_000,
            )
            .ok_or("invalid trusted TLS clock anchor")?;
            let trust = moruna_netstack::tls::Trust::new(read_tls_roots(roots)?, time)
                .map_err(String::from)?;
            stack.install_tls(trust)?;
        }
        _ => {
            return Err(String::from(
                "TLS needs both the tls-roots capability and a trusted UTC anchor",
            ));
        }
    }
    let in_flight = stack.settings.card_sends_in_flight;
    let mut up = format!(
        "KE2 netstack: up: {} interfaces, {} grants, {} CPUs",
        stack.ifaces.len(),
        config.grants.len(),
        start::cpus()
    );
    for (k, x) in stack.ifaces.iter().enumerate() {
        let addrs: Vec<String> = x.info().addrs.iter().map(|c| format!("{c}")).collect();
        let card = if k == 0 {
            String::from("loopback")
        } else {
            format!("{} {}", links[k - 1].service, mac_text(x.mac))
        };
        let how = if x.info().dhcp { " dhcp" } else { "" };
        up.push_str(&format!(
            "; {} ({}){} {}",
            x.name,
            card,
            how,
            addrs.join(" ")
        ));
    }
    println!(out, "{}", up);
    let shared = Shared::new(stack);
    let mut d = moruna_svc::Dispatcher::new();
    for (channel, policy) in listeners {
        d.listen(channel, factory(shared.clone(), policy))
            .map_err(|e| format!("listening: {}", e.name()))?;
    }
    let watches: Vec<Handle> = links
        .iter_mut()
        .map(|l| l.core.channel().0.duplicate(Rights::WAIT | Rights::READ))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("a card's channel to wait on: {}", e.name()))?;
    let cards: Cards = Rc::new(RefCell::new(links));
    for (index, handle) in watches.into_iter().enumerate() {
        d.watch(Box::new(CardWatch {
            handle,
            index,
            cards: cards.clone(),
            shared: shared.clone(),
        }))
        .map_err(|e| format!("watching a card: {}", e.name()))?;
    }
    let timer = Timer::create().map_err(|e| format!("timer_create: {}", e.name()))?;
    d.watch(Box::new(Clock(
        timer
            .0
            .duplicate(Rights::WAIT)
            .map_err(|e| format!("the timer: {}", e.name()))?,
    )))
    .map_err(|e| format!("watching the timer: {}", e.name()))?;
    let mut reported = alloc::vec![false; cards.borrow().len()];
    let mut linger: Option<u64> = None;
    loop {
        after_turn(&shared, &mut d, now());
        for note in core::mem::take(&mut shared.stack.borrow_mut().notes) {
            println!(out, "KE2 netstack: {}", note);
        }
        {
            let mut links = cards.borrow_mut();
            for (index, l) in links.iter_mut().enumerate() {
                if l.failed.is_none()
                    && let Err(e) = l.flush(index, &shared, in_flight)
                {
                    l.failed = Some(e);
                }
                if let (Some(why), false) = (&l.failed, reported[index]) {
                    // The interface is down; loopback and the other cards serve on.
                    println!(out, "KE2 netstack: FAIL: card {}: {}", l.service, why);
                    shared.stack.borrow_mut().set_link(index, false);
                    reported[index] = true;
                }
            }
        }
        if d.channels() == 0 {
            let at = *linger.get_or_insert(now().saturating_add(LINGER_NS));
            if shared.stack.borrow().idle() || now() >= at {
                break;
            }
        }
        let next = shared.stack.borrow_mut().poll_at();
        // At the stack's next deadline (now, when frames wait), or the linger's end;
        // disarmed (zero) when neither is due.
        let deadline = match (next, linger) {
            (Some(n), Some(l)) => n.min(l),
            (n, l) => n.or(l).unwrap_or(0),
        };
        timer
            .set(if deadline == 0 { 0 } else { deadline.max(1) })
            .map_err(|e| format!("timer_set: {}", e.name()))?;
        d.turn(DEADLINE_INFINITE)
            .map_err(|e| format!("wait: {}", e.name()))?;
    }
    let st = shared.stack.borrow();
    let mut line = String::from("KE2 netstack: served until every client left");
    for k in 0..st.cards() {
        let x = &st.ifaces[k + 1];
        line.push_str(&format!(
            "; {}: {} frames out, {} in, {} dropped",
            x.name, x.port.tx_frames, x.port.rx_frames, x.port.dropped
        ));
    }
    println!(out, "{}", line);
    drop(st);
    // The watches hold duplicates of the cards' channels: they go first, so closing
    // the connections ends them and the drivers see their last client leave.
    drop(d);
    drop(cards);
    Ok(())
}
