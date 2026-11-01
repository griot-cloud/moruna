//! Test-only assembly is necessary: ordinary Rust arithmetic may spill registers
//! around calls, hiding broken context switches. The single assembly region keeps
//! XMM0..12, an SSE2 floating accumulator, MXCSR, x87 accumulator/control live while
//! repeatedly entering CLOCK_GET and being timer-preempted. No production ABI changes.
extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{Channel, Memory, Signals, Thread, map, now, println, thread_exit, wait_one};
const WORKERS: usize = 8;
static READY: AtomicU64 = AtomicU64::new(0);
static GO: AtomicBool = AtomicBool::new(false);
static RESULTS: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
#[repr(C)]
struct State {
    old_mxcsr: u32,
    mxcsr: u32,
    seen_mxcsr: u32,
    old_cw: u16,
    cw: u16,
    seen_cw: u16,
    _pad: [u8; 6],
    integer: i64,
}
// Compile-time layout checks bind every assembly memory operand to its real field.
const _: () = {
    assert!(core::mem::offset_of!(State, old_mxcsr) == 0);
    assert!(core::mem::offset_of!(State, mxcsr) == 4);
    assert!(core::mem::offset_of!(State, seen_mxcsr) == 8);
    assert!(core::mem::offset_of!(State, old_cw) == 12);
    assert!(core::mem::offset_of!(State, cw) == 14);
    assert!(core::mem::offset_of!(State, seen_cw) == 16);
    assert!(core::mem::offset_of!(State, integer) == 24);
};
// This is the ONLY unsafe boundary in these test programs. All memory operands point
// within one live State, there are no stack operations or pointer-bearing syscalls,
// register clobbers are declared, MXCSR/control are restored and x87 stack emptied.
#[allow(unsafe_code)]
fn probe(pattern: u64, rounding: u32) -> (u64, u64) {
    let mut state = State {
        old_mxcsr: 0,
        mxcsr: 0x1f80 | (rounding << 13),
        seen_mxcsr: 0,
        old_cw: 0,
        cw: 0x037f | ((rounding as u16) << 10),
        seen_cw: 0,
        _pad: [0; 6],
        integer: 0,
    };
    let deadline = now() + ms(250);
    let status: u64;
    let iterations: u64;
    // SAFETY: the bounded region accesses only the supplied State; CLOCK_GET has
    // no pointer arguments. Busy loop has BOTH a real deadline and finite iteration
    // ceiling; no Rust call/compiler spill can restore the tested state for us.
    unsafe {
        core::arch::asm!(
                "stmxcsr [r15]", "ldmxcsr [r15+4]", "fnstcw [r15+12]", "fldcw [r15+14]", "fldz",
                "movq xmm14, r12", "punpcklqdq xmm14, xmm14",
                "movdqa xmm0, xmm14",
                "movdqa xmm1, xmm14",
                "movdqa xmm2, xmm14",
                "movdqa xmm3, xmm14",
                "movdqa xmm4, xmm14",
                "movdqa xmm5, xmm14",
                "movdqa xmm6, xmm14",
                "movdqa xmm7, xmm14",
                "movdqa xmm8, xmm14",
                "movdqa xmm9, xmm14",
                "movdqa xmm10, xmm14",
                "movdqa xmm11, xmm14",
                "movdqa xmm12, xmm14",
                "pxor xmm13, xmm13", "xor r9d, r9d", "mov r8d, 1",
                "2:",
                "movq xmm14, r12", "punpcklqdq xmm14, xmm14",
                "movdqa xmm15, xmm0", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm1", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm2", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm3", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm4", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm5", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm6", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm7", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm8", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm9", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm10", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm11", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "movdqa xmm15, xmm12", "pcmpeqb xmm15, xmm14", "pmovmskb r10d, xmm15",
                "cmp r10d, 65535", "jne 8f",
                "mov r10, 0x3ff0000000000000", "movq xmm14, r10", "addsd xmm13, xmm14",
                "inc r9", "cvtsi2sd xmm15, r9", "ucomisd xmm13, xmm15", "jne 8f", "jp 8f",
                "fld1", "faddp st(1), st(0)",
                "stmxcsr [r15+8]", "mov r10d, [r15+8]", "cmp r10d, [r15+4]", "jne 8f",
                "fnstcw [r15+16]", "movzx r10d, word ptr [r15+16]", "cmp r10w, [r15+14]", "jne 8f",
                "mov rax, r14", "syscall", "cmp rax, r13", "jae 3f",
                "cmp r9, 20000000", "jb 2b", "mov r8d, 2", "jmp 8f",
                "3:", "xor r8d, r8d",
                "8:", "fistp qword ptr [r15+24]", "cmp [r15+24], r9", "je 9f", "mov r8d, 3",
                "9:", "ldmxcsr [r15]", "fldcw [r15+12]",
                in("r12") pattern, in("r13") deadline, in("r14") moruna_abi::nr::CLOCK_GET,
                in("r15") &mut state as *mut State, lateout("r8") status, lateout("r9") iterations,
                out("rax") _, out("rdx") _, out("rcx") _, out("r11") _, out("r10") _,
                out("xmm0") _,            out("xmm1") _,            out("xmm2") _,            out("xmm3") _,            out("xmm4") _,            out("xmm5") _,            out("xmm6") _,            out("xmm7") _,            out("xmm8") _,            out("xmm9") _,            out("xmm10") _,            out("xmm11") _,            out("xmm12") _,            out("xmm13") _,            out("xmm14") _,            out("xmm15") _,
                options(nostack),
        );
    }
    (status, iterations)
}
extern "C" fn worker(arg: u64) -> ! {
    let id = (arg & 7) as usize;
    READY.fetch_add(1, Ordering::AcqRel);
    while !GO.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let (status, iterations) = probe(arg, (id & 3) as u32);
    RESULTS[id].store(
        if status == 0 && iterations > 1 {
            iterations << 8
        } else {
            if status != 0 { status } else { 4 }
        },
        Ordering::Release,
    );
    thread_exit()
}
pub fn run(seed: u64) -> i64 {
    let out = console();
    let mut stacks = Vec::new();
    for _ in 0..WORKERS {
        let Ok(stack) = Memory::create(64 * 1024) else {
            println!(out, "NATIVE FPU: FAIL: stack allocation");
            return 1;
        };
        stacks.push(stack);
    }
    let mut maps = Vec::new();
    for stack in &stacks {
        let Ok(mapping) = stack.map(0, 64 * 1024, map::WRITE) else {
            println!(out, "NATIVE FPU: FAIL: stack mapping");
            return 1;
        };
        maps.push(mapping);
    }
    let mut threads = Vec::new();
    for (id, mapping) in maps.iter().enumerate() {
        match Thread::spawn(worker, mapping.end(), seed | id as u64) {
            Ok(thread) => threads.push(thread),
            Err(error) => {
                println!(out, "NATIVE FPU: FAIL: spawn {}", error.name());
                return 1;
            }
        }
    }
    let deadline = now() + ms(10_000);
    while READY.load(Ordering::Acquire) != WORKERS as u64 {
        if now() >= deadline {
            println!(out, "NATIVE FPU: FAIL: workers not ready");
            return 1;
        }
        core::hint::spin_loop();
    }
    // Both independent processes must have all eight live workers before either
    // releases them. This is a REAL cross-process channel, not a model barrier.
    let peer = Channel(slot("peer"));
    if peer.write(&seed.to_le_bytes(), []).is_err()
        || !matches!(wait_one(&peer.0,Signals::READABLE|Signals::PEER_CLOSED,deadline),Ok(s) if s.contains(Signals::READABLE))
    {
        println!(out, "NATIVE FPU: FAIL: peer barrier");
        return 1;
    }
    let mut bytes = [0; 8];
    if !matches!(peer.read(&mut bytes, &mut []), Ok((8, 0))) || u64::from_le_bytes(bytes) == seed {
        println!(out, "NATIVE FPU: FAIL: independent peer identity");
        return 1;
    }
    GO.store(true, Ordering::Release);
    let mut total = 0;
    for (id, thread) in threads.iter().enumerate() {
        if !matches!(wait_one(&thread.0,Signals::TERMINATED,deadline),Ok(s) if s.contains(Signals::TERMINATED))
        {
            println!(out, "NATIVE FPU: FAIL: worker {} timeout", id);
            return 1;
        }
        let result = RESULTS[id].load(Ordering::Acquire);
        if result & 255 != 0 || result == 0 {
            println!(
                out,
                "NATIVE FPU: FAIL: seed {} worker {} diagnostic {}", seed, id, result
            );
            return 1;
        }
        total += result >> 8;
    }
    println!(
        out,
        "NATIVE FPU: ok: seed {} eight live workers preserved SIMD FP control and x87 state; {} checks",
        seed,
        total
    );
    0
}
