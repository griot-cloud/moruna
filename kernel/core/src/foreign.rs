//! Forwarded processes (SDD 28, d.1 and f.1): the foreign calls of a process that has a
//! forward go to a server as messages, except the fast calls its table names, which
//! the kernel answers in the System V convention the foreign program was built for.
//!
//! A forward is a channel endpoint the kernel holds for the process. A thread's
//! foreign call becomes a request on it (`moruna_abi::forward::request`), queued to
//! the server's end; the thread then waits for the reply carrying its token. Replies
//! for other threads stay queued for them; a reply whose token names no waiting thread,
//! or that is not a reply, is dropped. The server leaving ends the process.
//!
//! The fast calls are the ones on the caller's own memory, time and threads: a futex,
//! the clocks and sleeping, anonymous `mmap`, `munmap`, `mprotect`, `brk`, `madvise`,
//! the FS base, `set_tid_address`, `getrandom`, `exit` and `gettid`. They act only on
//! the calling thread and its process (KK-I1), and a call they do not take (a mapping
//! of a file, a futex operation they do not know) is forwarded instead.

use alloc::{collections::BTreeSet, string::String, sync::Arc, vec::Vec};

use moruna_abi::{
    DEADLINE_INFINITE, Error, PAGE_SIZE,
    forward::{fast, reply, request, table},
    limits, map, read_u32, read_u64, write_u32, write_u64,
};
use ostd::{
    arch::{cpu::context::UserContext, read_random},
    sync::{SpinLock, Waiter},
    task::Task,
    user::UserContextApi,
};

use crate::{
    channel::{Endpoint, Message},
    clock, futex,
    process::{Exit, Thread},
    syscall::Flow,
    usercopy,
    vm::{Backing, Kind, pages},
};

/// The errno values the fast calls answer with: the x86-64 System V convention's.
mod errno {
    pub const EPERM: u64 = 1;
    pub const EINTR: u64 = 4;
    pub const EIO: u64 = 5;
    pub const EAGAIN: u64 = 11;
    pub const ENOMEM: u64 = 12;
    pub const EACCES: u64 = 13;
    pub const EFAULT: u64 = 14;
    pub const EEXIST: u64 = 17;
    pub const EINVAL: u64 = 22;
    pub const ETIMEDOUT: u64 = 110;
}

fn fail(errno: u64) -> u64 {
    errno.wrapping_neg()
}

/// Whether `addr` is a canonical lower-half address, one the FS base may hold.
pub fn canonical(addr: u64) -> bool {
    addr < 0x0000_8000_0000_0000
}

pub struct Forward {
    endpoint: Arc<Endpoint>,
    calls: [u8; limits::FORWARD_FAST],
    /// The tokens of threads waiting for a reply.
    waiting: SpinLock<BTreeSet<u64>>,
}

impl Forward {
    /// A forward over the kernel's end of a channel, with the table `process_forward`
    /// read. `InvalidArgs` for a table of the wrong size or magic, or naming a fast
    /// call this kernel does not have.
    pub fn new(endpoint: Arc<Endpoint>, bytes: &[u8]) -> Result<Forward, Error> {
        if bytes.len() != table::SIZE
            || read_u32(bytes, table::MAGIC) != Some(table::MAGIC_VALUE)
            || read_u32(bytes, table::RESERVED) != Some(0)
        {
            return Err(Error::InvalidArgs);
        }
        let mut calls = [fast::NONE; limits::FORWARD_FAST];
        calls.copy_from_slice(&bytes[table::CALLS..table::CALLS + limits::FORWARD_FAST]);
        if calls.iter().any(|c| *c >= fast::COUNT) {
            return Err(Error::InvalidArgs);
        }
        Ok(Forward {
            endpoint,
            calls,
            waiting: SpinLock::new(BTreeSet::new()),
        })
    }
}

/// What a fast call does with the thread.
enum Fast {
    /// Its result, for `rax`.
    Value(u64),
    /// Not one it takes: forward the call.
    Forward,
    /// The thread ends, with the code.
    Exit(i64),
}

