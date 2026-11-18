//! The kernel calls this crate makes, each in one place. A host test compiles the stand-ins at
//! the end of this file instead, which keep the same contracts with the host's own memory,
//! clock and threads, so the logic above them runs unchanged. An error is the kernel's code
//! (`moruna_abi::Error`), which [`crate::from_kernel`] turns into an errno.

/// What a mapping may be used for: readable unless it is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

#[cfg(target_os = "moruna")]
pub use core_os::*;

#[cfg(not(target_os = "moruna"))]
pub use host::*;

/// The core's calls, through std's own copy of moruna-sys (`std::os::moruna::moruna_sys`), so
/// relibc shares std's runtime: its start page, its log, its threads and their control blocks
/// (system design 5B). A second copy of the crate would have none of them.
#[cfg(target_os = "moruna")]
mod core_os {
    use core::sync::atomic::AtomicU32;

    use std::boxed::Box;
    use std::os::moruna::moruna_sys as sys;

    use super::Access;

    fn code(e: sys::Error) -> u32 {
        e.code()
    }

    fn flags(a: Access) -> u64 {
        let mut f = 0;
        if a.read {
            f |= sys::map::READ;
        }
        if a.write {
            f |= sys::map::WRITE;
        }
        if a.execute {
            f |= sys::map::EXECUTE;
        }
        f
    }

    /// `len` bytes (whole pages) of a new memory object, mapped readable (and as `access` says
    /// once mapped): the address. The mapping is the C library's from here: `munmap` ends it.
    pub fn map(len: usize, access: Access) -> Result<usize, u32> {
        let memory = sys::Memory::create(len as u64).map_err(code)?;
        let first = Access {
            read: true,
            ..access
        };
        let mapping = memory
            .map_detached(0, len as u64, flags(first))
            .map_err(code)?;
        let addr = mapping.addr() as usize;
        // The pages stay mapped whatever becomes of the handle and the mapping's value:
        // the C library unmaps them by address.
        core::mem::forget(mapping);
        drop(memory);
        if !access.read {
            // SAFETY: the range was mapped just now and nothing holds it yet.
            unsafe { protect(addr, len, access) }?;
        }
        Ok(addr)
    }

    /// # Safety
    ///
    /// Nothing in the process uses the range again.
    pub unsafe fn unmap(addr: usize, len: usize) -> Result<(), u32> {
        // SAFETY: the caller's contract.
        unsafe { sys::unmap(addr as u64, len as u64) }.map_err(code)
    }

    /// # Safety
    ///
    /// Nothing in the process relies on an access the range loses.
    pub unsafe fn protect(addr: usize, len: usize, access: Access) -> Result<(), u32> {
        // SAFETY: the caller's contract.
        unsafe { sys::protect(addr as u64, len as u64, flags(access)) }.map_err(code)
    }

    pub fn monotonic_ns() -> u64 {
        sys::now()
    }

    /// The wall clock, std's: the clock page's realtime base plus the monotonic clock, or 1970
    /// plus the monotonic clock while the base is unset (ABI 2.8).
    pub fn realtime_ns() -> u64 {
        let now = sys::rt::time::SystemTime::now();
        now.sub_time(&sys::rt::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }

    pub fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> Result<(), u32> {
        sys::futex_wait(word, expected, deadline).map_err(code)
    }

    pub fn futex_wake(word: &AtomicU32, count: u32) -> Result<u32, u32> {
        sys::futex_wake(word, count).map_err(code)
    }

    pub fn random(buf: &mut [u8]) -> Result<(), u32> {
        sys::random(buf).map_err(code)
    }

    pub fn set_fs(base: usize) -> Result<(), u32> {
        sys::set_fs(base as u64).map_err(code)
    }

    /// The calling thread's control block, which its FS base and FS:0 hold: zero before the
    /// runtime gives it one.
    pub fn fs_base() -> usize {
        sys::rt::tls::current() as usize
    }

    /// Standard output and error: std's writer, lines of the task's log (system design 5B).
    pub fn write_out(bytes: &[u8]) -> usize {
        sys::rt::stdio::write(bytes)
    }

    /// The process's end, once std's writer has given the log its last line.
    pub fn exit(code: i32) -> ! {
        sys::rt::exit(code)
    }

    pub fn name() -> &'static str {
        sys::start::process_name()
    }

    pub fn abi_version() -> u64 {
        sys::abi_version()
    }

    /// `thread_yield` (ABI 2.8).
    pub fn yield_now() {
        sys::rt::thread::yield_now();
    }

