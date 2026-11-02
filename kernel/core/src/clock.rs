//! The clock and the deadline queue (SDD 20, f.6).
//!
//! The clock is the TSC scaled to nanoseconds since boot. Deadlines live in one queue
//! ordered by time; OSTD's 1 kHz timer interrupt pops every entry that is due, firing a
//! timer object or waking a waiting thread. Every mutation of the queue, and of a
//! timer's armed state, happens under the queue's lock, which disables interrupts.

use alloc::{collections::BTreeMap, sync::Arc, sync::Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use moruna_abi::{DEADLINE_INFINITE, Error, Signals, charge};
use ostd::{
    arch::{read_tsc, tsc_freq},
    sync::{LocalIrqDisabled, SpinLock, Waker},
    timer,
};

use crate::{
    account::{Account, Charge},
    signal::Signaller,
};

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// The realtime clock at boot, in nanoseconds since 1970, read from the CMOS clock.
static REALTIME_AT_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_ORIGIN: AtomicU64 = AtomicU64::new(0);
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// A queue key: the deadline and a sequence number that makes it unique.
pub type Key = (u64, u64);

enum Entry {
    Timer(Weak<Timer>),
    Wake(Arc<Waker>),
}

static QUEUE: SpinLock<BTreeMap<Key, Entry>, LocalIrqDisabled> = SpinLock::new(BTreeMap::new());

/// Starts the clock and the deadline queue. Fails if OSTD could not measure the TSC.
pub fn init() -> Result<(), &'static str> {
    let hz = tsc_freq();
    if hz == 0 {
        return Err("the TSC frequency is unknown, so the clock cannot run");
    }
    TSC_HZ.store(hz, Ordering::Release);
    TSC_ORIGIN.store(read_tsc(), Ordering::Release);
    REALTIME_AT_BOOT.store(rtc::read()?, Ordering::Release);
    timer::register_callback_on_cpu(tick);
    Ok(())
}

/// Nanoseconds since the clock started.
pub fn now() -> u64 {
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 {
        return 0;
    }
    let ticks = read_tsc().wrapping_sub(TSC_ORIGIN.load(Ordering::Acquire));
    let ns = (ticks as u128 * 1_000_000_000) / hz as u128;
    u64::try_from(ns).unwrap_or(u64::MAX - 1)
}

/// The realtime clock at boot, in nanoseconds since 1970.
pub fn realtime_at_boot() -> u64 {
    REALTIME_AT_BOOT.load(Ordering::Acquire)
}

/// The realtime clock now, in nanoseconds since 1970: the CMOS clock at boot, read once,
/// advanced by the monotonic clock.
pub fn realtime() -> u64 {
    realtime_at_boot().saturating_add(now())
}

/// The CMOS real-time clock, read once at boot (the node's time, as the machine's
/// firmware keeps it, in UTC).
mod rtc {
    use ostd::{arch::device::io_port::ReadWriteAccess, io::IoPort};

    const INDEX: u16 = 0x70;
    const DATA: u16 = 0x71;

    struct Cmos {
        index: IoPort<u8, ReadWriteAccess>,
        data: IoPort<u8, ReadWriteAccess>,
    }

    impl Cmos {
        fn get(&self, register: u8) -> u8 {
            // Bit 7 of the index keeps NMIs enabled as they were: clear.
            self.index.write(register & 0x7f);
            self.data.read()
        }

        fn updating(&self) -> bool {
            self.get(0x0a) & 0x80 != 0
        }
    }

    struct Reading {
        second: u8,
        minute: u8,
        hour: u8,
        day: u8,
        month: u8,
        year: u8,
        century: u8,
    }

    fn sample(cmos: &Cmos, century: Option<u8>) -> Reading {
        while cmos.updating() {
            core::hint::spin_loop();
        }
        Reading {
            second: cmos.get(0x00),
            minute: cmos.get(0x02),
            hour: cmos.get(0x04),
            day: cmos.get(0x07),
            month: cmos.get(0x08),
            year: cmos.get(0x09),
            century: century.map(|r| cmos.get(r)).unwrap_or(0),
        }
    }

    fn same(a: &Reading, b: &Reading) -> bool {
        (
            a.second, a.minute, a.hour, a.day, a.month, a.year, a.century,
        ) == (
            b.second, b.minute, b.hour, b.day, b.month, b.year, b.century,
        )
    }

    fn bcd(v: u8) -> u8 {
        (v & 0x0f) + (v >> 4) * 10
    }

