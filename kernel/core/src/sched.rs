//! The scheduler (SDD 20, f.11): one run queue per CPU, an idle task per CPU, and a
//! time slice, injected into OSTD before any task runs.
//!
//! OSTD's default scheduler queues every task on the boot processor, so a kernel that
//! wants its other CPUs must bring its own. This one places a new task on the CPU with
//! the fewest runnable tasks, preferring the current one on a tie; retains that CPU
//! across waits and wakes. OSTD dequeues a parking task before its old context has
//! finished switching out, so migrating it at that point could select a task still
//! in the foreground on another CPU. New tasks still spread across all CPUs. It runs each
//! CPU's queue first in, first out; preempts a task that has run for a
//! slice while another waits on its CPU; and gives each CPU an idle task that halts
//! until an interrupt, so a CPU with nothing to run sleeps instead of spinning. A wake
//! that queues a task on a CPU running its idle task asks that CPU to reschedule. A
//! CPU's boot context is never preempted (the boot processor's runs the whole boot);
//! it moves to a task when it yields at the end of its entry.
//!
//! It also records which CPUs ran process threads (the post-schedule hook calls
//! [`ran_process_thread`]), so a boot can show the processes ran across every CPU.
//!
//! A thread can be placed on one CPU (`thread_set_cpu`, data-os §2 and C5): a place to
//! run, never a function to run. The pin waits in [`PINS`] until the thread is next
//! queued, and it moves then, when it is on no CPU's queue. Since a task keeps its CPU,
//! that one move is the whole mechanism: the thread runs there until it is placed again.

use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use moruna_abi::Error;
use ostd::{
    cpu::{CpuId, PinCurrentCpu, num_cpus},
    sync::{LocalIrqDisabled, SpinLock, Waiter},
    task::{
        Task, TaskOptions, disable_preempt, halt_cpu,
        scheduler::{
            EnqueueFlags, LocalRunQueue, Scheduler, UpdateFlags, enable_preemption_on_cpu,
            info::CommonSchedInfo, inject_scheduler,
        },
    },
    util::id_set::Id,
};

use crate::process::Thread;

/// Timer ticks (1 ms each) a task runs before another task waiting on its CPU may
/// take the CPU from it.
const SLICE_TICKS: u32 = 10;

/// The most CPUs whose use [`ran_process_thread`] records.
pub const CPUS_MAX: usize = 64;

/// Bit `i` is set once CPU `i` has run a process thread.
static RAN: AtomicU64 = AtomicU64::new(0);

/// The pins not yet applied, by thread id: the thread, held weakly so a thread that ends
/// first leaves nothing behind, and the CPU it is to run on.
static PINS: SpinLock<BTreeMap<u64, (Weak<Thread>, CpuId)>, LocalIrqDisabled> =
    SpinLock::new(BTreeMap::new());
/// How many pins wait, read without the lock by every enqueue.
static PENDING: AtomicUsize = AtomicUsize::new(0);

struct RunQueue {
    current: Option<Arc<Task>>,
    queue: VecDeque<Arc<Task>>,
    idle: Arc<Task>,
    ticks: u32,
    /// Runnable tasks on this CPU other than its idle task, read without the lock by
    /// [`Cpus::select`].
    load: Arc<AtomicUsize>,
}

impl RunQueue {
    fn is_idle(&self, task: &Arc<Task>) -> bool {
        Arc::ptr_eq(task, &self.idle)
    }

    /// Whether `task` is this CPU's current task or waits in its queue.
    fn holds(&self, task: &Arc<Task>) -> bool {
        self.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, task))
            || self.queue.iter().any(|queued| Arc::ptr_eq(queued, task))
    }

    fn publish(&self) {
        let busy = self.current.as_ref().is_some_and(|c| !self.is_idle(c));
        self.load
            .store(self.queue.len() + usize::from(busy), Ordering::Relaxed);
    }
}

impl LocalRunQueue for RunQueue {
    fn current(&self) -> Option<&Arc<Task>> {
        self.current.as_ref()
    }

    fn update_current(&mut self, flags: UpdateFlags) -> bool {
        let waiting = !self.queue.is_empty();
        match flags {
            // The boot context (no current task) is never preempted: it leaves for a
            // task only when it yields, once the boot is done.
            UpdateFlags::Tick => {
                self.ticks = self.ticks.saturating_add(1);
                match &self.current {
                    None => false,
                    Some(c) => waiting && (self.is_idle(c) || self.ticks >= SLICE_TICKS),
                }
            }
            // The current task is leaving its CPU: the next one is the head of the
            // queue, or the idle task.
            UpdateFlags::Wait | UpdateFlags::Exit => true,
            // A yield from the boot context (no current task) moves to a task.
            UpdateFlags::Yield => waiting || self.current.is_none(),
        }
    }