    /// Starts a thread on the one thread path, std's and the C library's (system design 5B): a
    /// stack of `stack` bytes at least with a guard page, and the thread's block of TLS with its
    /// control block, installed before `main(arg)` runs. Its runtime handle, boxed, as an
    /// address, which [`join`] or [`detach`] or [`detach_releasing`] takes once.
    pub fn spawn(stack: usize, main: extern "C" fn(usize), arg: usize) -> Result<usize, u32> {
        sys::rt::thread::spawn(stack, main, arg)
            .map(|thread| Box::into_raw(Box::new(thread)).expose_provenance())
            .map_err(code)
    }

    /// Takes back the handle [`spawn`] gave.
    ///
    /// # Safety
    ///
    /// `thread` is a handle `spawn` gave, taken once.
    unsafe fn handle(thread: usize) -> Box<sys::rt::thread::Thread> {
        // SAFETY: the caller's contract.
        unsafe { Box::from_raw(core::ptr::with_exposed_provenance_mut(thread)) }
    }

    /// Waits for the kernel to say the thread has ended, then unmaps its stack.
    ///
    /// # Safety
    ///
    /// `thread` is a handle [`spawn`] gave, taken once.
    pub unsafe fn join(thread: usize) {
        // SAFETY: the caller's contract.
        unsafe { handle(thread) }.join()
    }

    /// Detaches the thread: its stack is unmapped once the kernel says it has ended. The thread
    /// frees its own block at its end.
    ///
    /// # Safety
    ///
    /// `thread` is a handle [`spawn`] gave, taken once.
    pub unsafe fn detach(thread: usize) {
        // SAFETY: the caller's contract.
        drop(unsafe { handle(thread) })
    }

    /// Detaches a thread that ended with its block `control` kept: the block and the stack are
    /// freed once the kernel says it has ended.
    ///
    /// # Safety
    ///
    /// `thread` is a handle [`spawn`] gave, taken once, and `control` that thread's block, which
    /// nothing else frees.
    pub unsafe fn detach_releasing(thread: usize, control: usize) {
        let control = core::ptr::with_exposed_provenance_mut(control);
        // SAFETY: the caller's contract.
        unsafe { handle(thread).detach_releasing(control) }
    }

    /// Sets the C library's part of every thread's end (`rt::thread::end_current`).
    pub fn at_thread_end(end: unsafe extern "C" fn()) {
        sys::rt::thread::at_thread_end(sys::rt::thread::Runtime::Libc, end)
    }

    /// The calling thread's end: the C library's part, then std's.
    ///
    /// # Safety
    ///
    /// Nothing reads the values of the thread's thread-locals after.
    pub unsafe fn end_current() {
        // SAFETY: the caller's contract.
        unsafe { sys::rt::thread::end_current() }
    }

    /// Ends the calling thread, its end already run, and keeps its block, which its
    /// `pthread_t` lies in, for the join or the detach that frees it ([`release`]).
    pub fn thread_exit() -> ! {
        sys::thread_exit()
    }

    /// Frees a thread's block: one that ended, or the caller's own.
    ///
    /// # Safety
    ///
    /// `control` is a block the thread path made, for a thread that has ended or is the caller,
    /// which reads none of its thread-locals after.
    pub unsafe fn release(control: usize) {
        // SAFETY: the caller's contract.
        unsafe { sys::rt::tls::release(control as *mut sys::rt::tls::Control) }
    }

    /// Where the bytes the control block keeps for the C library end: a C library's own
    /// per-thread state fits below it.
    pub const LIBC_END: usize =
        core::mem::offset_of!(sys::rt::tls::Control, libc) + sys::rt::tls::LIBC_AREA;
}

/// Stand-ins over the host, for tests: memory from the host's allocator, its clock, a futex
/// that polls, randomness from a counter, and standard output into a buffer the test reads.
#[cfg(not(target_os = "moruna"))]
mod host {
    extern crate std;

    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::cell::{Cell, RefCell};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use std::vec::Vec;

    use super::Access;

    const PAGE: usize = 4096;
    /// The kernel's codes the stand-ins answer with.
    const NO_MEMORY: u32 = 5;
    const INVALID_ARGS: u32 = 4;
    const SHOULD_WAIT: u32 = 6;
    const TIMED_OUT: u32 = 8;

    /// Every mapping a test made: address, length and access, as the core keeps them.
    static MAPPINGS: Mutex<Vec<(usize, usize, Access)>> = Mutex::new(Vec::new());

