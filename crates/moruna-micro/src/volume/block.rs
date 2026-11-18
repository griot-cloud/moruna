//! A volume's driver on the kernel: `moruna.driver.block` over the connection the node granted
//! the engine for it (item 3.3).
//!
//! The arena's regions are registered with the driver the first time a request names one
//! (`register`, once a region, the region's own memory object), so a read's sectors land in the
//! arena buffer the reactor was handed, by the driver's DMA, at any byte of it (`read_to`, minor
//! 2), and a write leaves from the sink's own bytes (`write_from`). Bounces are pages of DMA
//! memory of the engine's own, registered once and kept for the next.
//!
//! Every request is sent as soon as it is made, from whatever thread makes it; one thread reads
//! the driver's answers as they come, and another tells them, so an operation whose answer makes
//! another request (a bounce after a refusal) never waits on the thread that reads answers.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use block_protocol::{Info, block};
use moruna_svc::{
    Channel, DEADLINE_INFINITE, Encoder, Memory, ProtocolError, Rights, Signals, Status, Wire,
    frame::{self, Frame, Kind},
};
use moruna_sys::{Mapping, map};

use super::io::{Bounce, Driver, Geometry, Moved, Told};
use crate::arena::Objects;

/// How long the driver's handshake and a call made at open may take.
const CALL_NS: u64 = 120_000_000_000;
/// The minor of `moruna.driver.block` that places a transfer at any byte.
pub const PLACED_SINCE: u16 = 2;
/// The bytes of a registration's page.
const PAGE: u64 = 4096;
/// The sizes bounces are kept in: an edge's sector, and the largest request.
const CLASSES: [usize; 2] = [4096, 1 << 20];

/// Who waits for an answer.
enum Waiter {
    /// A request on its way: told how it ended.
    Told(Told),
    /// A call: handed the frame.
    Call(mpsc::Sender<Result<Frame, String>>),
}

#[derive(Default)]
struct State {
    next: u32,
    waiting: HashMap<u32, Waiter>,
    /// Why the connection is gone.
    gone: Option<String>,
    /// Each arena region registered, by its id: its registration.
    regions: HashMap<u64, u32>,
    /// Each bounce's memory registered, by its first byte's address: its length and its
    /// registration.
    bounces: BTreeMap<u64, (u64, u32)>,
    /// Bounces free to take again, by class.
    free: [Vec<Slab>; 2],
}

/// A bounce's memory: a DMA memory object, mapped, registered.
struct Slab {
    _memory: Memory,
    mapping: Mapping<'static>,
    len: usize,
}

struct Shared {
    channel: Channel,
    state: Mutex<State>,
    /// Held while a region is registered, so two requests naming it register it once.
    registering: Mutex<()>,
    geometry: Geometry,
    minor: u16,
    /// The writer lease, for a sink's volume.
    lease: Option<u64>,
    objects: Arc<Objects>,
}

/// One volume's driver, on the kernel.
#[derive(Clone)]
pub struct BlockDriver(Arc<Shared>);

fn encode(args: impl FnOnce(&mut Encoder) -> Result<(), moruna_svc::WireError>) -> Encoder {
    let mut e = Encoder::new();
    // A request's arguments always encode; one that did not would be refused as a bad
    // message, and said so.
    let _ = args(&mut e);
    e
}

/// What a failure frame says.
fn failure(frame: &mut Frame) -> Moved {
    let mut d = frame.decoder();
    let code = u32::decode(&mut d).ok();
    match code {
        Some(c) if c == block::Error::BadBuffer.code() => Moved::Elsewhere,
        Some(c) if c == Status::UnknownMethod as u32 => Moved::Elsewhere,
        Some(c) => match block::Error::from_code(c) {
            Some(e) => Moved::Failed(format!("{e:?}")),
            None => Moved::Failed(format!("failure {c:#x}")),
        },
        None => Moved::Failed("a failure with no code".into()),
    }
}

