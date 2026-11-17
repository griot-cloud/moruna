//! Thread-local keys (std's `sys::thread_local::key`), as std's Motor and Xous layers have them:
//! std's `thread_local!` keeps each variable's value behind a key, and a key is an index into a
//! table each thread has. A thread's FS base points at its table, whose first word holds the
//! table's own address, so one load through FS finds it. The kernel keeps each thread's FS base
//! (`thread_set_fs`), and nothing else in a native program uses it.
//!
//! Every thread std runs has a table before its first key is read: [`super::start`] gives the
//! first thread one, and [`super::thread`] each thread it starts. Keys are never reused: a
//! process makes at most [`KEYS`], one for each `thread_local!` it touches.

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use super::os;

/// A thread-local key: an index into each thread's table.
pub type Key = usize;

/// What a key's value is given to when its thread ends.
pub type Dtor = unsafe extern "C" fn(*mut u8);

/// The words of a thread's table: its own address, then one per key (4 KiB).
const WORDS: usize = 512;

/// The most keys a process makes.
pub const KEYS: usize = WORDS - 1;

/// The rounds of destructors a thread's end runs: a destructor may set a key again, and the
/// next round runs that one. Five, as std's own key-based platforms do.
const ROUNDS: usize = 5;

/// The next key; key 0 is the table's own address, never a key.
static NEXT: AtomicUsize = AtomicUsize::new(1);
/// Each key's destructor, null for none.
static DTORS: [AtomicPtr<()>; WORDS] = [const { AtomicPtr::new(ptr::null_mut()) }; WORDS];

/// A thread's table.
#[repr(C, align(64))]
pub(crate) struct Table(UnsafeCell<[*mut u8; WORDS]>);

// SAFETY: a table is read and written only by the thread it is installed for (through its FS
// base), and by the thread that made it before that one starts.
unsafe impl Sync for Table {}
// SAFETY: as above: it moves to the thread it is for before that thread reads it.
unsafe impl Send for Table {}

impl Table {
    pub(crate) const fn new() -> Table {
        Table(UnsafeCell::new([ptr::null_mut(); WORDS]))
    }
}

/// The first thread's table.
static MAIN: Table = Table::new();

/// Points the calling thread's FS base at `table`, whose first word is made its own address.
pub(crate) fn install(table: &Table) {
    let words: *mut *mut u8 = table.0.get().cast();
    // SAFETY: the table is the calling thread's, and nothing else reads it yet.
    unsafe { *words = words.cast() };
    os::install(words);
}

/// Gives the first thread its table.
pub(crate) fn install_main() {
    install(&MAIN);
}

/// Leaves the calling thread with no table: its own is about to be freed.
pub(crate) fn uninstall() {
    os::install(ptr::null_mut());
}

/// `key`, if it is one a process may have: the table has a word for it.
fn checked(key: usize) -> Key {
    assert!(
        (1..WORDS).contains(&key),
        "a thread-local key out of the table: {key} (at most {KEYS} keys)"
    );
    key
}

/// A new key, whose values `dtor` is given at their thread's end.
pub fn create(dtor: Option<Dtor>) -> Key {
    let key = checked(NEXT.fetch_add(1, Ordering::Relaxed));
    if let Some(dtor) = dtor {
        DTORS[key].store(dtor as *mut (), Ordering::Release);
    }
    key
}

/// The calling thread's word for `key`.
fn slot(key: Key) -> *mut *mut u8 {
    // SAFETY: the calling thread's table has `WORDS` words, and `checked` keeps `key` inside.
    unsafe { os::table().add(checked(key)) }
}

/// Sets the calling thread's value of `key`.
///
/// # Safety
///
/// The calling thread has a table (every thread std runs does), and `key` came from [`create`].
pub unsafe fn set(key: Key, value: *mut u8) {
    // SAFETY: the caller's contract: this thread's table, which only this thread touches.
    unsafe { *slot(key) = value };
}

/// The calling thread's value of `key`; null until set.
///
/// # Safety
///
/// As for [`set`].
pub unsafe fn get(key: Key) -> *mut u8 {
    // SAFETY: as for `set`.
    unsafe { *slot(key) }
}

/// Gives a key back. Keys are never reused, so nothing is done: its word stays empty.
///
/// # Safety
///
/// None needed; std marks it unsafe.
pub unsafe fn destroy(_key: Key) {}