/// Answers a forwarded process's foreign call `ctx` holds.
pub fn call(thread: &Arc<Thread>, forward: &Forward, ctx: &mut UserContext) -> Flow {
    let number = ctx.rax() as u64;
    let args = [
        ctx.rdi() as u64,
        ctx.rsi() as u64,
        ctx.rdx() as u64,
        ctx.r10() as u64,
        ctx.r8() as u64,
        ctx.r9() as u64,
    ];
    let which = usize::try_from(number)
        .ok()
        .and_then(|n| forward.calls.get(n).copied())
        .unwrap_or(fast::NONE);
    if which != fast::NONE {
        match fast_call(thread, which, args) {
            Fast::Value(v) => {
                ctx.set_rax(v as usize);
                return Flow::Continue;
            }
            Fast::Exit(code) => return Flow::ThreadExitCode(code),
            Fast::Forward => {}
        }
    }
    send(thread, forward, number, args, ctx)
}

/// The process's server is gone or broke its protocol: the process ends with why.
fn broken(thread: &Thread, why: String) -> Flow {
    thread.process.end(Exit::Fault(why));
    Flow::ThreadExit
}

fn send(
    thread: &Arc<Thread>,
    forward: &Forward,
    number: u64,
    args: [u64; 6],
    ctx: &mut UserContext,
) -> Flow {
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(request::SIZE).is_err() {
        return broken(thread, String::from("no memory for a forwarded call"));
    }
    bytes.resize(request::SIZE, 0);
    write_u32(&mut bytes, request::KIND, request::CALL);
    write_u32(&mut bytes, request::THREADS, thread.process.live_threads());
    write_u64(&mut bytes, request::TOKEN, thread.tid);
    write_u64(&mut bytes, request::NUMBER, number);
    for (i, a) in args.iter().enumerate() {
        write_u64(&mut bytes, request::ARGS + 8 * i, *a);
    }
    write_u64(&mut bytes, request::IP, ctx.instruction_pointer() as u64);
    *thread.parked.lock() = Some(ctx.clone());
    // A thread has one forwarded call in flight at a time: its request is paid for in
    // the thread's own charge (`charge::THREAD`).
    let request = Message {
        bytes,
        handles: Vec::new(),
        endpoints: 0,
        charge: None,
    };
    let answer = exchange(
        thread,
        &forward.endpoint,
        &forward.waiting,
        request,
        reply::SIZE,
        "its personality server",
    );
    *thread.parked.lock() = None;
    let message = match answer {
        Ok(Some(m)) => m.bytes,
        Ok(None) => return Flow::ThreadExit,
        Err(why) => return broken(thread, why),
    };
    let value = read_u64(&message, reply::VALUE).unwrap_or(0);
    match read_u64(&message, reply::ACTION) {
        Some(reply::RETURN) => {
            ctx.set_rax(value as usize);
            Flow::Continue
        }
        Some(reply::EXIT_THREAD) => Flow::ThreadExit,
        Some(reply::EXIT_PROCESS) => Flow::ProcessExit(value as i64),
        Some(reply::RESTART) => {
            let stack = read_u64(&message, reply::STACK).unwrap_or(0);
            let brk = read_u64(&message, reply::BRK).unwrap_or(0);
            if !crate::process::is_user(value) || !crate::process::is_user(stack) {
                return broken(
                    thread,
                    String::from("the server restarted it at an address outside user space"),
                );
            }
            thread.process.end_others(thread);
            if thread.process.space.reset_brk(brk).is_err() {
                return broken(
                    thread,
                    String::from("the server restarted it with a heap that overlaps a mapping"),
                );
            }
            *ctx = UserContext::default();
            ctx.set_instruction_pointer(value as usize);
            ctx.set_stack_pointer(stack as usize);
            thread.set_fs(0);
            thread
                .clear_tid
                .store(0, core::sync::atomic::Ordering::Relaxed);
            Flow::Continue
        }
        Some(other) => broken(
            thread,
            alloc::format!("its server answered with action {other}"),
        ),
        None => broken(thread, String::from("its server's reply was short")),
    }
}

