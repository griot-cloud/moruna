//! stream U: the usage build's engine prints its counters when it exits (`kernel/usage`).
//!
//! The engine is a Linux program on the personality, built for its musl target. In the usage
//! build it is compiled with `-C instrument-coverage` (LLVM's own profiling runtime comes with
//! it on that target) and with this crate's `usage` feature, and at its exit this prints its
//! counters on its standard error, which the Linux server writes to the console, a line at a
//! time, the line cut at 400 bytes; so the dump's lines are shorter than that. No production
//! build has this module, and nothing else changes: the engine's exit runs it from
//! `.fini_array`.

use moruna_usage::dump::{self, FINGERPRINT_MAX, Header};

/// The longest line the Linux server keeps whole, newline included.
const LINE: usize = 392;
/// Linux's `write`.
const WRITE: u64 = 1;

unsafe extern "C" {
    // The bounds of the counter and data sections, which the linker defines for sections
    // named as C identifiers (LLVM's `__llvm_prf_*`).
    static __start___llvm_prf_cnts: u8;
    static __stop___llvm_prf_cnts: u8;
    static __start___llvm_prf_data: u8;
    static __stop___llvm_prf_data: u8;
}

/// All of `line` on standard error, as long as the writes take it.
fn write_err(line: &[u8]) {
    let mut done = 0;
    while done < line.len() {
        let a = [
            2,
            line[done..].as_ptr() as u64,
            (line.len() - done) as u64,
            0,
            0,
            0,
        ];
        // SAFETY: Linux's `write` reads `len` bytes from `ptr`, which lie in `line`.
        let wrote = unsafe { super::syscall(WRITE, a) }.0 as i64;
        if wrote <= 0 {
            return;
        }
        done += wrote as usize;
    }
}

/// Prints the counters, as `usage::dump` does in a native program.
pub extern "C" fn at_exit() {
    let first = (&raw const __start___llvm_prf_cnts).cast::<u64>();
    let end = &raw const __stop___llvm_prf_cnts as usize;
    let count = end.saturating_sub(first as usize) / 8;
    let data = (&raw const __start___llvm_prf_data).cast::<u64>();
    let data_size = (&raw const __stop___llvm_prf_data as usize).saturating_sub(data as usize);
    // SAFETY: the data section holds whole 64-byte records when it is not empty; its first
    // eight bytes are read only then, and it is never written.
    let name = if data_size >= 8 {
        unsafe { core::ptr::read_volatile(data) }
    } else {
        0
    };
    let mut print = [0u8; FINGERPRINT_MAX];
    let header = Header {
        program: dump::fingerprint(data_size as u64, name, &mut print),
        process: crate::start::process_name(),
        // SAFETY: `rdtsc` reads the time-stamp counter, which every x86-64 CPU has.
        instance: unsafe { core::arch::x86_64::_rdtsc() },
        seq: 0,
        addr: first as u64,
        count,
        wakes: 0,
    };
    // SAFETY: counters `0..count` lie between the section's bounds, are 8-byte aligned and live
    // as long as the program; a read sees a whole value.
    let counter = |i: usize| unsafe { core::ptr::read_volatile(first.add(i)) };
    let mut buf = [0u8; LINE];
    dump::encode(&header, counter, &mut buf, &mut |line: &[u8]| {
        write_err(line)
    });
}

/// Run by the C library's `exit`, after the `atexit` handlers.
#[used]
#[unsafe(link_section = ".fini_array")]
static AT_EXIT: extern "C" fn() = at_exit;
