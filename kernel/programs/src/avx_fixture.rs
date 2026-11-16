//! DS-A's AVX check, a module of `datapath` (`architecture/data-os-streams.md`, stream A):
//! AVX state survives preemption and migration. The gate's tasks are built with `+avx,+avx2`, and OSTD saves a thread's
//! user state with `xsave`.
//!
//! One more thread than there are CPUs, each pinned (CPU 0 has two, so they preempt each
//! other every slice), keeps its own values in all 16 `ymm` registers for 300 ms inside
//! one assembly region. One of them moves to the last CPU and back with
//! `thread_set_cpu` from inside the region. Each stores its registers at the end, and every
//! lane is checked.
//!
//! Test-only assembly is necessary: ordinary Rust may spill or never use the registers. The
//! user target has no AVX, so the region's function enables it, and it runs only once the
//! CPU and the kernel are seen to have it (CPUID and XCR0).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use moruna_abi::nr;
use moruna_kb1_programs::ms;
use moruna_sys::{
    Error, Memory, Signals, Thread,
    datapath::{ClockPage, pin_current_thread},
    map, now, println, thread_exit, wait_one,
};

const MAX_THREADS: usize = 8;
/// The `ymm` registers' values: 16 registers of four 64-bit lanes.
const WORDS: usize = 64;

/// What one thread's region reads and writes. The offsets are the assembly's.
#[repr(C)]
struct Job {
    values: [AtomicU64; WORDS],
    out: [AtomicU64; WORDS],
    /// The counter values that end the three phases.
    ends: [AtomicU64; 3],
    /// Counter ticks between two reads that count as a preemption.
    gap: AtomicU64,
    /// The CPU it moves to, and back to; whether it moves.
    away: AtomicU64,
    home: AtomicU64,
    mover: AtomicU64,
    /// Preemptions seen, and the two `thread_set_cpu` results.
    gaps: AtomicU64,
    moved: [AtomicU64; 2],
}

const _: () = {
    assert!(core::mem::offset_of!(Job, out) == 512);
    assert!(core::mem::offset_of!(Job, ends) == 1024);
    assert!(core::mem::offset_of!(Job, gap) == 1048);
    assert!(core::mem::offset_of!(Job, away) == 1056);
    assert!(core::mem::offset_of!(Job, home) == 1064);
    assert!(core::mem::offset_of!(Job, mover) == 1072);
    assert!(core::mem::offset_of!(Job, gaps) == 1080);
    assert!(core::mem::offset_of!(Job, moved) == 1088);
};

impl Job {
    const fn new() -> Job {
        Job {
            values: [const { AtomicU64::new(0) }; WORDS],
            out: [const { AtomicU64::new(0) }; WORDS],
            ends: [const { AtomicU64::new(0) }; 3],
            gap: AtomicU64::new(0),
            away: AtomicU64::new(0),
            home: AtomicU64::new(0),
            mover: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            moved: [const { AtomicU64::new(0) }; 2],
        }
    }
}

static JOBS: [Job; MAX_THREADS] = [const { Job::new() }; MAX_THREADS];
static DONE: AtomicU32 = AtomicU32::new(0);

/// The value thread `t` keeps in lane `l` of `ymm{r}`.
fn value(t: usize, r: usize, l: usize) -> u64 {
    0xa5a5_0000_0000_0000 ^ (t as u64) << 40 ^ (r as u64) << 32 ^ (l as u64) << 24 ^ 0x5a5a
}

/// Whether the CPU has AVX and the kernel lets programs use it: CPUID's AVX and OSXSAVE
/// bits, and XCR0's SSE and AVX state.
#[allow(unsafe_code)]
fn avx() -> bool {
    use core::arch::x86_64::__cpuid;
    let ecx = __cpuid(1).ecx;
    if ecx & (1 << 28) == 0 || ecx & (1 << 27) == 0 {
        return false;
    }
    let xcr0: u64;
    // SAFETY: XGETBV with ECX 0 reads XCR0, which OSXSAVE (checked above) makes legal in
    // user mode; it touches nothing else.
    unsafe {
        core::arch::asm!(
            ".byte 0x0f, 0x01, 0xd0",
            "shl rdx, 32",
            "or rax, rdx",
            in("ecx") 0,
            out("rax") xcr0,
            out("rdx") _,
            options(nomem, nostack),
        );
    }
    xcr0 & 0b110 == 0b110
}

