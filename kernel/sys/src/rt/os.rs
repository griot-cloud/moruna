//! The kernel calls `rt` makes, each in one place. A host test of `rt` compiles the stand-ins at
//! the end of this file instead, which keep the same contracts with the host's own threads,
//! clock and memory, so the logic above them runs unchanged.

#[cfg(test)]
pub(super) use host::*;
#[cfg(not(test))]
pub(super) use moruna::*;

#[cfg(not(test))]
mod moruna {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use moruna_abi::{DEADLINE_INFINITE, Error, PAGE_SIZE, Signals, limits, map};

    use crate::datapath::ClockPage;
    use crate::{Handle, Memory, Result, raw};

    /// `futex_wait`: sleeps while `word` holds `expected`, until a wake or `deadline`.
    pub(in crate::rt) fn wait(word: &AtomicU32, expected: u32, deadline: u64) -> Result<()> {
        crate::futex_wait(word, expected, deadline)
    }

    /// `futex_wake`: wakes at most `count` threads sleeping on `word`; how many.
    pub(in crate::rt) fn wake(word: &AtomicU32, count: u32) -> u32 {
        crate::futex_wake(word, count).unwrap_or(0)
    }

    /// The clock page's address once mapped; 1 when the kernel maps none, and `clock_get` is
    /// asked instead (the same clock, to the nanosecond).
    static CLOCK: AtomicU64 = AtomicU64::new(0);

    /// Nanoseconds on the kernel's monotonic clock, read from the clock page with no call.
    pub(in crate::rt) fn clock() -> u64 {
        page().map_or_else(crate::now, |page| page.now())
    }

    /// The clock page, mapped at the first call; `None` when the kernel maps none.
    fn page() -> Option<ClockPage> {
        let mut page = CLOCK.load(Ordering::Relaxed);
        if page == 0 {
            page = ClockPage::map().map_or(1, |p| p.address());
            CLOCK.store(page, Ordering::Relaxed);
        }
        (page != 1).then(|| ClockPage::at(page))
    }

    /// The realtime base (zero while unset) and the monotonic clock, read together from the
    /// clock page; with no page, no base and `clock_get`.
    pub(in crate::rt) fn realtime() -> (u64, u64) {
        match page() {
            Some(page) => page.realtime_parts(),
            None => (0, crate::now()),
        }
    }

    /// The calling thread's thread pointer: the address its FS base holds, which its control
    /// block's first word holds again (the psABI's FS:0, read because `rdfsbase` may not be
    /// enabled). Every thread the runtime runs has one from before its first instruction of
    /// std's or the C library's.
    pub(in crate::rt) fn thread_pointer() -> *mut u8 {
        let base: u64;
        // SAFETY: the thread's FS base is its control block, whose first word is its address
        // (`tls::Block::new`); the load reads that word.
        unsafe {
            core::arch::asm!(
                "mov {}, qword ptr fs:[0]",
                out(reg) base,
                options(nostack, readonly, preserves_flags),
            );
        }
        core::ptr::with_exposed_provenance_mut(base as usize)
    }

    /// Points the calling thread's FS base at `pointer`, a control block (null: none).
    pub(in crate::rt) fn install(pointer: *mut u8) {
        // The address is this process's own memory, so the kernel takes it.
        let _ = crate::set_fs(pointer.expose_provenance() as u64);
    }

    /// The program's TLS template, from its own headers, which the std linker script maps at
    /// `__ehdr_start` (`kernel/targets/x86_64-moruna.ld`).
    pub(in crate::rt) fn template() -> super::super::tls::Template {
        use super::super::tls::Template;
        unsafe extern "C" {
            static __ehdr_start: u8;
        }
        let start = &raw const __ehdr_start;
        // SAFETY: the linker maps the ELF header and the program headers it names at
        // `__ehdr_start`, in the program's first segment, readable for its life.
        let headers = unsafe {
            let head = core::slice::from_raw_parts(start, 64);
            let phoff = u64::from_le_bytes(head[32..40].try_into().unwrap_or([0; 8])) as usize;
            let entsize = u16::from_le_bytes([head[54], head[55]]) as usize;
            let count = u16::from_le_bytes([head[56], head[57]]) as usize;
            core::slice::from_raw_parts(start, phoff + entsize * count)
        };
        Template::from_headers(headers).unwrap_or(Template::NONE)
    }