/// At the calling thread's end: gives each value it holds to its key's destructor, emptying the
/// key first, round after round until a round finds none, at most [`ROUNDS`].
pub(crate) fn run_dtors() {
    for _ in 0..ROUNDS {
        let mut ran = false;
        let made = NEXT.load(Ordering::Acquire).min(WORDS);
        for (key, dtor) in DTORS.iter().enumerate().take(made).skip(1) {
            let dtor = dtor.load(Ordering::Acquire);
            if dtor.is_null() {
                continue;
            }
            // SAFETY: the thread is ending in `thread::run`, which installed its table.
            let value = unsafe { get(key) };
            if value.is_null() {
                continue;
            }
            // SAFETY: as above.
            unsafe { set(key, ptr::null_mut()) };
            // SAFETY: `create` stored a `Dtor` here, and std's contract for it is that it
            // takes the value its key held.
            unsafe {
                let dtor: Dtor = core::mem::transmute::<*mut (), Dtor>(dtor);
                dtor(value);
            }
            ran = true;
        }
        if !ran {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::boxed::Box;
    use std::sync::atomic::AtomicU32;
    use std::thread;

    use super::*;

    static DROPPED: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn count(value: *mut u8) {
        assert!(!value.is_null());
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }

    /// Run on a fresh host thread with a table of its own, as `thread::run` gives one.
    fn on_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        thread::spawn(move || {
            let table = Box::new(Table::new());
            install(&table);
            let r = f();
            // A test may leave the thread with no table, as a thread's end does.
            if !os::table().is_null() {
                run_dtors();
                uninstall();
            }
            r
        })
        .join()
        .unwrap()
    }

    #[test]
    fn each_thread_has_its_own_value_of_a_key() {
        let key = create(None);
        assert!(key >= 1);
        on_thread(move || {
            // SAFETY: this thread has a table; the key is a created one.
            unsafe {
                assert!(get(key).is_null(), "a key is empty until set");
                set(key, ptr::without_provenance_mut(7));
                assert_eq!(get(key).addr(), 7);
            }
        });
        on_thread(move || {
            // SAFETY: as above.
            unsafe { assert!(get(key).is_null(), "another thread's value is its own") };
        });
        unsafe { destroy(key) };
    }

    #[test]
    fn the_table_s_first_word_is_its_own_address() {
        on_thread(|| {
            let table = os::table();
            // SAFETY: `install` wrote the word.
            assert_eq!(unsafe { *table }, table.cast());
        });
    }

    #[test]
    fn a_thread_s_end_gives_each_value_to_its_destructor_once() {
        let key = create(Some(count));
        let unset = create(Some(count));
        let plain = create(None);
        let before = DROPPED.load(Ordering::SeqCst);
        on_thread(move || {
            // SAFETY: as above.
            unsafe {
                set(key, ptr::without_provenance_mut(1));
                set(plain, ptr::without_provenance_mut(2));
            }
            let _ = unset;
        });
        // Only this thread's own value is given: `unset` held none, `plain` has no destructor.
        assert_eq!(DROPPED.load(Ordering::SeqCst), before + 1);
    }

    static SECOND: AtomicUsize = AtomicUsize::new(0);
    static RAN_SECOND: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn second(_: *mut u8) {
        RAN_SECOND.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn sets_another(_: *mut u8) {
        // A destructor that touches a thread-local whose key it sets: the next round runs it.
        unsafe {
            set(
                SECOND.load(Ordering::SeqCst),
                ptr::without_provenance_mut(3),
            )
        };
    }

    #[test]
    fn a_value_a_destructor_sets_is_given_to_the_next_round() {
        let first = create(Some(sets_another));
        SECOND.store(create(Some(second)), Ordering::SeqCst);
        on_thread(move || {
            // SAFETY: as above.
            unsafe { set(first, ptr::without_provenance_mut(1)) };
        });
        assert_eq!(RAN_SECOND.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_first_thread_gets_the_static_table() {
        on_thread(|| {
            install_main();
            assert_eq!(
                os::table().cast_const(),
                MAIN.0.get().cast::<*mut u8>().cast_const()
            );
            uninstall();
            assert!(os::table().is_null());
        });
    }

    #[test]
    #[should_panic(expected = "out of the table")]
    fn a_key_past_the_table_is_refused() {
        checked(WORDS);
    }

    #[test]
    #[should_panic(expected = "out of the table")]
    fn key_zero_is_the_table_s_own_word() {
        checked(0);
    }
}