/// One thread's region: `ymm0` to `ymm15` loaded from `job.values`, three phases of
/// spinning on the time-stamp counter (counting gaps), with the mover's
/// `thread_set_cpu` to `away` after the first and to `home` after the second, then the
/// registers stored to `job.out`.
#[allow(unsafe_code)]
#[target_feature(enable = "avx")]
fn region(job: &Job) {
    // SAFETY: every memory operand is a field of `job` (offsets checked above), which
    // lives in a static; the only call is THREAD_SET_CPU, whose arguments are integers;
    // the registers the region changes are declared, `ymm` through their `xmm` halves,
    // and `vzeroupper` leaves the upper halves clean.
    unsafe {
        core::arch::asm!(
            "vmovdqu ymm0, [r12 + 0]",
            "vmovdqu ymm1, [r12 + 32]",
            "vmovdqu ymm2, [r12 + 64]",
            "vmovdqu ymm3, [r12 + 96]",
            "vmovdqu ymm4, [r12 + 128]",
            "vmovdqu ymm5, [r12 + 160]",
            "vmovdqu ymm6, [r12 + 192]",
            "vmovdqu ymm7, [r12 + 224]",
            "vmovdqu ymm8, [r12 + 256]",
            "vmovdqu ymm9, [r12 + 288]",
            "vmovdqu ymm10, [r12 + 320]",
            "vmovdqu ymm11, [r12 + 352]",
            "vmovdqu ymm12, [r12 + 384]",
            "vmovdqu ymm13, [r12 + 416]",
            "vmovdqu ymm14, [r12 + 448]",
            "vmovdqu ymm15, [r12 + 480]",
            "xor r14d, r14d",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov r13, rax",
            // Phase one.
            "2:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov rcx, rax",
            "sub rcx, r13",
            "mov r13, rax",
            "cmp rcx, [r12 + 1048]",
            "jbe 3f",
            "inc r14",
            "3:",
            "cmp rax, [r12 + 1024]",
            "jb 2b",
            // The mover goes to `away`.
            "cmp qword ptr [r12 + 1072], 0",
            "je 4f",
            "mov rax, {set_cpu}",
            "xor edi, edi",
            "mov rsi, [r12 + 1056]",
            "syscall",
            "mov [r12 + 1088], rax",
            "4:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov r13, rax",
            // Phase two.
            "5:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov rcx, rax",
            "sub rcx, r13",
            "mov r13, rax",
            "cmp rcx, [r12 + 1048]",
            "jbe 6f",
            "inc r14",
            "6:",
            "cmp rax, [r12 + 1032]",
            "jb 5b",
            // And back `home`.
            "cmp qword ptr [r12 + 1072], 0",
            "je 7f",
            "mov rax, {set_cpu}",
            "xor edi, edi",
            "mov rsi, [r12 + 1064]",
            "syscall",
            "mov [r12 + 1096], rax",
            "7:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov r13, rax",
            // Phase three.
            "8:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "mov rcx, rax",
            "sub rcx, r13",
            "mov r13, rax",
            "cmp rcx, [r12 + 1048]",
            "jbe 9f",
            "inc r14",
            "9:",
            "cmp rax, [r12 + 1040]",
            "jb 8b",
            "vmovdqu [r12 + 512], ymm0",
            "vmovdqu [r12 + 544], ymm1",
            "vmovdqu [r12 + 576], ymm2",
            "vmovdqu [r12 + 608], ymm3",
            "vmovdqu [r12 + 640], ymm4",
            "vmovdqu [r12 + 672], ymm5",
            "vmovdqu [r12 + 704], ymm6",
            "vmovdqu [r12 + 736], ymm7",
            "vmovdqu [r12 + 768], ymm8",
            "vmovdqu [r12 + 800], ymm9",
            "vmovdqu [r12 + 832], ymm10",
            "vmovdqu [r12 + 864], ymm11",
            "vmovdqu [r12 + 896], ymm12",
            "vmovdqu [r12 + 928], ymm13",
            "vmovdqu [r12 + 960], ymm14",
            "vmovdqu [r12 + 992], ymm15",
            "mov [r12 + 1080], r14",
            "vzeroupper",
            set_cpu = const nr::THREAD_SET_CPU,
            in("r12") job as *const Job,
            out("rax") _, out("rcx") _, out("rdx") _, out("rdi") _, out("rsi") _,
            out("r11") _, out("r13") _, out("r14") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nostack),
        );
    }
}

/// Thread `t`: pinned to its CPU, then its region.
extern "C" fn worker(t: u64) -> ! {
    let job = &JOBS[t as usize];
    if pin_current_thread(Some(job.home.load(Ordering::Relaxed) as u32)).is_ok() {
        #[allow(unsafe_code)]
        // SAFETY: `run` starts a worker only once `avx` has found AVX on, which is all
        // the target feature asks.
        unsafe {
            region(job)
        };
    }
    DONE.fetch_add(1, Ordering::Release);
    thread_exit()
}

