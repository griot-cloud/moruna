//! `moruna-engine` (SDD 30 d.4): the engine's command line, `moruna run` and `moruna serve`
//! (MH 4.2), with the standard kinds and no embedded interpreter. It is what runs on the Moruna
//! kernel, a static Linux program the personality starts with granted handles, where a Python
//! stage is an `image` whose interpreter is in the image; the `moruna` binary of `moruna-py` is
//! the same command line with the Python adapter compiled in.

/// The engine's Rust heap: counted while a warm-up runs, and its large buffers kept for reuse
/// while a run lasts (12 l), the same allocator the `moruna` binary and the Python extension
/// install, so the engine behaves the same on a host, in its own VM and on the kernel.
#[global_allocator]
static ALLOCATOR: moruna_runtime::check::warmup::CountingAllocator =
    moruna_runtime::check::warmup::CountingAllocator;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kinds = moruna_runtime::kinds::Registry::standard();
    let cancel = moruna_kernel::CancelToken::new();
    std::process::exit(moruna_runtime::host::cli::main(&args, &kinds, cancel));
}

/// The engine's monotonic clock on the Moruna kernel's clock page (`architecture/data-os.md`
/// C8): std's `Instant`, which the trace, the controller and the cost model time every batch
/// with, reads `clock_gettime(CLOCK_MONOTONIC)`, and this static musl program defines that
/// itself, so a time read is a few loads and the time-stamp counter, with no call. Every other
/// clock, every read off the kernel and every read before the kernel answers `clock_page` goes
/// to musl's own, as before.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
mod clock {
    use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

    use moruna_abi::clock_page as page;
    use moruna_sys::datapath::ClockReading;

    unsafe extern "C" {
        /// musl's own `clock_gettime`, which its public name is a weak alias of.
        fn __clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> libc::c_int;
    }

    const UNKNOWN: u8 = 0;
    const PAGE: u8 = 1;
    const NONE: u8 = 2;
    static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
    static ADDRESS: AtomicU64 = AtomicU64::new(0);

    #[unsafe(no_mangle)]
    extern "C" fn clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> libc::c_int {
        if clk == libc::CLOCK_MONOTONIC
            && !ts.is_null()
            && let Some(ns) = now()
        {
            // SAFETY: the caller passes a `timespec` to write, as `clock_gettime` requires.
            unsafe {
                (*ts).tv_sec = (ns / 1_000_000_000) as _;
                (*ts).tv_nsec = (ns % 1_000_000_000) as _;
            }
            return 0;
        }
        // SAFETY: the same arguments, to musl's own.
        unsafe { __clock_gettime(clk, ts) }
    }

    /// Monotonic nanoseconds from the clock page, once it is mapped.
    fn now() -> Option<u64> {
        let address = match STATE.load(Ordering::Acquire) {
            PAGE => ADDRESS.load(Ordering::Relaxed),
            NONE => return None,
            _ => map()?,
        };
        read(address)
    }

    /// Maps the clock page, once: off the kernel, or where the kernel refuses, never again.
    fn map() -> Option<u64> {
        let mapped = if moruna_micro::on_kernel() {
            moruna_sys::datapath::clock_page().ok()
        } else {
            None
        };
        match mapped {
            Some(address) => {
                ADDRESS.store(address, Ordering::Relaxed);
                STATE.store(PAGE, Ordering::Release);
            }
            None => STATE.store(NONE, Ordering::Release),
        }
        mapped
    }

    fn word(address: u64, at: usize) -> u64 {
        // SAFETY: the kernel keeps the clock page mapped readable for the life of the process,
        // and every field is an aligned `u64` it writes atomically under the sequence.
        unsafe { (*((address + at as u64) as *const AtomicU64)).load(Ordering::Acquire) }
    }

    /// The page read under its sequence: retried while the kernel writes it.
    fn read(address: u64) -> Option<u64> {
        let mut bytes = [0u8; page::LEN];
        for _ in 0..1000 {
            let before = word(address, page::SEQUENCE);
            for at in [page::BASE_NS, page::BASE_TSC, page::MULTIPLIER, page::SHIFT] {
                bytes[at..at + 8].copy_from_slice(&word(address, at).to_le_bytes());
            }
            let after = word(address, page::SEQUENCE);
            if let Some(reading) = ClockReading::from_bytes(before, &bytes, after) {
                // SAFETY: `rdtsc` reads the time-stamp counter, which user mode may read.
                let tsc = unsafe { core::arch::x86_64::_rdtsc() };
                return Some(reading.nanos_at(tsc));
            }
            std::hint::spin_loop();
        }
        None
    }
}