    /// Days from 1970-01-01 to the date (the civil-from-days inverse).
    fn days(year: i64, month: i64, day: i64) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn read() -> Result<u64, &'static str> {
        let index = IoPort::acquire(INDEX).map_err(|_| "the CMOS clock's index port is taken")?;
        let data = IoPort::acquire(DATA).map_err(|_| "the CMOS clock's data port is taken")?;
        let cmos = Cmos { index, data };
        let century = ostd::arch::kernel::ACPI_INFO
            .get()
            .and_then(|info| info.century_register)
            .map(|r| r.get());
        // Read until two readings agree, so an update between registers is not mixed in.
        let mut last = sample(&cmos, century);
        let reading = loop {
            let next = sample(&cmos, century);
            if same(&last, &next) {
                break next;
            }
            last = next;
        };
        let status = cmos.get(0x0b);
        let decode = |v: u8| if status & 0x04 == 0 { bcd(v) } else { v };
        let pm = reading.hour & 0x80 != 0;
        let mut hour = decode(reading.hour & 0x7f);
        if status & 0x02 == 0 {
            // A 12-hour clock: 12 AM is 0, 12 PM is 12.
            hour %= 12;
            if pm {
                hour += 12;
            }
        }
        let century = if century.is_some() {
            decode(reading.century) as i64
        } else {
            20
        };
        let year = century * 100 + decode(reading.year) as i64;
        let (month, day) = (decode(reading.month) as i64, decode(reading.day) as i64);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
            return Err("the CMOS clock holds no valid date");
        }
        let seconds = days(year, month, day) * 86_400
            + hour as i64 * 3600
            + decode(reading.minute) as i64 * 60
            + decode(reading.second) as i64;
        u64::try_from(seconds)
            .map(|s| s * 1_000_000_000)
            .map_err(|_| "the CMOS clock is before 1970")
    }
}

fn next_key(deadline: u64) -> Key {
    (deadline, SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

/// Queues a wake-up of `waker` at `deadline`; returns the key to cancel it with.
pub fn arm_wake(deadline: u64, waker: Arc<Waker>) -> Key {
    let key = next_key(deadline);
    QUEUE.lock().insert(key, Entry::Wake(waker));
    key
}

/// Cancels a queued wake-up; a no-op if it already fired.
pub fn disarm(key: Key) {
    QUEUE.lock().remove(&key);
}

fn tick() {
    let now = now();
    let mut queue = QUEUE.lock();
    while let Some(entry) = queue.first_entry() {
        if entry.key().0 > now {
            break;
        }
        match entry.remove() {
            Entry::Timer(timer) => {
                if let Some(timer) = timer.upgrade() {
                    timer.armed.store(0, Ordering::Relaxed);
                    timer.signaller.update(Signals::NONE, Signals::SIGNALED);
                }
            }
            Entry::Wake(waker) => {
                waker.wake_up();
            }
        }
    }
}

/// A timer object: raises `SIGNALED` when its deadline passes.
pub struct Timer {
    pub signaller: Signaller,
    /// The sequence number of the queued entry, zero when not armed.
    armed: AtomicU64,
    deadline: AtomicU64,
    _charge: Charge,
}

const _: () = assert!(core::mem::size_of::<Timer>() as u64 <= charge::OBJECT);

impl Timer {
    /// A timer, its record charged to `account`.
    pub fn new(account: &Arc<Account>) -> Result<Timer, Error> {
        Ok(Timer {
            signaller: Signaller::new(Signals::NONE),
            armed: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            _charge: Charge::new(account, charge::OBJECT)?,
        })
    }

    /// Clears `SIGNALED` and arms the timer for `deadline`; zero disarms it. A deadline
    /// already past fires on the next tick.
    pub fn set(self: &Arc<Self>, deadline: u64) {
        let mut queue = QUEUE.lock();
        let seq = self.armed.swap(0, Ordering::Relaxed);
        if seq != 0 {
            queue.remove(&(self.deadline.load(Ordering::Relaxed), seq));
        }
        self.signaller.update(Signals::SIGNALED, Signals::NONE);
        if deadline != 0 && deadline != DEADLINE_INFINITE {
            let key = next_key(deadline);
            self.deadline.store(deadline, Ordering::Relaxed);
            self.armed.store(key.1, Ordering::Relaxed);
            queue.insert(key, Entry::Timer(Arc::downgrade(self)));
        }
    }
}
