//! Relibc Threads, or RLCT.

use core::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc::collections::BTreeMap;
#[cfg(target_os = "redox")]
use redox_rt::{
    RtTcb,
    arch::{STACK_SIZE, STACK_TOP},
};

use crate::{
    error::Errno,
    header::{errno::*, pthread as header, sched::sched_param, sys_mman},
    ld_so::tcb::Tcb,
    platform::{
        Pal, Sys,
        types::{c_int, c_void, clockid_t, pthread_t},
    },
};

use crate::sync::{Mutex, waitval::Waitval};

/// Called only by the main thread, as part of relibc_start.
pub unsafe fn init() {
    //TODO: what is the best way to get these values?
    #[cfg(target_os = "redox")]
    let (stack_base, stack_size) = ((STACK_TOP - STACK_SIZE) as *mut c_void, STACK_SIZE);
    #[cfg(any(target_os = "linux", target_os = "moruna"))]
    let (stack_base, stack_size) = (ptr::null_mut(), 0);

    let thread = Pthread {
        waitval: Waitval::new(),
        has_enabled_cancelation: AtomicBool::new(true),
        has_queued_cancelation: AtomicBool::new(false),
        has_async_cancelation: AtomicBool::new(false),
        flags: PthreadFlags::empty().bits().into(),

        //index: FIRST_THREAD_IDX,

        // TODO: set these values on Linux as well
        stack_base,
        stack_size,

        tcb_selfref: UnsafeCell::new(core::ptr::null_mut()),

        os_tid: UnsafeCell::new(Sys::current_os_tid()),
    };

    let current_tcb = unsafe { Tcb::current() }.expect("no TCB present for main thread");
    current_tcb.pthread = thread;
    unsafe {
        current_tcb.pthread.tcb_selfref.get().write(current_tcb);
    }
}

//static NEXT_INDEX: AtomicU32 = AtomicU32::new(FIRST_THREAD_IDX + 1);
//const FIRST_THREAD_IDX: usize = 1;

pub unsafe fn terminate_from_main_thread() {
    for tcb in OS_TID_TO_PTHREAD.lock().values() {
        let _ = unsafe { cancel(&(*tcb.0).pthread) };
    }
}

bitflags::bitflags! {
    pub struct PthreadFlags: usize {
        const DETACHED = 1;
        /// Moruna: the thread has reached its end joinable, its block kept, so a detach
        /// after it frees the block once the kernel says the thread has ended.
        const EXITED = 2;
    }
}

#[derive(Debug)]
pub struct Pthread {
    pub(crate) waitval: Waitval<Retval>,
    pub(crate) has_queued_cancelation: AtomicBool,
    pub(crate) has_enabled_cancelation: AtomicBool,
    pub(crate) has_async_cancelation: AtomicBool,
    pub(crate) flags: AtomicUsize,

    pub(crate) stack_base: *mut c_void,
    pub(crate) stack_size: usize,

    pub(crate) tcb_selfref: UnsafeCell<*mut Tcb>,

    pub os_tid: UnsafeCell<OsTid>,
}

#[derive(Clone, Copy, Debug, Default, Ord, Eq, PartialOrd, PartialEq)]
pub struct OsTid {
    #[cfg(target_os = "redox")]
    pub thread_fd: usize,
    #[cfg(any(target_os = "linux", target_os = "moruna"))]
    pub thread_id: usize,
}

unsafe impl Send for Pthread {}
unsafe impl Sync for Pthread {}

#[derive(Clone, Copy, Debug)]
pub struct Retval(pub *mut c_void);

struct MmapGuard {
    page_start: *mut c_void,
    mmap_size: usize,
}
impl Drop for MmapGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = Sys::munmap(self.page_start, self.mmap_size);
        }
    }
}