/// The check, on `cpus` CPUs (at most `MAX_THREADS - 1`): `Err` with the exit code once
/// its FAIL line is printed.
pub fn check(out: &moruna_sys::Console, cpus: u32) -> Result<(), i64> {
    match run(out, cpus.min(MAX_THREADS as u32 - 1)) {
        0 => Ok(()),
        code => Err(code),
    }
}

fn run(out: &moruna_sys::Console, cpus: u32) -> i64 {
    if !avx() {
        println!(out, "DS-A avx: FAIL: no AVX for programs (CPUID or XCR0)");
        return 1;
    }
    let threads = cpus as usize + 1;
    let Ok(page) = ClockPage::map() else {
        println!(out, "DS-A avx: FAIL: clock_page");
        return 1;
    };
    // Counter ticks for `ns` nanoseconds, from the clock page's scale.
    let reading = page.reading();
    let ticks =
        |ns: u64| ((u128::from(ns) << reading.shift) / u128::from(reading.multiplier)) as u64;
    let start = {
        let mut aux = 0;
        #[allow(unsafe_code)]
        // SAFETY: `rdtscp` reads the counter and IA32_TSC_AUX and writes `aux`.
        unsafe {
            core::arch::x86_64::__rdtscp(&mut aux)
        }
    } + ticks(ms(50));
    let last = cpus.saturating_sub(1);
    for (t, job) in JOBS.iter().enumerate().take(threads) {
        for r in 0..16 {
            for l in 0..4 {
                job.values[r * 4 + l].store(value(t, r, l), Ordering::Relaxed);
            }
        }
        for (phase, end) in job.ends.iter().enumerate() {
            end.store(
                start + ticks(ms(100) * (phase as u64 + 1)),
                Ordering::Relaxed,
            );
        }
        job.gap.store(ticks(500_000), Ordering::Relaxed);
        // Thread `cpus` shares CPU 0 with thread 0, and is the one that moves.
        job.home
            .store((t % cpus as usize) as u64, Ordering::Relaxed);
        job.away.store(u64::from(last), Ordering::Relaxed);
        job.mover
            .store(u64::from(t == threads - 1), Ordering::Relaxed);
    }
    let mut started = Vec::new();
    for t in 0..threads {
        let stack = match Memory::create(64 * 1024) {
            Ok(s) => s,
            Err(e) => return fail(out, "a stack", e),
        };
        let mapping = match stack.map_detached(0, 64 * 1024, map::WRITE) {
            Ok(m) => m,
            Err(e) => return fail(out, "mapping a stack", e),
        };
        let top = mapping.end();
        // The stack stays mapped: the thread exits on it.
        core::mem::forget(mapping);
        core::mem::forget(stack);
        match Thread::spawn(worker, top, t as u64) {
            Ok(thread) => started.push(thread),
            Err(e) => return fail(out, "thread_create", e),
        }
    }
    for thread in &started {
        if !matches!(wait_one(&thread.0, Signals::TERMINATED, now() + ms(30_000)), Ok(s) if s.contains(Signals::TERMINATED))
        {
            println!(out, "DS-A avx: FAIL: a thread did not end");
            return 1;
        }
    }
    let mover = &JOBS[threads - 1];
    let moved = [
        mover.moved[0].load(Ordering::Relaxed),
        mover.moved[1].load(Ordering::Relaxed),
    ];
    if moved != [0, 0] {
        println!(
            out,
            "DS-A avx: FAIL: thread_set_cpu from the region gave {:#x} and {:#x}",
            moved[0],
            moved[1]
        );
        return 1;
    }
    let mut gaps = (u64::MAX, 0);
    for (t, job) in JOBS.iter().enumerate().take(threads) {
        for r in 0..16 {
            for l in 0..4 {
                let got = job.out[r * 4 + l].load(Ordering::Relaxed);
                let want = value(t, r, l);
                if got != want {
                    println!(
                        out,
                        "DS-A avx: FAIL: thread {} ymm{} lane {} held {:#x}, not {:#x}",
                        t,
                        r,
                        l,
                        got,
                        want
                    );
                    return 1;
                }
            }
        }
        let g = job.gaps.load(Ordering::Relaxed);
        gaps = (gaps.0.min(g), gaps.1.max(g));
    }
    println!(
        out,
        "DS-A avx: ok: {} threads on {} CPUs kept all 16 ymm registers through {} to {} preemptions each in 300 ms, one moved to CPU {} and back with thread_set_cpu ({} done)",
        threads,
        cpus,
        gaps.0,
        gaps.1,
        last,
        DONE.load(Ordering::Acquire)
    );
    0
}

fn fail(out: &moruna_sys::Console, what: &str, e: Error) -> i64 {
    println!(out, "DS-A avx: FAIL: {}: {}", what, e.name());
    1
}
