// SPDX-License-Identifier: MPL-2.0

//! Moruna's core under the personality, through `moruna-sys`.
//!
//! Every thread the personality runs on has a stack of `STACK_SIZE` bytes aligned to its
//! size; the word at its base names the thread's task, so finding the current task takes
//! no call and no TLS (the program owns FS).
//!
//! The program's memory is reached through a fault-tolerant copy. Once the core resumes
//! a fault in the copy routines at their recovery point (`set_user_copy`), copies run in
//! place; until then each one is a `process_read` or `process_write` on the personality's
//! own process, which answers `Fault` safely.

use alloc::{boxed::Box, vec::Vec};
use core::{
    arch::global_asm,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};

use moruna_sys::{DEADLINE_INFINITE, Error, RawHandle, Thread, raw};

use super::{Dir, FOREVER};

/// A personality thread's stack: its size, and its alignment.
pub(crate) const STACK_SIZE: usize = 256 * 1024;

pub(crate) fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> bool {
    let deadline = if deadline == FOREVER {
        DEADLINE_INFINITE
    } else {
        deadline
    };
    !matches!(
        moruna_sys::futex_wait(word, expected, deadline),
        Err(Error::TimedOut)
    )
}

pub(crate) fn futex_wake(word: &AtomicU32, count: u32) -> u32 {
    moruna_sys::futex_wake(word, count).unwrap_or(0)
}

pub(crate) fn now() -> u64 {
    moruna_sys::now()
}

pub(crate) fn yield_now() {
    core::hint::spin_loop();
}

/// The stack base of the calling thread.
fn stack_base() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer and touches nothing.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack)) };
    sp & !(STACK_SIZE - 1)
}

pub(crate) fn current_slot() -> usize {
    // SAFETY: every personality thread runs on a stack made by `new_stack`, whose first
    // word is the slot, written before the thread runs.
    unsafe { *(stack_base() as *const usize) }
}

pub(crate) fn set_current_slot(value: usize) {
    // SAFETY: as above; only the thread itself writes its slot.
    unsafe { *(stack_base() as *mut usize) = value };
}

/// A stack for a personality thread: its aligned base and its top. It is never freed
/// while the personality runs.
pub(crate) fn new_stack() -> Option<(usize, usize)> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(2 * STACK_SIZE).ok()?;
    let start = v.as_mut_ptr() as usize;
    core::mem::forget(v);
    let base = (start + STACK_SIZE - 1) & !(STACK_SIZE - 1);
    // SAFETY: the slot lies inside the reservation just leaked.
    unsafe { *(base as *mut usize) = 0 };
    Some((base, base + STACK_SIZE))
}

extern "C" fn thread_start(arg: u64) -> ! {
    // SAFETY: `spawn` leaked this box for the new thread, which takes it once.
    let f = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) };
    f();
    moruna_sys::thread_exit()
}

pub(crate) fn spawn(f: Box<dyn FnOnce() + Send>) -> Result<(), ()> {
    let (_, top) = new_stack().ok_or(())?;
    let arg = Box::into_raw(Box::new(f)) as u64;
    match Thread::spawn(thread_start, top as u64, arg) {
        Ok(thread) => {
            core::mem::forget(thread);
            Ok(())
        }
        Err(_) => {
            // SAFETY: the thread did not start, so the box is still ours.
            drop(unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) });
            Err(())
        }
    }
}

pub(crate) fn debug_write(bytes: &[u8]) {
    if let Some(console) = moruna_sys::start::peek("console") {
        let _ = raw::debug_write(console, bytes);
    }
}

pub(crate) fn random(buf: &mut [u8]) {
    let _ = moruna_sys::random(buf);
}

pub(crate) fn num_cpus() -> usize {
    moruna_sys::start::cpus().max(1)
}

pub(crate) fn exit(code: u32) -> ! {
    moruna_sys::exit(code as i64)
}

/// Whether the core resumes faults in the copy routines at their recovery point.
static IN_PLACE: AtomicBool = AtomicBool::new(false);
/// The personality's own process, for copies before that.
static SELF: AtomicUsize = AtomicUsize::new(0);

/// Copies run in place from now on: the core resumes a fault in
/// `[user_copy_range().0, .1)` at `user_copy_range().2` with the registers as they were.
pub fn set_user_copy() {
    IN_PLACE.store(true, Ordering::Release);
}

/// Copies go through `process_read` and `process_write` on `process`.
pub fn set_self_process(process: RawHandle) {
    SELF.store(process as usize, Ordering::Release);
}

/// The copy routines' range and their recovery point, for the core.
pub fn user_copy_range() -> (usize, usize, usize) {
    unsafe extern "C" {
        static linuxlayer_user_start: u8;
        static linuxlayer_user_end: u8;
        static linuxlayer_user_fixup: u8;
    }
    (
        &raw const linuxlayer_user_start as usize,
        &raw const linuxlayer_user_end as usize,
        &raw const linuxlayer_user_fixup as usize,
    )
}