impl Shared {
    /// Sends request `ordinal` for `waiter`; `Err` once the connection is gone.
    fn send(&self, ordinal: u16, body: Encoder, waiter: Waiter) -> Result<(), (Waiter, String)> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(why) = &s.gone {
            return Err((waiter, why.clone()));
        }
        s.next = s.next.wrapping_add(1).max(1);
        let txid = s.next;
        s.waiting.insert(txid, waiter);
        if let Err(e) = frame::send(&self.channel, Kind::Request, ordinal, txid, body) {
            let waiter = s.waiting.remove(&txid);
            let why = format!("sending request {ordinal} to the volume's driver: {e:?}");
            return match waiter {
                Some(w) => Err((w, why)),
                None => Ok(()),
            };
        }
        Ok(())
    }

    /// A request on its way, `told` how it ends.
    fn request(&self, ordinal: u16, body: Encoder, told: Told) {
        if let Err((Waiter::Told(told), why)) = self.send(ordinal, body, Waiter::Told(told)) {
            told(Moved::Failed(why));
        }
    }

    /// A call, waited for.
    fn call(&self, ordinal: u16, body: Encoder) -> Result<Frame, String> {
        let (tx, rx) = mpsc::channel();
        if let Err((_, why)) = self.send(ordinal, body, Waiter::Call(tx)) {
            return Err(why);
        }
        rx.recv_timeout(Duration::from_nanos(CALL_NS))
            .map_err(|_| format!("the volume's driver did not answer request {ordinal}"))?
    }

    /// The registration and the byte within it of the `len` bytes at `addr`: a bounce's, or an
    /// arena region's, registered now if it was not. `None`: no memory the driver reaches.
    fn place(&self, addr: u64, len: u64) -> Result<Option<(u32, u64)>, String> {
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((&base, &(n, id))) = s.bounces.range(..=addr).next_back()
                && addr + len <= base + n
            {
                return Ok(Some((id, addr - base)));
            }
        }
        let Some((region, base, bytes)) = self.objects.find(addr) else {
            return Ok(None);
        };
        if addr + len > base + bytes {
            return Ok(None);
        }
        let known = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Registrations of regions the arena has let go end now.
            let gone: Vec<(u64, u32)> = s
                .regions
                .iter()
                .filter(|(r, _)| !self.objects.holds(**r))
                .map(|(r, id)| (*r, *id))
                .collect();
            for (r, id) in gone {
                s.regions.remove(&r);
                drop(s);
                let _ = self.call(8, encode(|e| id.encode(e)));
                s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            }
            s.regions.get(&region).copied()
        };
        let id = match known {
            Some(id) => id,
            None => {
                let _one = self.registering.lock().unwrap_or_else(|e| e.into_inner());
                let again = self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .regions
                    .get(&region)
                    .copied();
                match again {
                    Some(id) => id,
                    None => {
                        let (memory, _) = self
                            .objects
                            .memory(region)
                            .ok_or("an arena region let go while a request named it")?;
                        let id = self.register(&memory)?;
                        self.state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .regions
                            .insert(region, id);
                        id
                    }
                }
            }
        };
        Ok(Some((id, addr - base)))
    }

    /// Registers `memory` with the driver: its registration.
    fn register(&self, memory: &Memory) -> Result<u32, String> {
        let handle = memory
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
            .map_err(|e| format!("duplicating memory for the volume's driver: {}", e.name()))?;
        let mut body = Encoder::new();
        Memory(handle)
            .encode(&mut body)
            .map_err(|e| format!("encoding a registration: {e:?}"))?;
        let mut frame = self.call(5, body)?;
        if frame.kind != Kind::Reply {
            return Err(format!(
                "the volume's driver refused a registration: {:?}",
                failure(&mut frame)
            ));
        }
        let mut d = frame.decoder();
        let (id, _pages) =
            <(u32, u32)>::decode(&mut d).map_err(|e| format!("a registration's answer: {e:?}"))?;
        Ok(id)
    }

    /// A placed transfer: `read_to` or `write_from` on a driver of minor 2, `read_at` or
    /// `write_at` on an older one when the bytes start a page; `Elsewhere` otherwise.
    fn placed(&self, write: bool, addr: u64, sector: u64, count: u32, told: Told) {
        let bytes = u64::from(count) * self.geometry.sector;
        let (id, offset) = match self.place(addr, bytes) {
            Ok(Some(at)) => at,
            Ok(None) => return told(Moved::Elsewhere),
            Err(why) => return told(Moved::Failed(why)),
        };
        let lease = self.lease.unwrap_or(0);
        let (ordinal, body) = if self.minor >= PLACED_SINCE {
            let ordinal = if write { 12 } else { 11 };
            (
                ordinal,
                encode(|e| {
                    if write {
                        lease.encode(e)?;
                    }
                    (id, offset, sector, count).encode(e)
                }),
            )
        } else if offset.is_multiple_of(PAGE) && offset / PAGE <= u64::from(u32::MAX) {
            let page = (offset / PAGE) as u32;
            let ordinal = if write { 7 } else { 6 };
            (
                ordinal,
                encode(|e| {
                    if write {
                        lease.encode(e)?;
                    }
                    (id, page, sector, count).encode(e)
                }),
            )
        } else {
            return told(Moved::Elsewhere);
        };
        self.request(ordinal, body, told);
    }

    /// Fails every request waiting, once the connection is gone.
    fn end(&self, why: String) -> Vec<Waiter> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.gone.get_or_insert(why);
        s.waiting.drain().map(|(_, w)| w).collect()
    }
}