    fn try_pick_next(&mut self) -> Option<&Arc<Task>> {
        let next = match self.queue.pop_front() {
            Some(task) => task,
            None if self.current.is_none() => self.idle.clone(),
            None => return None,
        };
        self.ticks = 0;
        if let Some(previous) = self.current.replace(next)
            && !self.is_idle(&previous)
        {
            self.queue.push_back(previous);
        }
        self.publish();
        self.current.as_ref()
    }

    fn dequeue_current(&mut self) -> Option<Arc<Task>> {
        let current = self.current.take();
        // Preserve the task's assigned CPU even while it is absent from the
        // runnable queue. OSTD has not switched away from this context yet;
        // publishing cpu=None here would permit a concurrent wake to migrate
        // the still-foreground task. Queue membership is checked under its
        // home queue's lock in enqueue, independently of this affinity.
        self.publish();
        current
    }
}

struct Cpus {
    queues: Vec<SpinLock<RunQueue>>,
    loads: Vec<Arc<AtomicUsize>>,
}

impl Cpus {
    /// Where a task that names no CPU goes: a woken one to this CPU if at most its
    /// waker is runnable here, else (and a new one always) the CPU with the fewest
    /// runnable tasks, this one on a tie.
    fn select(&self, flags: EnqueueFlags) -> CpuId {
        let here = disable_preempt().current_cpu();
        let count = self.loads.len();
        let start = here.as_usize();
        if flags == EnqueueFlags::Wake && self.loads[start].load(Ordering::Relaxed) <= 1 {
            return here;
        }
        let mut best = start;
        let mut best_load = self.loads[start].load(Ordering::Relaxed);
        for step in 1..count {
            let cpu = (start + step) % count;
            let load = self.loads[cpu].load(Ordering::Relaxed);
            if load < best_load {
                best = cpu;
                best_load = load;
            }
        }
        CpuId::try_from(best).unwrap_or(here)
    }
}

impl Cpus {
    /// Moves `task`, which is being queued, to `cpu` if no queue holds it: `false` when
    /// its CPU's queue still has it (a wake while it runs), and the pin must wait.
    fn place(&self, task: &Arc<Task>, cpu: CpuId) -> bool {
        let Some(old) = task.cpu().get() else {
            return task.cpu().set_if_is_none(cpu).is_ok();
        };
        if old == cpu {
            return true;
        }
        // Under its old queue's lock, so no enqueue queues it there meanwhile: one that
        // read the old CPU finds it changed once it has the lock, and retries.
        let rq = self.queues[old.as_usize()].disable_irq().lock();
        if rq.holds(task) {
            return false;
        }
        task.cpu().set_anyway(cpu);
        true
    }
}

impl Scheduler for Cpus {
    fn enqueue(&self, runnable: Arc<Task>, flags: EnqueueFlags) -> Option<CpuId> {
        if PENDING.load(Ordering::Acquire) != 0
            && let Some((thread, cpu)) = take_pin(&runnable)
            && !self.place(&runnable, cpu)
        {
            put_pin(&thread, cpu);
        }
        // Only the first enqueue chooses a CPU. An assigned CPU is affinity,
        // not evidence that this task is still runnable: a waiter can already
        // be dequeued while its old context is completing the switch-out.
        let selected = self.select(flags);
        let (target, mut rq) = loop {
            let target = match runnable.cpu().set_if_is_none(selected) {
                Ok(()) => selected,
                Err(cpu) => cpu,
            };
            let rq = self.queues[target.as_usize()].disable_irq().lock();
            // A pin may have moved it while this waited for the lock.
            if runnable.cpu().get() == Some(target) {
                break (target, rq);
            }
        };
        if rq.holds(&runnable) {
            return None;
        }
        // Only a CPU running its idle task is asked to reschedule; one still in its
        // boot context takes the task when it yields.
        let idle = rq.current.as_ref().is_some_and(|c| rq.is_idle(c));
        rq.queue.push_back(runnable);
        rq.publish();
        idle.then_some(target)
    }

    fn local_rq_with(&self, f: &mut dyn FnMut(&dyn LocalRunQueue)) {
        let guard = disable_preempt();
        let rq = self.queues[guard.current_cpu().as_usize()]
            .disable_irq()
            .lock();
        f(&*rq);
    }

    fn mut_local_rq_with(&self, f: &mut dyn FnMut(&mut dyn LocalRunQueue)) {
        let guard = disable_preempt();
        let mut rq = self.queues[guard.current_cpu().as_usize()]
            .disable_irq()
            .lock();
        f(&mut *rq);
    }
}