    /// The CPUs online, from the start page; one when it says none.
    pub(in crate::rt) fn cpus() -> usize {
        crate::start::cpus().max(1)
    }

    /// `thread_yield`: another ready thread runs first.
    pub(in crate::rt) fn yield_now() {
        crate::thread_yield();
    }

    /// Writes `line` on the console `console`, in one call.
    #[cfg(feature = "rt")]
    pub(in crate::rt) fn console_write(console: moruna_abi::RawHandle, line: &[u8]) {
        raw::debug_write(console, line);
    }

    /// A thread's stack: a memory object of its own, mapped, with its lowest page made
    /// unreachable so an overflow faults rather than writes past it.
    pub(in crate::rt) struct Stack {
        addr: u64,
        len: u64,
    }

    impl Stack {
        /// Maps `len` bytes, whole pages, the first of them the guard.
        pub(in crate::rt) fn map(len: usize) -> Result<Stack> {
            let memory = Memory::create(len as u64)?;
            let mapping = memory.map_detached(0, len as u64, map::WRITE)?;
            let stack = Stack {
                addr: mapping.addr(),
                len: mapping.len(),
            };
            // The stack owns the pages now: it unmaps them when dropped.
            core::mem::forget(mapping);
            // No access: a guard page. A kernel that refuses leaves the stack without one.
            let _ = raw::memory_protect(stack.addr, PAGE_SIZE, 0);
            Ok(stack)
        }

        /// The address above its last byte, where the thread's stack pointer starts.
        pub(in crate::rt) fn top(&self) -> u64 {
            self.addr + self.len
        }
    }

    impl Drop for Stack {
        fn drop(&mut self) {
            raw::call2(moruna_abi::nr::MEMORY_UNMAP, self.addr, self.len);
        }
    }

    /// A thread of this process, ended or not.
    pub(in crate::rt) struct ThreadHandle(Handle);

    /// Starts a thread at [`super::super::thread::trampoline`] with `arg`, its stack pointer at
    /// `stack`'s top.
    pub(in crate::rt) fn spawn(stack: &Stack, arg: u64) -> Result<ThreadHandle> {
        crate::Thread::spawn(super::super::thread::trampoline, stack.top(), arg)
            .map(|t| ThreadHandle(t.0))
    }

    /// Waits for the thread to end. A wait the kernel interrupts is made again; any other
    /// failure means the handle names no thread to wait for.
    pub(in crate::rt) fn join(thread: &ThreadHandle) {
        while crate::wait_one(&thread.0, Signals::TERMINATED, DEADLINE_INFINITE)
            == Err(Error::Interrupted)
        {}
    }

    /// Whether the thread has ended, asked with no wait.
    pub(in crate::rt) fn ended(thread: &ThreadHandle) -> bool {
        crate::wait_one(&thread.0, Signals::TERMINATED, 0)
            .is_ok_and(|seen| seen.contains(Signals::TERMINATED))
    }

    /// Ends the calling thread.
    pub(in crate::rt) fn thread_exit() -> ! {
        crate::thread_exit()
    }

    /// The largest stack the kernel maps: one memory object.
    pub(in crate::rt) const STACK_MAX: usize = limits::MEMORY_MAX_BYTES as usize;
}

#[cfg(test)]
mod host {
    extern crate std;

    use core::sync::atomic::{AtomicU32, Ordering};
    use std::cell::Cell;
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use moruna_abi::{DEADLINE_INFINITE, Error};

    use crate::Result;

    /// The host's futex: every wait sleeps on one condition, and a wake ends every wait,
    /// which the futex contract allows (a waiter may wake early).
    struct Futexes {
        generation: u64,
        waiting: u32,
    }

    static FUTEXES: Mutex<Futexes> = Mutex::new(Futexes {
        generation: 0,
        waiting: 0,
    });
    static WOKEN: Condvar = Condvar::new();