/// Reads the driver's answers, and hands each waiter its own.
fn pump(shared: Arc<Shared>, answers: mpsc::Sender<(Told, Moved)>) {
    loop {
        match moruna_svc::wait_one(
            &shared.channel.0,
            Signals::READABLE | Signals::PEER_CLOSED,
            DEADLINE_INFINITE,
        ) {
            Ok(_) => {}
            Err(e) => {
                return gone(&shared, &answers, format!("waiting: {}", e.name()));
            }
        }
        loop {
            let mut frame = match frame::recv(&shared.channel) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return gone(&shared, &answers, format!("reading: {e:?}")),
            };
            let waiter = shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .waiting
                .remove(&frame.txid);
            match waiter {
                Some(Waiter::Told(told)) => {
                    let moved = match frame.kind {
                        Kind::Reply => Moved::Done,
                        Kind::Failure => failure(&mut frame),
                        other => Moved::Failed(format!("an answer of kind {other:?}")),
                    };
                    let _ = answers.send((told, moved));
                }
                Some(Waiter::Call(tx)) => {
                    let _ = tx.send(Ok(frame));
                }
                None => {}
            }
        }
        if moruna_svc::wait_one(&shared.channel.0, Signals::PEER_CLOSED, 0).is_ok() {
            return gone(&shared, &answers, "the volume's driver closed".into());
        }
    }
}

fn gone(shared: &Shared, answers: &mpsc::Sender<(Told, Moved)>, why: String) {
    for waiter in shared.end(why.clone()) {
        match waiter {
            Waiter::Told(told) => {
                let _ = answers.send((told, Moved::Failed(why.clone())));
            }
            Waiter::Call(tx) => {
                let _ = tx.send(Err(why.clone()));
            }
        }
    }
}

impl BlockDriver {
    /// The driver on `channel`, a connection to a volume's driver the node granted: the
    /// handshake, its geometry, and for a sink's volume (`writable`) the writer lease.
    pub fn open(
        channel: Channel,
        objects: Arc<Objects>,
        writable: bool,
    ) -> Result<BlockDriver, String> {
        let mut client = block::Client::connect(channel, moruna_svc::deadline_in(CALL_NS))
            .map_err(|e| format!("hello to the volume's driver: {e:?}"))?;
        let minor = client.core().server_minor();
        client.core().set_timeout(CALL_NS);
        let info: Info = client
            .info()
            .map_err(|e| format!("the volume's geometry: {e:?}"))?;
        let lease = if writable {
            Some(
                client
                    .acquire_write()
                    .map_err(|e| format!("the volume's writer lease: {e:?}"))?,
            )
        } else {
            None
        };
        let geometry = Geometry {
            sector: u64::from(info.sector_size),
            sectors: info.sectors,
            max_sectors: info.max_sectors,
            read_only: info.read_only,
        };
        let shared = Arc::new(Shared {
            channel: client.into_core().into_channel(),
            state: Mutex::default(),
            registering: Mutex::default(),
            geometry,
            minor,
            lease,
            objects,
        });
        let (tx, rx) = mpsc::channel::<(Told, Moved)>();
        let reader = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("moruna-volume".into())
            .spawn(move || pump(reader, tx))
            .map_err(|e| format!("the volume's reader: {e}"))?;
        std::thread::Builder::new()
            .name("moruna-volume-told".into())
            .spawn(move || {
                while let Ok((told, moved)) = rx.recv() {
                    told(moved);
                }
            })
            .map_err(|e| format!("the volume's answers: {e}"))?;
        Ok(BlockDriver(shared))
    }