    /// The most a stand-in mapping may be: past it the "kernel" has no memory.
    pub const MAP_MAX: usize = 1 << 30;

    pub fn map(len: usize, access: Access) -> Result<usize, u32> {
        if len == 0 || len > MAP_MAX {
            return Err(NO_MEMORY);
        }
        let layout = Layout::from_size_align(len, PAGE).map_err(|_| INVALID_ARGS)?;
        // SAFETY: the layout's size is not zero.
        let p = unsafe { alloc_zeroed(layout) };
        if p.is_null() {
            return Err(NO_MEMORY);
        }
        let mut m = MAPPINGS.lock().unwrap();
        m.push((p as usize, len, access));
        Ok(p as usize)
    }

    /// The access of the page at `addr`, if a stand-in mapping holds it.
    #[cfg(test)]
    pub fn access_at(addr: usize) -> Option<Access> {
        let m = MAPPINGS.lock().unwrap();
        m.iter()
            .find(|(a, l, _)| (*a..a + l).contains(&addr))
            .map(|(_, _, access)| *access)
    }

    /// # Safety
    ///
    /// As the core's: the range is a whole mapping a test made and no longer uses.
    pub unsafe fn unmap(addr: usize, len: usize) -> Result<(), u32> {
        let mut m = MAPPINGS.lock().unwrap();
        let Some(i) = m.iter().position(|(a, l, _)| *a == addr && *l == len) else {
            return Err(INVALID_ARGS);
        };
        m.remove(i);
        // SAFETY: allocated in `map` with this layout.
        unsafe {
            dealloc(
                addr as *mut u8,
                Layout::from_size_align_unchecked(len, PAGE),
            )
        };
        Ok(())
    }

    /// # Safety
    ///
    /// As the core's.
    pub unsafe fn protect(addr: usize, len: usize, access: Access) -> Result<(), u32> {
        let mut m = MAPPINGS.lock().unwrap();
        match m
            .iter_mut()
            .find(|(a, l, _)| addr >= *a && addr + len <= a + l)
        {
            Some(e) => {
                e.2 = access;
                Ok(())
            }
            None => Err(INVALID_ARGS),
        }
    }

    fn start() -> Instant {
        static START: Mutex<Option<Instant>> = Mutex::new(None);
        *START.lock().unwrap().get_or_insert_with(Instant::now)
    }

    pub fn monotonic_ns() -> u64 {
        // Never zero, as the core's is not once it has booted.
        start().elapsed().as_nanos() as u64 + 1
    }

    pub fn realtime_ns() -> u64 {
        monotonic_ns()
    }

    /// A futex that polls: `ShouldWait` when the word is not `expected`, then back once it
    /// changes (a wake, as far as a waiter can tell) or at the deadline.
    pub fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> Result<(), u32> {
        if word.load(Ordering::Acquire) != expected {
            return Err(SHOULD_WAIT);
        }
        loop {
            if monotonic_ns() >= deadline {
                return Err(TIMED_OUT);
            }
            std::thread::sleep(Duration::from_micros(200));
            if word.load(Ordering::Acquire) != expected {
                return Ok(());
            }
        }
    }

    /// The waiters see the word change; how many woke is not known here.
    pub fn futex_wake(_word: &AtomicU32, _count: u32) -> Result<u32, u32> {
        Ok(0)
    }

    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub fn random(buf: &mut [u8]) -> Result<(), u32> {
        for b in buf {
            *b = NEXT.fetch_add(1, Ordering::Relaxed) as u8;
        }
        Ok(())
    }

    std::thread_local! {
        static FS: Cell<usize> = const { Cell::new(0) };
        static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    pub fn set_fs(base: usize) -> Result<(), u32> {
        FS.with(|f| f.set(base));
        Ok(())
    }

    pub fn fs_base() -> usize {
        FS.with(Cell::get)
    }

    pub fn write_out(bytes: &[u8]) -> usize {
        OUT.with(|o| o.borrow_mut().extend_from_slice(bytes));
        bytes.len()
    }

    /// What this thread wrote to standard output and error, taken.
    #[cfg(test)]
    pub fn written() -> Vec<u8> {
        OUT.with(|o| core::mem::take(&mut *o.borrow_mut()))
    }

    /// A test cannot end its process: the end is a panic it can catch.
    pub fn exit(code: i32) -> ! {
        std::panic::panic_any(code)
    }

    pub fn yield_now() {
        std::thread::yield_now();
    }

    pub fn name() -> &'static str {
        "host"
    }

    pub fn abi_version() -> u64 {
        2 << 16 | 8
    }
}