fn idle() {
    loop {
        halt_cpu();
    }
}

/// Builds the scheduler with an idle task for every CPU, injects it, enables the time
/// slice on this CPU, and registers the entry the other CPUs run. Called once, from
/// the boot context of the boot processor, before any task is spawned.
pub fn init() -> Result<(), &'static str> {
    let count = num_cpus();
    if count > CPUS_MAX {
        return Err("more CPUs than the scheduler records");
    }
    let mut queues = Vec::new();
    let mut loads = Vec::new();
    for _ in 0..count {
        let task = TaskOptions::new(idle)
            .build()
            .map_err(|_| "an idle task could not be built")?;
        let load = Arc::new(AtomicUsize::new(0));
        loads.push(load.clone());
        queues.push(SpinLock::new(RunQueue {
            current: None,
            queue: VecDeque::new(),
            idle: Arc::new(task),
            ticks: 0,
            load,
        }));
    }
    let cpus: &'static Cpus =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Cpus { queues, loads }));
    inject_scheduler(cpus);
    enable_preemption_on_cpu();
    ostd::boot::smp::register_ap_entry(enable_preemption_on_cpu);
    Ok(())
}

/// Records that the current CPU is running a process thread; returns the CPU's bit.
pub fn ran_process_thread() -> u64 {
    let cpu = disable_preempt().current_cpu().as_usize();
    let bit = if cpu < CPUS_MAX { 1u64 << cpu } else { 0 };
    RAN.fetch_or(bit, Ordering::Relaxed);
    bit
}

/// How many CPUs have run a process thread so far.
pub fn cpus_that_ran() -> u32 {
    RAN.load(Ordering::Relaxed).count_ones()
}

/// The thread a task runs, if it is a process thread.
fn thread_of(task: &Task) -> Option<&Arc<Thread>> {
    task.data().downcast_ref::<Arc<Thread>>()
}

/// Takes the pin waiting for `task`'s thread, if one does.
fn take_pin(task: &Task) -> Option<(Arc<Thread>, CpuId)> {
    let thread = thread_of(task)?;
    let mut pins = PINS.lock();
    let (_, cpu) = pins.remove(&thread.tid)?;
    PENDING.store(pins.len(), Ordering::Release);
    Some((thread.clone(), cpu))
}

/// Records that `thread` is to run on `cpu`, replacing any pin it had; pins of threads
/// that have ended go.
fn put_pin(thread: &Arc<Thread>, cpu: CpuId) {
    let mut pins = PINS.lock();
    pins.retain(|_, (t, _)| t.strong_count() > 0);
    pins.insert(thread.tid, (Arc::downgrade(thread), cpu));
    PENDING.store(pins.len(), Ordering::Release);
}

/// Drops `thread`'s pin, if one waits.
fn drop_pin(thread: &Thread) {
    let mut pins = PINS.lock();
    pins.remove(&thread.tid);
    PENDING.store(pins.len(), Ordering::Release);
}

/// The CPU running the caller now.
pub fn current_cpu() -> u32 {
    disable_preempt().current_cpu().as_usize() as u32
}

/// `thread_set_cpu`: runs `thread` on CPU `cpu` only, or, with `None`, wherever it is now
/// (the scheduler moves no thread on its own). `caller` is the calling thread. The
/// caller placing itself is on that CPU when this returns: it sleeps until the next timer
/// tick wakes it, and is queued there. Another thread moves the next time it is queued:
/// at once when it is asleep, when it next sleeps when it runs.
pub fn set_cpu(caller: &Arc<Thread>, thread: &Arc<Thread>, cpu: Option<u32>) -> Result<(), Error> {
    let Some(cpu) = cpu else {
        drop_pin(thread);
        return Ok(());
    };
    let cpu = usize::try_from(cpu)
        .ok()
        .and_then(|c| CpuId::try_from(c).ok())
        .ok_or(Error::InvalidArgs)?;
    put_pin(thread, cpu);
    if !Arc::ptr_eq(caller, thread) {
        return Ok(());
    }
    while current_cpu() as usize != cpu.as_usize() {
        if caller.killed() {
            drop_pin(caller);
            return Err(Error::BadState);
        }
        // Leave this CPU: asleep, it is on no queue, and the tick's wake queues it on
        // its new CPU. A tick before it sleeps finds it still running, keeps the pin,
        // and the loop sleeps again.
        let (waiter, waker) = Waiter::new_pair();
        let key = crate::clock::arm_wake(crate::clock::now(), waker);
        waiter.wait();
        crate::clock::disarm(key);
    }
    drop_pin(caller);
    Ok(())
}