pub(crate) unsafe fn create(
    attrs: Option<&header::RlctAttr>,
    start_routine: extern "C" fn(arg: *mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> Result<pthread_t, Errno> {
    let attrs = attrs.cloned().unwrap_or_default();

    // Moruna: the runtime's one thread path, std's and the C library's (system design 5B).
    #[cfg(target_os = "moruna")]
    return unsafe { create_moruna(&attrs, start_routine, arg) };

    #[cfg(not(target_os = "redox"))]
    let current_sigmask = 0_u64;
    #[cfg(target_os = "redox")]
    let current_sigmask =
        redox_rt::signal::get_sigmask().expect("failed to obtain sigprocmask for caller");

    // Create a locked mutex, unlocked by the thread after it has started.
    let synchronization_mutex = unsafe { Mutex::locked(current_sigmask) };
    let synchronization_mutex = &synchronization_mutex;

    let stack_size = attrs.stacksize.next_multiple_of(Sys::getpagesize());

    let stack_base = if attrs.stack != 0 {
        attrs.stack as *mut c_void
    } else {
        let ret = unsafe {
            sys_mman::mmap(
                core::ptr::null_mut(),
                stack_size,
                sys_mman::PROT_READ | sys_mman::PROT_WRITE,
                sys_mman::MAP_PRIVATE | sys_mman::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ret as isize == -1 {
            // "Insufficient resources"
            return Err(Errno(EAGAIN));
        }
        ret
    };

    let mut flags = PthreadFlags::empty();
    match i32::from(attrs.detachstate) {
        header::PTHREAD_CREATE_DETACHED => flags |= PthreadFlags::DETACHED,
        header::PTHREAD_CREATE_JOINABLE => (),

        other => unreachable!("unknown detachstate {}", other),
    }

    let stack_raii = MmapGuard {
        page_start: stack_base,
        mmap_size: stack_size,
    };

    let current_tcb = unsafe { Tcb::current() }.expect("no TCB!");
    let new_tcb = unsafe { Tcb::new(current_tcb.tls_len) }.map_err(|_| Errno(ENOMEM))?;
    new_tcb.pthread.flags = flags.bits().into();
    new_tcb.pthread.stack_base = stack_base;
    new_tcb.pthread.stack_size = stack_size;

    new_tcb.masters_ptr = current_tcb.masters_ptr;
    new_tcb.masters_len = current_tcb.masters_len;
    new_tcb.linker_ptr = current_tcb.linker_ptr;

    let stack_end = unsafe { stack_base.add(stack_size) };
    let mut stack = stack_end.cast::<usize>();
    {
        let mut push = |value: usize| {
            stack = unsafe { stack.sub(1) };
            unsafe { stack.write(value) };
        };

        if cfg!(target_arch = "aarch64") {
            // Aarch64 requires the stack to be 16 byte aligned after
            // the call instruction, unlike x86 which requires it to be
            // aligned before the call instruction. As such push an
            // extra word on the stack to align the stack to 16 bytes.
            push(0);
        }
        push(0);
        push(0);
        push(ptr::from_ref(synchronization_mutex) as usize);
        push(ptr::from_mut(new_tcb) as usize);

        push(arg as usize);
        push(start_routine as usize);

        push(new_thread_shim as *const () as usize);
    }

    let Ok(os_tid) = (unsafe { Sys::rlct_clone(stack, &mut new_tcb.os_specific) }) else {
        return Err(Errno(EAGAIN));
    };
    core::mem::forget(stack_raii);

    let _ = synchronization_mutex.lock();

    OS_TID_TO_PTHREAD
        .lock()
        .insert(os_tid, ForceSendSync(new_tcb));

    Ok(&raw const new_tcb.pthread as *mut _)
}

/// A shim to wrap thread entry points in logic to set up TLS, for example
unsafe extern "C" fn new_thread_shim(
    entry_point: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    tcb: *mut Tcb,
    synchronization_mutex: *const Mutex<u64>,
) -> ! {
    let tcb = unsafe { tcb.as_mut() }.expect("non-null TLS is required");

    #[cfg(not(target_os = "redox"))]
    {
        unsafe { tcb.activate() };
    }
    #[cfg(target_os = "redox")]
    {
        // `thr_fd` in `tcb` is set by [`Sys::rlct_clone`] *before* jumping to
        // the entry point of the new thread.
        unsafe {
            tcb.activate(None);
        }
        redox_rt::signal::setup_sighandler(&tcb.os_specific, false);
    }

    #[cfg(target_os = "redox")]
    let procmask = unsafe { (&*synchronization_mutex).as_ptr().read() };

    unsafe { tcb.copy_masters() }.unwrap();

    unsafe { tcb.pthread.os_tid.get().write(Sys::current_os_tid()) };

    unsafe { (&*synchronization_mutex).manual_unlock() };

    #[cfg(target_os = "redox")]
    {
        redox_rt::signal::set_sigmask(Some(procmask), None)
            .expect("failed to set procmask in child thread");
    }

    let retval = unsafe { entry_point(arg) };

    unsafe { exit_current_thread(Retval(retval)) }
}
pub unsafe fn join(thread: &Pthread) -> Result<Retval, Errno> {
    // We don't have to return EDEADLK, but unlike e.g. pthread_t lifetime checking, it's a
    // relatively easy check.
    if core::ptr::eq(
        thread,
        current_thread().expect("current thread not present"),
    ) {
        return Err(Errno(EDEADLK));
    }

    // Waitval starts locked, and is unlocked when the thread finishes.
    let retval = *thread.waitval.wait();

    // Moruna: the waitval is posted by the ending thread while it still runs. Its block is
    // freed only once the kernel says the thread has ended (system design 5B).
    #[cfg(target_os = "moruna")]
    if let Some(tcb) = unsafe { thread.tcb_selfref.get().read().as_ref() } {
        let handle = tcb.os_specific.take_handle();
        if handle != 0 {
            unsafe { relibc_moruna::thread::join(handle) };
        }
    }

    // We have now awaited the thread and received its return value. POSIX states that the
    // pthread_t of this thread, will no longer be valid. In practice, we can thus deallocate the
    // thread state.

    unsafe { dealloc_thread(thread) };

    Ok(retval)
}

pub unsafe fn detach(thread: &Pthread) -> Result<(), Errno> {
    #[cfg(target_os = "moruna")]
    return unsafe { detach_moruna(thread) };

    #[cfg(not(target_os = "moruna"))]
    {
        thread
            .flags
            .fetch_or(PthreadFlags::DETACHED.bits(), Ordering::AcqRel);
        Ok(())
    }
}

/// Moruna: a detach. A thread still running frees its own block at its end; one that has
/// reached its end joinable, its block kept, has its block freed by the runtime with its stack,
/// once the kernel says it has ended.
#[cfg(target_os = "moruna")]
unsafe fn detach_moruna(thread: &Pthread) -> Result<(), Errno> {
    let tcb = unsafe { thread.tcb_selfref.get().read() };
    // Taken before the flag is set: a thread that sees the flag may free its block at once.
    let handle = match unsafe { tcb.as_ref() } {
        Some(tcb) => tcb.os_specific.take_handle(),
        None => 0,
    };
    let before = thread
        .flags
        .fetch_or(PthreadFlags::DETACHED.bits(), Ordering::AcqRel);
    if before & PthreadFlags::EXITED.bits() != 0 {
        // Its block is kept, and is this detach's to free.
        unsafe {
            OS_TID_TO_PTHREAD.lock().remove(&thread.os_tid.get().read());
        }
        if handle != 0 {
            unsafe { relibc_moruna::thread::detach_releasing(handle, tcb as usize) };
        }
    } else if handle != 0 {
        // `thread` may be freed from here on: not read again.
        unsafe { relibc_moruna::thread::detach(handle) };
    }
    Ok(())
}

pub fn current_thread() -> Option<&'static Pthread> {
    unsafe { Tcb::current().map(|p| &p.pthread) }
}

pub unsafe fn testcancel() {
    let this_thread = current_thread().expect("current thread not present");

    if this_thread.has_queued_cancelation.load(Ordering::Acquire)
        && this_thread.has_enabled_cancelation.load(Ordering::Acquire)
    {
        unsafe { cancel_current_thread() };
    }
}

pub unsafe fn exit_current_thread(retval: Retval) -> ! {
    // Moruna: the thread's end is the runtime's one (system design 5B): relibc's part
    // (`moruna_thread_end`, below), then std's thread-locals' destructors.
    #[cfg(target_os = "moruna")]
    {
        unsafe { relibc_moruna::thread::end_current() };
    }

    #[cfg(not(target_os = "moruna"))]
    {
        // Run pthread_cleanup_push/pthread_cleanup_pop destructors.
        unsafe { header::run_destructor_stack() };

        unsafe { header::tls::run_all_destructors() };
    }

    let this = current_thread().expect("failed to obtain current thread when exiting");
    let stack_base = this.stack_base;
    let stack_size = this.stack_size;

    // dealloc_thread() or waitval.post() might unmaps the tcb so extract the thread_fd now
    #[cfg(target_os = "redox")]
    let status_fd = {
        let _guard = redox_rt::signal::tmp_disable_signals();
        let thread_fd = RtTcb::current().thread_fd();
        thread_fd.dup_into_upper(b"status").unwrap()
    };

    // Moruna: said once, so a detach either finds the thread still running (and the thread then
    // frees its own block) or finds it at its end joinable, its block the detach's to free.
    #[cfg(target_os = "moruna")]
    let flags = this
        .flags
        .fetch_or(PthreadFlags::EXITED.bits(), Ordering::AcqRel);
    #[cfg(not(target_os = "moruna"))]
    let flags = this.flags.load(Ordering::Acquire);
    if flags & PthreadFlags::DETACHED.bits() != 0 {
        // When detached, the thread state no longer makes any sense, and can immediately be
        // deallocated.
        unsafe { dealloc_thread(this) };
    } else {
        // When joinable, the return value should be made available to other threads.
        unsafe { this.waitval.post(retval) };
    }

    #[cfg(target_os = "redox")]
    unsafe {
        redox_rt::thread::exit_this_thread_inner(stack_base.cast(), stack_size, status_fd)
    }
    unsafe { Sys::exit_thread(stack_base.cast(), stack_size) }
}

unsafe fn dealloc_thread(thread: &Pthread) {
    // TODO: How should this be handled on Linux?
    unsafe {
        OS_TID_TO_PTHREAD.lock().remove(&thread.os_tid.get().read());
    }
    #[cfg(target_os = "redox")]
    unsafe {
        let tcb = thread.tcb_selfref.get().read();
        if !tcb.is_null() {
            loop {
                match syscall::funmap(tcb as usize, syscall::PAGE_SIZE) {
                    Ok(_) => break,
                    // might be not guarded by tmp_disable_signals()
                    Err(syscall::Error { errno: EINTR }) => continue,
                    Err(e) => log::warn!("Cannot dealloc_thread: {e:?}"),
                }
            }
        }
    }
    // Moruna: the thread's block, kept past its end since its pthread_t lies in it, is freed
    // now: by the join, once the kernel has said the thread ended, or by a detached thread at
    // its own end, which reads no thread-local after (system design 5B). Every block, the first
    // thread's too, is the runtime's to free.
    #[cfg(target_os = "moruna")]
    unsafe {
        let tcb = thread.tcb_selfref.get().read();
        if !tcb.is_null() {
            relibc_moruna::thread::release(tcb as usize);
        }
    }
}

/// Moruna: what a thread `create_moruna` starts is given.
#[cfg(target_os = "moruna")]
struct MorunaStart {
    routine: extern "C" fn(arg: *mut c_void) -> *mut c_void,
    arg: *mut c_void,
    flags: PthreadFlags,
    stack_size: usize,
    /// The new thread's Tcb, which its creator reads once `mutex` is unlocked.
    created: *const core::sync::atomic::AtomicPtr<Tcb>,
    mutex: *const Mutex<u64>,
}

/// Moruna: a thread on the runtime's one thread path (system design 5B). The runtime maps its
/// stack and makes its block of TLS and control block; the thread makes the control block its
/// Tcb, says where it is, and runs `start_routine`. A stack the caller gives is not taken.
#[cfg(target_os = "moruna")]
unsafe fn create_moruna(
    attrs: &header::RlctAttr,
    start_routine: extern "C" fn(arg: *mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> Result<pthread_t, Errno> {
    use core::sync::atomic::AtomicPtr;

    if attrs.stack != 0 {
        return Err(Errno(EINVAL));
    }
    let mut flags = PthreadFlags::empty();
    match i32::from(attrs.detachstate) {
        header::PTHREAD_CREATE_DETACHED => flags |= PthreadFlags::DETACHED,
        header::PTHREAD_CREATE_JOINABLE => (),
        other => unreachable!("unknown detachstate {}", other),
    }
    let detached = flags.contains(PthreadFlags::DETACHED);
    let synchronization_mutex = unsafe { Mutex::locked(0) };
    let created = AtomicPtr::<Tcb>::new(ptr::null_mut());
    let start = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(MorunaStart {
        routine: start_routine,
        arg,
        flags,
        stack_size: attrs.stacksize,
        created: &raw const created,
        mutex: &raw const synchronization_mutex,
    }));
    let spawned =
        relibc_moruna::thread::spawn(attrs.stacksize, moruna_thread_start, start as usize);
    let Ok(thread) = spawned else {
        // SAFETY: no thread started, so the start is still this function's.
        drop(unsafe { alloc::boxed::Box::from_raw(start) });
        return Err(Errno(EAGAIN));
    };
    let _ = synchronization_mutex.lock();
    // The new thread stored its Tcb, and put itself in OS_TID_TO_PTHREAD, before it unlocked
    // the mutex.
    let new_tcb = created.load(Ordering::Acquire);
    // Its address only: a detached thread may have ended, its block freed, by now.
    let pthread = new_tcb
        .cast::<u8>()
        .wrapping_add(core::mem::offset_of!(Tcb, pthread))
        .cast::<Pthread>();
    if detached {
        // It frees its own block at its end; its stack goes once the kernel says it has ended.
        unsafe { relibc_moruna::thread::detach(thread) };
    } else {
        // A joinable thread keeps its block until its join or its detach, which take the handle
        // and free the block once the kernel says it has ended. It waits for the handle before
        // its routine runs.
        unsafe { (*new_tcb).generic.os_specific.give_handle(thread) };
    }
    Ok(pthread as pthread_t)
}

/// Moruna: a new thread's first code, its block installed by the runtime.
#[cfg(target_os = "moruna")]
extern "C" fn moruna_thread_start(start: usize) {
    // SAFETY: the start `create_moruna` boxed for this thread, given once.
    let start = unsafe { alloc::boxed::Box::from_raw(start as *mut MorunaStart) };
    let tcb = unsafe { Tcb::adopt_current() };
    tcb.pthread.flags = start.flags.bits().into();
    tcb.pthread.stack_size = start.stack_size;
    let os_tid = Sys::current_os_tid();
    unsafe { tcb.pthread.os_tid.get().write(os_tid) };
    let tcb: *mut Tcb = tcb;
    OS_TID_TO_PTHREAD.lock().insert(os_tid, ForceSendSync(tcb));
    // SAFETY: the creator waits on the mutex, so both still live; neither is read after.
    unsafe {
        (*start.created).store(tcb, Ordering::Release);
        (*start.mutex).manual_unlock();
    }
    if !start.flags.contains(PthreadFlags::DETACHED) {
        // SAFETY: the thread's own Tcb, joinable: kept until its join or its detach.
        unsafe { (*tcb).generic.os_specific.wait_for_handle() };
    }
    let retval = (start.routine)(start.arg);
    unsafe { exit_current_thread(Retval(retval)) }
}

/// Moruna: relibc's part of every thread's end, std's threads' and its own, which the runtime
/// runs before std's (`moruna_sys::rt::thread::end_current`): the cleanup handlers and the
/// `thread_local` destructors (`__cxa_thread_atexit_impl`), then the keys' destructors.
#[cfg(target_os = "moruna")]
pub(crate) unsafe extern "C" fn moruna_thread_end() {
    unsafe { header::run_destructor_stack() };
    unsafe { header::tls::run_all_destructors() };
}

pub const SIGRT_RLCT_CANCEL: usize = 33;
pub const SIGRT_RLCT_TIMER: usize = 34;

unsafe extern "C" fn cancel_sighandler(_: c_int) {
    unsafe { cancel_current_thread() };
}
unsafe fn cancel_current_thread() {
    // Terminate the thread
    unsafe { exit_current_thread(Retval(header::PTHREAD_CANCELED)) };
}

pub unsafe fn cancel(thread: &Pthread) -> Result<(), Errno> {
    // TODO: What order should these atomic bools be accessed in?
    thread.has_queued_cancelation.store(true, Ordering::Release);

    if thread.has_enabled_cancelation.load(Ordering::Acquire)
        && thread.has_async_cancelation.load(Ordering::Acquire)
    {
        (unsafe { Sys::rlct_kill(thread.os_tid.get().read(), SIGRT_RLCT_CANCEL) })?;
    }

    Ok(())
}

pub fn set_sched_param(
    _thread: &Pthread,
    _policy: c_int,
    _param: &sched_param,
) -> Result<(), Errno> {
    // TODO
    Ok(())
}
pub fn set_sched_priority(_thread: &Pthread, _prio: c_int) -> Result<(), Errno> {
    // TODO
    Ok(())
}
pub fn set_cancel_state(state: c_int) -> Result<c_int, Errno> {
    let this_thread = current_thread().expect("current thread not present");

    let was_cancelable = match state {
        header::PTHREAD_CANCEL_ENABLE => {
            let old = this_thread
                .has_enabled_cancelation
                .swap(true, Ordering::Release);

            if this_thread.has_queued_cancelation.load(Ordering::Acquire) {
                unsafe {
                    cancel_current_thread();
                }
            }
            old
        }
        header::PTHREAD_CANCEL_DISABLE => this_thread
            .has_enabled_cancelation
            .swap(false, Ordering::Release),

        _ => return Err(Errno(EINVAL)),
    };

    Ok(match was_cancelable {
        true => header::PTHREAD_CANCEL_ENABLE,
        false => header::PTHREAD_CANCEL_DISABLE,
    })
}
pub fn set_cancel_type(ty: c_int) -> Result<c_int, Errno> {
    let this_thread = current_thread().expect("current thread not present");

    let new_async = match ty {
        header::PTHREAD_CANCEL_DEFERRED => false,
        header::PTHREAD_CANCEL_ASYNCHRONOUS => true,

        _ => return Err(Errno(EINVAL)),
    };

    let was_async = this_thread
        .has_async_cancelation
        .swap(new_async, Ordering::AcqRel);

    Ok(match was_async {
        true => header::PTHREAD_CANCEL_ASYNCHRONOUS,
        false => header::PTHREAD_CANCEL_DEFERRED,
    })
}
#[expect(unused_variables, reason = "function not yet fully implemented")]
pub fn get_cpu_clkid(thread: &Pthread) -> Result<clockid_t, Errno> {
    // TODO
    Err(Errno(ENOENT))
}
#[expect(unused_variables, reason = "function not yet fully implemented")]
pub fn get_sched_param(thread: &Pthread) -> Result<(clockid_t, sched_param), Errno> {
    // TODO should be possible to return sched_param
    Err(Errno(ENOSYS))
}

// TODO: Hash map?
// TODO: RwLock to improve perf?
static OS_TID_TO_PTHREAD: Mutex<BTreeMap<OsTid, ForceSendSync<*mut Tcb>>> =
    Mutex::new(BTreeMap::new());

#[derive(Clone, Copy)]
struct ForceSendSync<T>(T);
unsafe impl<T> Send for ForceSendSync<T> {}
unsafe impl<T> Sync for ForceSendSync<T> {}

/*pub(crate) fn current_thread_index() -> u32 {
    current_thread().expect("current thread not present").index
}*/

#[derive(Clone, Copy, Default, Debug)]
pub enum Pshared {
    #[default]
    Private,

    Shared,
}
impl Pshared {
    pub const fn from_raw(raw: c_int) -> Option<Self> {
        Some(match raw {
            header::PTHREAD_PROCESS_PRIVATE => Self::Private,
            header::PTHREAD_PROCESS_SHARED => Self::Shared,

            _ => return None,
        })
    }
    pub const fn raw(self) -> c_int {
        match self {
            Self::Private => header::PTHREAD_PROCESS_PRIVATE,
            Self::Shared => header::PTHREAD_PROCESS_SHARED,
        }
    }
}
