//! One disk's I/O thread (`architecture/data-os.md` §3): it owns the queue's [`Engine`], takes
//! operations from any thread, polls the device's used ring while requests are in flight and
//! sleeps on a futex word only when it has none, so a busy disk costs no call per request and
//! an idle one no CPU.
//!
//! A submitter adds its operation to the disk's list and bumps the word; it makes a call (the
//! wake) only when the thread is asleep. A thread whose requests are slow to come back stops
//! spinning after a while and waits for the device's interrupt instead, with a short deadline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use moruna_mmio::Region;

use super::engine::{Engine, Op, Stats};

/// Empty polls of a busy queue before the thread waits for the device's interrupt.
pub const SPINS: u32 = 2048;
/// How long one wait for the interrupt lasts before the thread polls again.
pub const INTERRUPT_WAIT_NS: u64 = 1_000_000;

/// Where the I/O thread sleeps while it has nothing to do, and how it is woken.
pub trait Bell: Send + Sync {
    /// The word: a submitter bumps it before it wakes the thread.
    fn word(&self) -> &AtomicU32;
    /// Sleeps while the word holds `seen`, or less (a wake may come early).
    fn wait(&self, seen: u32);
    /// Wakes the thread.
    fn wake(&self);
}

/// What the I/O thread needs of the device beside its queue.
pub trait Port: Send {
    /// Tells the device the queue has new requests.
    fn ring(&self) -> Result<(), String>;
    /// Waits at most `ns` for the device to say it returned a request.
    fn wait_interrupt(&self, ns: u64);
    /// Resets the device, so it uses no memory it was given.
    fn reset(&self);
}

struct Shared {
    name: String,
    capacity: u64,
    length: AtomicU64,
    ops: Mutex<VecDeque<Op>>,
    bell: Arc<dyn Bell>,
    asleep: AtomicBool,
    closing: AtomicBool,
    stats: Mutex<Stats>,
    broken: Mutex<Option<String>>,
}

/// One disk, driven by its own thread. Cloned handles share it.
#[derive(Clone)]
pub struct Disk {
    shared: Arc<Shared>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk")
            .field("name", &self.shared.name)
            .field("capacity", &self.shared.capacity)
            .field("length", &self.length())
            .finish()
    }
}