/// Queues `request` towards the peer of `endpoint`, the kernel's end of a channel to a
/// server (a forward's personality, SDD 28 f.1, or a task's engine, SDD 29 f.4), and
/// waits for the reply with this thread's token: a message of exactly `reply_size`
/// bytes whose first eight are the token. Replies for the other threads in `waiting`
/// stay queued for them; anything else is dropped. `Ok(None)` when the thread was
/// killed while it waited; `Err` when the server (`who`) has gone.
pub(crate) fn exchange(
    thread: &Thread,
    endpoint: &Arc<Endpoint>,
    waiting: &SpinLock<BTreeSet<u64>>,
    request: Message,
    reply_size: usize,
    who: &str,
) -> Result<Option<Message>, String> {
    waiting.lock().insert(thread.tid);
    let answer = rendezvous(thread, endpoint, waiting, request, reply_size, who);
    waiting.lock().remove(&thread.tid);
    answer
}

fn rendezvous(
    thread: &Thread,
    endpoint: &Arc<Endpoint>,
    waiting: &SpinLock<BTreeSet<u64>>,
    request: Message,
    reply_size: usize,
    who: &str,
) -> Result<Option<Message>, String> {
    let channel = &endpoint.channel;
    let (side, peer) = (endpoint.side, endpoint.peer());
    let (waiter, waker) = Waiter::new_pair();
    endpoint.signaller().observe(&waker);
    thread.set_waker(Some(waker.clone()));
    let mut pending = Some(request);
    // What is dropped from the queue is dropped after the lock, and there is never more
    // of it than the queue holds.
    let mut dropped: Vec<Message> = Vec::new();
    if dropped.try_reserve(limits::CHANNEL_DEPTH).is_err() {
        return Err(alloc::format!("no memory to wait for {who}"));
    }
    let result = loop {
        if thread.killed() {
            break Ok(None);
        }
        {
            let mut inner = channel.inner.lock();
            if let Some(message) = pending.take() {
                if !inner.open[peer] {
                    break Err(alloc::format!("{who} has gone"));
                }
                if inner.queues[peer].len() < limits::CHANNEL_DEPTH {
                    inner.queues[peer].push_back(message);
                    channel.refresh(&inner);
                } else {
                    pending = Some(message);
                }
            }
            if pending.is_none() {
                let waiting = waiting.lock();
                let mut found = None;
                let mut removed = false;
                let mut i = 0;
                while i < inner.queues[side].len() {
                    let token = inner.queues[side][i]
                        .bytes
                        .get(..reply_size)
                        .filter(|_| inner.queues[side][i].bytes.len() == reply_size)
                        .and_then(|b| read_u64(b, 0));
                    match token {
                        Some(t) if t == thread.tid => {
                            found = inner.queues[side].remove(i);
                            removed = true;
                            break;
                        }
                        Some(t) if waiting.contains(&t) => i += 1,
                        // Not a reply, or for no waiting thread: dropped (outside the
                        // lock below, with the handles it carries).
                        _ => {
                            if let Some(stale) = inner.queues[side].remove(i) {
                                channel.uncarry(side, stale.endpoints);
                                // Within what was reserved: one pass drops at most
                                // what the queue holds, and each pass is emptied.
                                dropped.push(stale);
                            }
                            removed = true;
                        }
                    }
                }
                drop(waiting);
                if let Some(message) = &found {
                    channel.uncarry(side, message.endpoints);
                }
                channel.refresh(&inner);
                // A reply queued while another was waiting raised no signal: the
                // threads it is for are woken once a reply has been taken and more
                // are left.
                if removed && !inner.queues[side].is_empty() {
                    endpoint.signaller().poke();
                }
                if let Some(message) = found {
                    break Ok(Some(message));
                }
                if !inner.open[peer] {
                    break Err(alloc::format!("{who} has gone"));
                }
            }
        }
        dropped.clear();
        waiter.wait();
    };
    endpoint.signaller().unobserve(&waker);
    thread.set_waker(None);
    drop(dropped);
    result
}

fn timespec(thread: &Thread, ptr: u64) -> Result<u64, u64> {
    let space = &thread.process.space;
    let sec = usercopy::read_u64(space, ptr).map_err(|_| errno::EFAULT)?;
    let nsec = usercopy::read_u64(space, ptr + 8).map_err(|_| errno::EFAULT)?;
    if (sec as i64) < 0 || nsec >= 1_000_000_000 {
        return Err(errno::EINVAL);
    }
    Ok(sec.saturating_mul(1_000_000_000).saturating_add(nsec))
}