    pub(in crate::rt) fn wait(word: &AtomicU32, expected: u32, deadline: u64) -> Result<()> {
        let mut futexes = FUTEXES.lock().unwrap();
        // Read under the lock a waker takes after its store: no wake is lost.
        if word.load(Ordering::SeqCst) != expected {
            return Err(Error::ShouldWait);
        }
        let generation = futexes.generation;
        futexes.waiting += 1;
        let answer = loop {
            if futexes.generation != generation {
                break Ok(());
            }
            let now = clock();
            if deadline != DEADLINE_INFINITE && now >= deadline {
                break Err(Error::TimedOut);
            }
            let left = if deadline == DEADLINE_INFINITE {
                Duration::from_secs(1)
            } else {
                Duration::from_nanos(deadline - now)
            };
            futexes = WOKEN.wait_timeout(futexes, left).unwrap().0;
        };
        futexes.waiting -= 1;
        answer
    }

    pub(in crate::rt) fn wake(_word: &AtomicU32, count: u32) -> u32 {
        let mut futexes = FUTEXES.lock().unwrap();
        futexes.generation += 1;
        WOKEN.notify_all();
        count.min(futexes.waiting)
    }

    pub(in crate::rt) fn clock() -> u64 {
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        // Not zero, as the kernel's clock is not by the time a program runs.
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos() as u64
            + 1
    }

    /// The host stands in a wall clock never set.
    pub(in crate::rt) fn realtime() -> (u64, u64) {
        (0, clock())
    }

    std::thread_local! {
        static POINTER: Cell<usize> = const { Cell::new(0) };
    }

    /// The host's threads have their own FS base: the stand-in keeps the pointer aside.
    pub(in crate::rt) fn thread_pointer() -> *mut u8 {
        core::ptr::with_exposed_provenance_mut(POINTER.with(Cell::get))
    }

    pub(in crate::rt) fn install(pointer: *mut u8) {
        POINTER.with(|t| t.set(pointer.expose_provenance()));
    }

    /// A template of eight initialized bytes in a TLS of 96, aligned to 16.
    pub(in crate::rt) fn template() -> super::super::tls::Template {
        static IMAGE: [u8; 8] = *b"host tls";
        super::super::tls::Template {
            image: IMAGE.as_ptr() as usize,
            image_len: IMAGE.len(),
            size: 96,
            align: 16,
        }
    }

    pub(in crate::rt) fn cpus() -> usize {
        4
    }

    pub(in crate::rt) fn yield_now() {
        std::thread::yield_now();
    }

    /// A host process has no console slot, so nothing reaches here.
    #[cfg(feature = "rt")]
    pub(in crate::rt) fn console_write(_: moruna_abi::RawHandle, _: &[u8]) {}

    /// The host's stack is its thread's own.
    pub(in crate::rt) struct Stack;

    impl Stack {
        pub(in crate::rt) fn map(len: usize) -> Result<Stack> {
            if len > STACK_MAX {
                return Err(Error::NoMemory);
            }
            Ok(Stack)
        }
    }

    pub(in crate::rt) struct ThreadHandle(Mutex<Option<JoinHandle<()>>>);

    pub(in crate::rt) fn spawn(_stack: &Stack, arg: u64) -> Result<ThreadHandle> {
        let handle = std::thread::spawn(move || {
            // SAFETY: `arg` is the start `thread::spawn` boxed for exactly this thread.
            unsafe { super::super::thread::run(arg) }
        });
        Ok(ThreadHandle(Mutex::new(Some(handle))))
    }

    pub(in crate::rt) fn join(thread: &ThreadHandle) {
        if let Some(handle) = thread.0.lock().unwrap().take() {
            handle.join().unwrap();
        }
    }

    pub(in crate::rt) fn ended(thread: &ThreadHandle) -> bool {
        thread
            .0
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    /// A host stack is not mapped: the limit is only for the test of a refusal.
    pub(in crate::rt) const STACK_MAX: usize = 64 << 20;
}