impl Disk {
    /// Starts the I/O thread of disk `name` over `engine` and `port`; `length` is the bytes of
    /// the file the disk holds (at most its capacity).
    pub fn start<Q, S, P>(
        name: &str,
        engine: Engine<Q, S>,
        port: P,
        bell: Arc<dyn Bell>,
        length: u64,
    ) -> std::io::Result<Disk>
    where
        Q: Region + Send + 'static,
        S: Region + Send + 'static,
        P: Port + 'static,
    {
        let shared = Arc::new(Shared {
            name: name.to_string(),
            capacity: engine.capacity(),
            length: AtomicU64::new(length.min(engine.capacity())),
            ops: Mutex::new(VecDeque::new()),
            bell,
            asleep: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            stats: Mutex::new(Stats::default()),
            broken: Mutex::new(None),
        });
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name(format!("moruna-blk-{name}"))
            .spawn(move || run(theirs, engine, port))?;
        Ok(Disk {
            shared,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    /// Its start-page name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// The disk's bytes.
    pub fn capacity(&self) -> u64 {
        self.shared.capacity
    }

    /// The bytes of the file it holds.
    pub fn length(&self) -> u64 {
        self.shared.length.load(Ordering::SeqCst)
    }

    /// Says the file it holds is `length` bytes, at most its capacity: what the launcher
    /// named in its serial.
    pub fn set_length(&self, length: u64) {
        self.shared
            .length
            .store(length.min(self.shared.capacity), Ordering::SeqCst);
    }

    /// What it has moved so far.
    pub fn stats(&self) -> Stats {
        *lock(&self.shared.stats)
    }

    /// Hands the thread an operation; its `done` is told on the thread.
    pub fn submit(&self, op: Op) {
        if let Some(why) = lock(&self.shared.broken).clone() {
            (op.done)(Err(why));
            return;
        }
        if self.shared.closing.load(Ordering::SeqCst) {
            (op.done)(Err(format!("disk {} is closed", self.shared.name)));
            return;
        }
        lock(&self.shared.ops).push_back(op);
        self.shared.bell.word().fetch_add(1, Ordering::SeqCst);
        if self.shared.asleep.load(Ordering::SeqCst) {
            self.shared.bell.wake();
        }
    }

    /// Lets every operation it holds end, then resets the device and ends the thread.
    pub fn close(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.bell.word().fetch_add(1, Ordering::SeqCst);
        self.shared.bell.wake();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }

    /// Runs `op` and waits for it: for the few operations made outside the reactor (the
    /// serial at open, the output layer at the run's end).
    pub fn run(&self, mut op: Op) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        op.done = Box::new(move |r| {
            let _ = tx.send(r);
        });
        self.submit(op);
        rx.recv()
            .unwrap_or_else(|_| Err(format!("disk {} dropped an operation", self.shared.name)))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn run<Q: Region, S: Region, P: Port>(shared: Arc<Shared>, mut engine: Engine<Q, S>, port: P) {
    let mut spins = 0u32;
    loop {
        let taken: Vec<Op> = lock(&shared.ops).drain(..).collect();
        for op in taken {
            engine.submit(op);
        }
        engine.pump(|| port.ring());
        let done = engine.poll();
        *lock(&shared.stats) = engine.stats();
        if let Some(why) = engine.broken() {
            let why = format!("disk {}: {why}", shared.name);
            *lock(&shared.broken) = Some(why.clone());
            port.reset();
            engine.break_down(why.clone());
            engine.abandon();
            for op in lock(&shared.ops).drain(..) {
                (op.done)(Err(why.clone()));
            }
            *lock(&shared.stats) = engine.stats();
            return;
        }
        let busy = engine.in_flight() > 0 || engine.waiting() > 0;
        if !busy && shared.closing.load(Ordering::SeqCst) && lock(&shared.ops).is_empty() {
            port.reset();
            engine.break_down(format!("disk {} is closed", shared.name));
            engine.abandon();
            return;
        }
        if busy {
            if done > 0 {
                spins = 0;
            } else if spins < SPINS {
                spins += 1;
                std::hint::spin_loop();
            } else {
                spins = 0;
                port.wait_interrupt(INTERRUPT_WAIT_NS);
            }
            continue;
        }
        spins = 0;
        let seen = shared.bell.word().load(Ordering::SeqCst);
        shared.asleep.store(true, Ordering::SeqCst);
        if lock(&shared.ops).is_empty() && !shared.closing.load(Ordering::SeqCst) {
            shared.bell.wait(seen);
        }
        shared.asleep.store(false, Ordering::SeqCst);
    }
}

/// A bell on `std`'s parking, for a disk off the Moruna kernel (tests, models).
pub struct Parked {
    word: AtomicU32,
    lock: Mutex<()>,
    cv: std::sync::Condvar,
}

impl Parked {
    /// A bell nobody has rung.
    pub fn new() -> Parked {
        Parked {
            word: AtomicU32::new(0),
            lock: Mutex::new(()),
            cv: std::sync::Condvar::new(),
        }
    }
}

impl Default for Parked {
    fn default() -> Parked {
        Parked::new()
    }
}

impl Bell for Parked {
    fn word(&self) -> &AtomicU32 {
        &self.word
    }

    fn wait(&self, seen: u32) {
        let guard = lock(&self.lock);
        if self.word.load(Ordering::SeqCst) == seen {
            let _ = self
                .cv
                .wait_timeout(guard, std::time::Duration::from_millis(50));
        }
    }

    fn wake(&self) {
        let _guard = lock(&self.lock);
        self.cv.notify_all();
    }
}

/// A bell on the Moruna kernel's futex: a native call, never the personality's.
pub struct Futex(AtomicU32);

impl Futex {
    /// A bell nobody has rung.
    pub fn new() -> Futex {
        Futex(AtomicU32::new(0))
    }
}

impl Default for Futex {
    fn default() -> Futex {
        Futex::new()
    }
}

impl Bell for Futex {
    fn word(&self) -> &AtomicU32 {
        &self.0
    }

    fn wait(&self, seen: u32) {
        let _ = moruna_sys::futex_wait(&self.0, seen, moruna_sys::DEADLINE_INFINITE);
    }

    fn wake(&self) {
        let _ = moruna_sys::futex_wake(&self.0, 1);
    }
}