fn put_timespec(thread: &Thread, ptr: u64, ns: u64) -> u64 {
    let mut b = [0u8; 16];
    write_u64(&mut b, 0, ns / 1_000_000_000);
    write_u64(&mut b, 8, ns % 1_000_000_000);
    match usercopy::write(&thread.process.space, ptr, &b) {
        Ok(()) => 0,
        Err(_) => fail(errno::EFAULT),
    }
}

/// The clocks of `clock_gettime`: realtime, monotonic, the process's and the thread's
/// CPU time, monotonic raw, the coarse realtime and monotonic, and boot time.
fn clock_now(thread: &Thread, id: u64) -> Option<u64> {
    match id as i32 {
        0 | 5 => Some(clock::realtime()),
        1 | 4 | 6 | 7 => Some(clock::now()),
        2 => Some(
            thread
                .process
                .cpu_ns
                .load(core::sync::atomic::Ordering::Relaxed)
                .saturating_add(thread.slice()),
        ),
        3 => Some(thread.cpu_time()),
        _ => None,
    }
}

/// Sleeps until `deadline` on the monotonic clock, or until the thread is killed.
fn sleep_until(thread: &Thread, deadline: u64) -> Result<(), ()> {
    let (waiter, waker) = Waiter::new_pair();
    thread.set_waker(Some(waker.clone()));
    let armed = (deadline != DEADLINE_INFINITE).then(|| clock::arm_wake(deadline, waker));
    let result = loop {
        if thread.killed() {
            break Err(());
        }
        if clock::now() >= deadline {
            break Ok(());
        }
        waiter.wait();
    };
    if let Some(armed) = armed {
        clock::disarm(armed);
    }
    thread.set_waker(None);
    result
}

fn map_error(e: Error) -> u64 {
    fail(match e {
        Error::NoMemory | Error::Limit | Error::OutOfRange | Error::QuotaExceeded => errno::ENOMEM,
        Error::AccessDenied => errno::EACCES,
        Error::Fault => errno::EFAULT,
        Error::BadState => errno::EEXIST,
        _ => errno::EINVAL,
    })
}

fn futex_errno(e: Error) -> u64 {
    fail(match e {
        Error::ShouldWait => errno::EAGAIN,
        Error::TimedOut => errno::ETIMEDOUT,
        Error::BadState => errno::EINTR,
        Error::Fault => errno::EFAULT,
        Error::NoMemory | Error::QuotaExceeded => errno::ENOMEM,
        _ => errno::EINVAL,
    })
}

const FUTEX_PRIVATE: u64 = 128;
const FUTEX_REALTIME: u64 = 256;

fn futex_call(thread: &Thread, a: [u64; 6]) -> Fast {
    let (addr, op, val) = (a[0], a[1], a[2] as u32);
    let koid = thread.process.koid;
    let cmd = op & !(FUTEX_PRIVATE | FUTEX_REALTIME);
    let done = |r: Result<u32, Error>| match r {
        Ok(n) => Fast::Value(n as u64),
        Err(e) => Fast::Value(futex_errno(e)),
    };
    match cmd {
        // FUTEX_WAIT: a relative timeout on the monotonic clock.
        0 => {
            let deadline = if a[3] == 0 {
                DEADLINE_INFINITE
            } else {
                match timespec(thread, a[3]) {
                    Ok(ns) => clock::now().saturating_add(ns).min(DEADLINE_INFINITE - 1),
                    Err(e) => return Fast::Value(fail(e)),
                }
            };
            done(futex::wait(thread, addr, val, deadline, futex::ALL).map(|_| 0))
        }
        // FUTEX_WAKE.
        1 => done(futex::wake(koid, addr, val, futex::ALL)),
        // FUTEX_REQUEUE and FUTEX_CMP_REQUEUE: the fourth argument is a count.
        3 | 4 => {
            let expected = (cmd == 4).then_some(a[5] as u32);
            done(futex::requeue(
                thread,
                addr,
                a[4],
                val,
                a[3] as u32,
                expected,
            ))
        }
        // FUTEX_WAIT_BITSET: an absolute timeout, monotonic or realtime.
        9 => {
            let deadline = if a[3] == 0 {
                DEADLINE_INFINITE
            } else {
                match timespec(thread, a[3]) {
                    Ok(ns) if op & FUTEX_REALTIME != 0 => {
                        ns.saturating_sub(clock::realtime_at_boot())
                    }
                    Ok(ns) => ns,
                    Err(e) => return Fast::Value(fail(e)),
                }
            };
            done(futex::wait(thread, addr, val, deadline.max(1), a[5] as u32).map(|_| 0))
        }
        // FUTEX_WAKE_BITSET.
        10 => done(futex::wake(koid, addr, val, a[5] as u32)),
        _ => Fast::Forward,
    }
}