// Each routine leaves in rcx how much is left to do and ends at the shared epilogue,
// which returns it; a fault resumes at the epilogue with rcx as the fault left it, which
// `rep movsb` and `rep stosb` keep exact.
global_asm!(
    ".pushsection .text.linuxlayer_user, \"ax\"",
    ".global linuxlayer_user_start",
    ".global linuxlayer_user_end",
    ".global linuxlayer_user_fixup",
    "linuxlayer_user_start:",
    // copy(dst = rdi, src = rsi, len = rdx) -> left
    "linuxlayer_copy:",
    "mov rcx, rdx",
    "rep movsb",
    "jmp linuxlayer_user_fixup",
    // set(dst = rdi, value = sil, len = rdx) -> left
    "linuxlayer_set:",
    "mov rcx, rdx",
    "mov eax, esi",
    "rep stosb",
    "jmp linuxlayer_user_fixup",
    // load(ptr = rdi, out = rsi) -> 0, or 4 on a fault
    "linuxlayer_load32:",
    "mov ecx, 4",
    "mov eax, dword ptr [rdi]",
    "mov dword ptr [rsi], eax",
    "xor ecx, ecx",
    "jmp linuxlayer_user_fixup",
    // cmpxchg(ptr = rdi, old = esi, new = edx, out = rcx) -> 0, or 4 on a fault
    "linuxlayer_cmpxchg32:",
    "mov r8, rcx",
    "mov ecx, 4",
    "mov eax, esi",
    "lock cmpxchg dword ptr [rdi], edx",
    "mov dword ptr [r8], eax",
    "xor ecx, ecx",
    "linuxlayer_user_end:",
    "linuxlayer_user_fixup:",
    "mov rax, rcx",
    "ret",
    ".popsection",
);

unsafe extern "C" {
    fn linuxlayer_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn linuxlayer_set(dst: *mut u8, value: u32, len: usize) -> usize;
    fn linuxlayer_load32(ptr: *const u32, out: *mut u32) -> usize;
    fn linuxlayer_cmpxchg32(ptr: *mut u32, old: u32, new: u32, out: *mut u32) -> usize;
}

fn self_process() -> Option<RawHandle> {
    match SELF.load(Ordering::Acquire) {
        0 => None,
        v => Some(v as RawHandle),
    }
}

/// A copy through the core, the largest prefix that succeeds, a page at a time once one
/// fails; returns how many bytes were not copied.
fn copy_through_core(dst: usize, src: usize, len: usize, to_program: bool) -> usize {
    let Some(process) = self_process() else {
        return len;
    };
    let call = |d: usize, s: usize, n: usize| {
        // SAFETY: the personality's side of the copy is its own memory, valid for `n`.
        let ok = unsafe {
            if to_program {
                let data = core::slice::from_raw_parts(s as *const u8, n);
                raw::process_write(process, d as u64, data)
            } else {
                let buf = core::slice::from_raw_parts_mut(d as *mut u8, n);
                raw::process_read(process, s as u64, buf)
            }
        };
        Error::from_raw(ok).is_ok()
    };
    if call(dst, src, len) {
        return 0;
    }
    let mut done = 0;
    while done < len {
        let program = if to_program { dst } else { src } + done;
        let n = (4096 - program % 4096).min(len - done);
        if !call(dst + done, src + done, n) {
            break;
        }
        done += n;
    }
    len - done
}

/// Copies `len` bytes, one side or both the program's memory; returns how many were not
/// copied.
///
/// # Safety
///
/// The personality's side must be valid for `len` bytes.
pub(crate) unsafe fn copy_fallible(dst: *mut u8, src: *const u8, len: usize, dir: Dir) -> usize {
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: a fault in the routine resumes at its recovery point.
        return unsafe { linuxlayer_copy(dst, src, len) };
    }
    match dir {
        Dir::ToProgram => copy_through_core(dst as usize, src as usize, len, true),
        Dir::FromProgram => copy_through_core(dst as usize, src as usize, len, false),
        Dir::Both => {
            let mut page = [0u8; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(4096);
                let p = page.as_mut_ptr() as usize;
                let got = n - copy_through_core(p, src as usize + done, n, false);
                let put = got - copy_through_core(dst as usize + done, p, got, true);
                done += put;
                if put != n {
                    break;
                }
            }
            len - done
        }
    }
}

/// Fills `len` bytes of the program's memory with `value`; returns how many were not
/// filled.
///
/// # Safety
///
/// None beyond the copy's.
pub(crate) unsafe fn set_fallible(dst: *mut u8, value: u8, len: usize) -> usize {
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: as for `copy_fallible`.
        return unsafe { linuxlayer_set(dst, value as u32, len) };
    }
    let page = [value; 4096];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(4096);
        let left = copy_through_core(dst as usize + done, page.as_ptr() as usize, n, true);
        done += n - left;
        if left != 0 {
            break;
        }
    }
    len - done
}

/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn load_u32_fallible(ptr: *const u32) -> Option<u32> {
    let mut out = 0u32;
    if IN_PLACE.load(Ordering::Acquire) {
        // SAFETY: as for `copy_fallible`.
        return (unsafe { linuxlayer_load32(ptr, &mut out) } == 0).then_some(out);
    }
    let left = copy_through_core((&raw mut out) as usize, ptr as usize, 4, false);
    (left == 0).then_some(out)
}

/// # Safety
///
/// `ptr` is aligned.
pub(crate) unsafe fn cmpxchg_u32_fallible(ptr: *mut u32, old: u32, new: u32) -> Option<u32> {
    if !IN_PLACE.load(Ordering::Acquire) {
        // Not atomic through the core: refused until copies run in place.
        return None;
    }
    let mut out = 0u32;
    // SAFETY: as for `copy_fallible`.
    (unsafe { linuxlayer_cmpxchg32(ptr, old, new, &mut out) } == 0).then_some(out)
}