    /// The minor version the driver announced.
    pub fn minor(&self) -> u16 {
        self.0.minor
    }
}

impl Driver for BlockDriver {
    fn geometry(&self) -> Geometry {
        self.0.geometry
    }

    fn read_to(&self, addr: u64, sector: u64, count: u32, told: Told) {
        self.0.placed(false, addr, sector, count, told);
    }

    fn write_from(&self, addr: u64, sector: u64, count: u32, told: Told) {
        self.0.placed(true, addr, sector, count, told);
    }

    fn flush(&self, told: Told) {
        let lease = self.0.lease.unwrap_or(0);
        self.0.request(4, encode(|e| lease.encode(e)), told);
    }

    fn bounce(&self, bytes: usize) -> Result<Box<dyn Bounce>, String> {
        let class = CLASSES.iter().position(|c| bytes <= *c);
        let len = class.map_or(bytes.next_multiple_of(PAGE as usize), |c| CLASSES[c]);
        if let Some(c) = class
            && let Some(slab) = self.0.state.lock().unwrap_or_else(|e| e.into_inner()).free[c].pop()
        {
            return Ok(Box::new(Held {
                slab: Some(slab),
                class,
                shared: Arc::clone(&self.0),
                bytes,
            }));
        }
        let memory = Memory::create_dma(len as u64)
            .map_err(|e| format!("a bounce of {len} bytes: {}", e.name()))?;
        let mapping = memory
            .map_detached(0, len as u64, map::WRITE)
            .map_err(|e| format!("mapping a bounce: {}", e.name()))?;
        let id = self.0.register(&memory)?;
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bounces
            .insert(mapping.addr(), (len as u64, id));
        Ok(Box::new(Held {
            slab: Some(Slab {
                _memory: memory,
                mapping,
                len,
            }),
            class,
            shared: Arc::clone(&self.0),
            bytes,
        }))
    }
}

/// A bounce taken: back to its class's free list when dropped.
struct Held {
    slab: Option<Slab>,
    class: Option<usize>,
    shared: Arc<Shared>,
    bytes: usize,
}

impl Bounce for Held {
    fn addr(&self) -> u64 {
        self.slab.as_ref().map_or(0, |s| s.mapping.addr())
    }

    #[allow(unsafe_code)]
    fn bytes(&self) -> &[u8] {
        let Some(s) = &self.slab else { return &[] };
        // SAFETY: the mapping is `s.len` bytes of this process's memory, mapped writable for as
        // long as the slab is held, and `bytes` is at most `s.len`; the driver writes it only
        // while a request naming it is on its way, which has ended before the bounce is read.
        unsafe { std::slice::from_raw_parts(s.mapping.addr() as *const u8, self.bytes.min(s.len)) }
    }

    #[allow(unsafe_code)]
    fn bytes_mut(&mut self) -> &mut [u8] {
        let Some(s) = &self.slab else {
            return &mut [];
        };
        // SAFETY: as `bytes`, and `&mut self` makes this the only view of it.
        unsafe {
            std::slice::from_raw_parts_mut(s.mapping.addr() as *mut u8, self.bytes.min(s.len))
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some(slab) = self.slab.take() else { return };
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.class {
            Some(c) => s.free[c].push(slab),
            None => {
                // An odd size is not kept: its registration ends with it.
                if let Some((_, id)) = s.bounces.remove(&slab.mapping.addr()) {
                    drop(s);
                    let _ = self.shared.send(
                        8,
                        encode(|e| id.encode(e)),
                        Waiter::Told(Box::new(|_| {})),
                    );
                }
            }
        }
    }
}
