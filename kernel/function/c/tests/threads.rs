//! One thread path for both runtimes (system design 5B, item 5.2b): a thread `pthread_create`
//! started runs std's thread-locals' destructors at its end, whether its routine returns or
//! calls `pthread_exit`, and a std thread runs the C library's (a key's destructor). Joins and
//! detaches, before a thread's end and after it, free each thread once.
//!
//! A program of its own (`harness = false`): `cargo test` runs it on the host, against the
//! host's C library; build-core.sh links it for the core (`bin/threads`), against relibc, where
//! it runs as a booted task. It ends with `threads: ok`, or panics.

use std::cell::RefCell;
use std::ffi::{c_int, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

// The C library is linked with the SDK: relibc on the core.
use moruna_function as _;

// On the core, std's `_start` names the C library's start and end weakly, which takes no member
// of an archive: this program names them outright, as build-core.sh's link does for the C ones.
#[cfg(target_os = "moruna")]
unsafe extern "C" {
    fn moruna_libc_init();
    fn moruna_libc_exit(code: c_int) -> !;
}
#[cfg(target_os = "moruna")]
#[used]
static C_LIBRARY: (unsafe extern "C" fn(), unsafe extern "C" fn(c_int) -> !) =
    (moruna_libc_init, moruna_libc_exit);

/// `pthread_t`, a word on each C library this runs against.
type Pthread = usize;

unsafe extern "C" {
    fn pthread_create(
        thread: *mut Pthread,
        attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: Pthread, retval: *mut *mut c_void) -> c_int;
    fn pthread_detach(thread: Pthread) -> c_int;
    fn pthread_exit(retval: *mut c_void) -> !;
    // `pthread_key_t` is an `unsigned long` or narrower: written into a zeroed word, passed in a
    // register.
    fn pthread_key_create(
        key: *mut c_ulong,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn pthread_setspecific(key: c_ulong, value: *const c_void) -> c_int;
}

/// std thread-local values dropped, on any thread.
static DROPPED: AtomicUsize = AtomicUsize::new(0);

struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static HELD: RefCell<Option<Counted>> = const { RefCell::new(None) };
}

fn hold() {
    HELD.with(|held| *held.borrow_mut() = Some(Counted));
}

extern "C" fn holds_and_returns(_: *mut c_void) -> *mut c_void {
    hold();
    ptr::without_provenance_mut(7)
}

extern "C" fn holds_and_exits(_: *mut c_void) -> *mut c_void {
    hold();
    // SAFETY: nothing on this thread's stack refers to its thread-locals.
    unsafe { pthread_exit(ptr::without_provenance_mut(9)) }
}

/// Threads that have reached their routine's end.
static FINISHED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn holds_and_finishes(_: *mut c_void) -> *mut c_void {
    hold();
    FINISHED.fetch_add(1, Ordering::SeqCst);
    ptr::null_mut()
}

/// Key values destroyed, on any thread.
static DESTROYED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn destroy(value: *mut c_void) {
    assert_eq!(value.addr(), 5, "the value the thread set");
    DESTROYED.fetch_add(1, Ordering::SeqCst);
}

fn start(routine: extern "C" fn(*mut c_void) -> *mut c_void) -> Pthread {
    let mut thread: Pthread = 0;
    // SAFETY: a routine of the right type, default attributes, and a place for the handle.
    let created = unsafe { pthread_create(&mut thread, ptr::null(), routine, ptr::null_mut()) };
    assert_eq!(created, 0, "pthread_create");
    thread
}

fn join(thread: Pthread) -> usize {
    let mut retval = ptr::null_mut();
    // SAFETY: a joinable thread, joined once.
    assert_eq!(
        unsafe { pthread_join(thread, &mut retval) },
        0,
        "pthread_join"
    );
    retval.addr()
}

fn wait_until(what: &AtomicUsize, at_least: usize) {
    while what.load(Ordering::SeqCst) < at_least {
        std::thread::yield_now();
    }
}

fn main() {
    // A pthread's end runs std's destructors: its routine returning, or `pthread_exit`.
    assert_eq!(join(start(holds_and_returns)), 7);
    assert_eq!(
        DROPPED.load(Ordering::SeqCst),
        1,
        "after a routine's return"
    );
    assert_eq!(join(start(holds_and_exits)), 9);
    assert_eq!(DROPPED.load(Ordering::SeqCst), 2, "after pthread_exit");

    // A std thread's end runs the C library's: a key's destructor.
    let mut key: c_ulong = 0;
    // SAFETY: a place for the key, and a destructor of the right type.
    assert_eq!(unsafe { pthread_key_create(&mut key, Some(destroy)) }, 0);
    std::thread::spawn(move || {
        // SAFETY: a key this program made.
        assert_eq!(
            unsafe { pthread_setspecific(key, ptr::without_provenance(5)) },
            0
        );
        hold();
    })
    .join()
    .unwrap();
    assert_eq!(DESTROYED.load(Ordering::SeqCst), 1, "the key's destructor");
    assert_eq!(DROPPED.load(Ordering::SeqCst), 3, "and std's own");

    // A detach before the thread's end, and one after it: each thread ends once, its
    // destructors run once.
    let running = start(holds_and_finishes);
    // SAFETY: a joinable thread, detached once and not joined.
    assert_eq!(unsafe { pthread_detach(running) }, 0, "pthread_detach");
    let ending = start(holds_and_finishes);
    wait_until(&FINISHED, 2);
    // SAFETY: as above.
    assert_eq!(unsafe { pthread_detach(ending) }, 0, "pthread_detach");
    wait_until(&DROPPED, 5);

    // Many threads, each joined: blocks freed only as each thread has ended.
    let threads: Vec<Pthread> = (0..32).map(|_| start(holds_and_returns)).collect();
    for thread in threads {
        assert_eq!(join(thread), 7);
    }
    assert_eq!(DROPPED.load(Ordering::SeqCst), 37);
    println!("threads: ok");
}