const MAP_SHARED: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

fn mmap_anonymous(thread: &Thread, a: [u64; 6]) -> Fast {
    let (addr, len, prot, flags) = (a[0], a[1], a[2], a[3]);
    if flags & MAP_ANONYMOUS == 0 {
        return Fast::Forward;
    }
    if flags & (MAP_SHARED | MAP_PRIVATE) == 0 || len == 0 || prot & !map::PROTECTION != 0 {
        return Fast::Value(fail(errno::EINVAL));
    }
    let Ok(len) = pages(len) else {
        return Fast::Value(fail(errno::ENOMEM));
    };
    let space = &thread.process.space;
    let fixed = flags & MAP_FIXED != 0;
    let noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    if (fixed || noreplace) && !addr.is_multiple_of(PAGE_SIZE) {
        return Fast::Value(fail(errno::EINVAL));
    }
    let mapped = if fixed || noreplace {
        space.map(Some(addr), len, Kind::Mapping, prot, Backing::Zero, fixed)
    } else if addr != 0 && addr.is_multiple_of(PAGE_SIZE) {
        // A hint: taken when it is free.
        space
            .map(Some(addr), len, Kind::Mapping, prot, Backing::Zero, false)
            .or_else(|_| space.map(None, len, Kind::Mapping, prot, Backing::Zero, false))
    } else {
        space.map(None, len, Kind::Mapping, prot, Backing::Zero, false)
    };
    Fast::Value(match mapped {
        Ok(va) => va,
        Err(e) => map_error(e),
    })
}

fn fill_random(thread: &Thread, buf: u64, len: u64) -> u64 {
    let len = len.min(64 * 1024) as usize;
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(len).is_err() {
        return fail(errno::ENOMEM);
    }
    while bytes.len() < len {
        let Some(word) = read_random() else {
            return fail(errno::EIO);
        };
        let take = (len - bytes.len()).min(8);
        bytes.extend_from_slice(&word.to_le_bytes()[..take]);
    }
    match usercopy::write(&thread.process.space, buf, &bytes) {
        Ok(()) => len as u64,
        Err(_) => fail(errno::EFAULT),
    }
}

fn fast_call(thread: &Thread, which: u8, a: [u64; 6]) -> Fast {
    let space = &thread.process.space;
    match which {
        fast::FUTEX => futex_call(thread, a),
        fast::CLOCK_GETTIME => match clock_now(thread, a[0]) {
            Some(ns) => Fast::Value(put_timespec(thread, a[1], ns)),
            None => Fast::Value(fail(errno::EINVAL)),
        },
        fast::CLOCK_GETRES => {
            if clock_now(thread, a[0]).is_none() {
                return Fast::Value(fail(errno::EINVAL));
            }
            if a[1] == 0 {
                return Fast::Value(0);
            }
            Fast::Value(put_timespec(thread, a[1], 1))
        }
        fast::GETTIMEOFDAY => {
            if a[0] != 0 {
                let ns = clock::realtime();
                let mut b = [0u8; 16];
                write_u64(&mut b, 0, ns / 1_000_000_000);
                write_u64(&mut b, 8, (ns % 1_000_000_000) / 1000);
                if usercopy::write(space, a[0], &b).is_err() {
                    return Fast::Value(fail(errno::EFAULT));
                }
            }
            if a[1] != 0 && usercopy::write(space, a[1], &[0u8; 8]).is_err() {
                return Fast::Value(fail(errno::EFAULT));
            }
            Fast::Value(0)
        }
        fast::TIME => {
            let secs = clock::realtime() / 1_000_000_000;
            if a[0] != 0 && usercopy::write(space, a[0], &secs.to_le_bytes()).is_err() {
                return Fast::Value(fail(errno::EFAULT));
            }
            Fast::Value(secs)
        }
        fast::NANOSLEEP => match timespec(thread, a[0]) {
            Ok(ns) => {
                let deadline = clock::now().saturating_add(ns).min(DEADLINE_INFINITE - 1);
                Fast::Value(match sleep_until(thread, deadline) {
                    Ok(()) => 0,
                    Err(()) => fail(errno::EINTR),
                })
            }
            Err(e) => Fast::Value(fail(e)),
        },
        fast::CLOCK_NANOSLEEP => {
            let (id, flags) = (a[0], a[1]);
            if clock_now(thread, id).is_none() || matches!(id as i32, 2 | 3) {
                return Fast::Value(fail(errno::EINVAL));
            }
            let ns = match timespec(thread, a[2]) {
                Ok(ns) => ns,
                Err(e) => return Fast::Value(fail(e)),
            };
            let absolute = flags & 1 != 0;
            let deadline = match (absolute, id as i32) {
                (true, 0 | 5) => ns.saturating_sub(clock::realtime_at_boot()),
                (true, _) => ns,
                (false, _) => clock::now().saturating_add(ns),
            };
            Fast::Value(
                match sleep_until(thread, deadline.min(DEADLINE_INFINITE - 1)) {
                    Ok(()) => 0,
                    Err(()) => fail(errno::EINTR),
                },
            )
        }
        fast::MMAP => mmap_anonymous(thread, a),
        fast::MUNMAP => {
            let Ok(len) = pages(a[1]) else {
                return Fast::Value(fail(errno::EINVAL));
            };
            if !a[0].is_multiple_of(PAGE_SIZE) || len == 0 {
                return Fast::Value(fail(errno::EINVAL));
            }
            Fast::Value(match space.unmap(a[0], len, None) {
                Ok(()) => 0,
                Err(_) => fail(errno::EINVAL),
            })
        }
        fast::MPROTECT => {
            let Ok(len) = pages(a[1]) else {
                return Fast::Value(fail(errno::ENOMEM));
            };
            if !a[0].is_multiple_of(PAGE_SIZE) || a[2] & !map::PROTECTION != 0 {
                return Fast::Value(fail(errno::EINVAL));
            }
            if len == 0 {
                return Fast::Value(0);
            }
            Fast::Value(match space.protect(a[0], len, a[2], None) {
                Ok(()) => 0,
                Err(e) => map_error(e),
            })
        }
        fast::BRK => Fast::Value(space.brk(a[0])),
        fast::MADVISE => {
            let Ok(len) = pages(a[1]) else {
                return Fast::Value(fail(errno::EINVAL));
            };
            if !a[0].is_multiple_of(PAGE_SIZE) {
                return Fast::Value(fail(errno::EINVAL));
            }
            match a[2] {
                // MADV_DONTNEED and MADV_FREE: the pages read zero again.
                4 | 8 if len > 0 => Fast::Value(match space.discard(a[0], len) {
                    Ok(()) => 0,
                    Err(e) => map_error(e),
                }),
                // Advice on how pages will be used changes nothing here.
                _ => Fast::Value(0),
            }
        }
        fast::SCHED_YIELD => {
            Task::yield_now();
            Fast::Value(0)
        }
        fast::ARCH_PRCTL => match a[0] {
            // ARCH_SET_FS.
            0x1002 => {
                if !canonical(a[1]) {
                    return Fast::Value(fail(errno::EPERM));
                }
                thread.set_fs(a[1]);
                Fast::Value(0)
            }
            // ARCH_GET_FS.
            0x1003 => Fast::Value(
                match usercopy::write(space, a[1], &thread.fs().to_le_bytes()) {
                    Ok(()) => 0,
                    Err(_) => fail(errno::EFAULT),
                },
            ),
            _ => Fast::Forward,
        },
        fast::SET_TID_ADDRESS => {
            thread
                .clear_tid
                .store(a[0], core::sync::atomic::Ordering::Relaxed);
            Fast::Value(thread.tid)
        }
        fast::GETRANDOM => {
            // GRND_NONBLOCK, GRND_RANDOM and GRND_INSECURE: the source never blocks.
            if a[2] & !0x7 != 0 {
                return Fast::Value(fail(errno::EINVAL));
            }
            Fast::Value(fill_random(thread, a[0], a[1]))
        }
        fast::EXIT => Fast::Exit(a[0] as i32 as i64),
        fast::GETTID => Fast::Value(thread.tid),
        _ => Fast::Forward,
    }
}
