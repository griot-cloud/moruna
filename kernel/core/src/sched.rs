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
//! Unpinned, a thread is free to run anywhere: the next time it is queued the scheduler
//! places it as it places a new thread.
//!
//! A worker's core runs its pinned worker (C5). While some core has no pinned thread, the
//! scheduler places an unpinned thread on such a core, and an unpinned thread queued on a
//! core with a pinned thread moves to one: a reactor, a ticker or a helper that first ran
//! there before the worker was pinned stops preempting it. When every core has a pinned
//! thread, an unpinned one shares a core with a worker.
//!
//! A CPU other than the boot processor that runs one thread, or none, stretches its tick
//! to [`STRETCH`] timer periods (C5, measured: each tick costs a pinned CPU about 19 us
//! under KVM): nothing on it needs the tick then, since only the boot processor's tick
//! serves the deadline queue. A thread queued on a stretched CPU asks it to reschedule
//! at once, which brings its tick back to every period. Grace periods do not wait for
//! the tick: a CPU passes a quiescent state each time it goes back to user mode and each
//! time its idle task wakes (OSTD, `ostd-adaptation/NOTICE.md`).

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

/// The timer periods (1 ms each) a CPU waits between ticks while it runs one thread or
/// none: what bounds a grace period, and a thread's wait for a stretched CPU that is in
/// user mode with no other way into the kernel.
const STRETCH: u32 = 100;

/// The most CPUs whose use [`ran_process_thread`] records.
pub const CPUS_MAX: usize = 64;

/// Bit `i` is set once CPU `i` has run a process thread.
static RAN: AtomicU64 = AtomicU64::new(0);

/// Where a pin puts a thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// On this CPU only.
    Cpu(CpuId),
    /// Not pinned: wherever the scheduler places it, as it places a new thread.
    Anywhere,
}

/// The pins not yet applied, by thread id: the thread, held weakly so a thread that ends
/// first leaves nothing behind, and where it is to run.
static PINS: SpinLock<BTreeMap<u64, (Weak<Thread>, Place)>, LocalIrqDisabled> =
    SpinLock::new(BTreeMap::new());
/// How many pins wait, read without the lock by every enqueue.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Whether unpinned threads keep off the cores of pinned ones while a core is free of them.
const PLACE_AWAY: bool = true;

/// Bit `i` is set while CPU `i` has a pinned thread.
static PINNED_CPUS: AtomicU64 = AtomicU64::new(0);
/// Bit `i` is set for every CPU there is.
static ALL_CPUS: AtomicU64 = AtomicU64::new(0);

/// The scheduler, for a pin that needs no move.
static SCHEDULER: SpinLock<Option<&'static Cpus>> = SpinLock::new(None);

/// Whether a CPU without a pinned thread is there to place an unpinned one on.
fn free_core() -> bool {
    PINNED_CPUS.load(Ordering::Relaxed) != ALL_CPUS.load(Ordering::Relaxed)
}

/// Whether an unpinned thread may be placed on `cpu`.
fn may_place(cpu: usize) -> bool {
    !PLACE_AWAY || !free_core() || PINNED_CPUS.load(Ordering::Relaxed) & bit(cpu) == 0
}

fn bit(cpu: usize) -> u64 {
    if cpu < CPUS_MAX { 1 << cpu } else { 0 }
}

struct RunQueue {
    current: Option<Arc<Task>>,
    queue: VecDeque<Arc<Task>>,
    idle: Arc<Task>,
    ticks: u32,
    /// Runnable tasks on this CPU other than its idle task, read without the lock by
    /// [`Cpus::select`].
    load: Arc<AtomicUsize>,
    /// The boot processor, which never stretches its tick: it serves the deadline queue.
    boot: bool,
    /// Whether this CPU's tick is stretched now.
    stretched: bool,
    /// Its number.
    cpu: usize,
    /// The threads pinned to it, by thread id, held weakly: one that ends goes at a tick.
    pinned: BTreeMap<u64, Weak<Thread>>,
}

impl RunQueue {
    fn is_idle(&self, task: &Arc<Task>) -> bool {
        Arc::ptr_eq(task, &self.idle)
    }

    /// Records that `thread` is pinned to this CPU, or no longer is, and publishes
    /// whether any thread is; pins of threads that have ended go.
    fn pin(&mut self, thread: &Arc<Thread>, pinned: bool) {
        if pinned {
            self.pinned.insert(thread.tid, Arc::downgrade(thread));
        } else {
            self.pinned.remove(&thread.tid);
        }
        self.publish_pins();
    }

    fn publish_pins(&mut self) {
        self.pinned.retain(|_, t| t.strong_count() > 0);
        if self.pinned.is_empty() {
            PINNED_CPUS.fetch_and(!bit(self.cpu), Ordering::Relaxed);
        } else {
            PINNED_CPUS.fetch_or(bit(self.cpu), Ordering::Relaxed);
        }
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

    /// Stretches this CPU's tick while nothing waits on it, and brings it back when
    /// something does. Called on the CPU itself, with its queue's lock held.
    fn retick(&mut self) {
        let stretch = !self.boot && self.current.is_some() && self.queue.is_empty();
        if stretch != self.stretched {
            self.stretched = stretch;
            ostd::timer::set_tick_stretch(if stretch { STRETCH } else { 1 });
        }
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
                self.retick();
                if !self.pinned.is_empty() {
                    self.publish_pins();
                }
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
        self.retick();
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
    ///
    /// While some CPU has no pinned thread, only such CPUs are chosen.
    fn select(&self, flags: EnqueueFlags) -> CpuId {
        let here = disable_preempt().current_cpu();
        let count = self.loads.len();
        let start = here.as_usize();
        if flags == EnqueueFlags::Wake
            && may_place(start)
            && self.loads[start].load(Ordering::Relaxed) <= 1
        {
            return here;
        }
        let mut best = None;
        for step in 0..count {
            let cpu = (start + step) % count;
            if !may_place(cpu) {
                continue;
            }
            let load = self.loads[cpu].load(Ordering::Relaxed);
            if best.is_none_or(|(_, least)| load < least) {
                best = Some((cpu, load));
            }
        }
        best.and_then(|(cpu, _)| CpuId::try_from(cpu).ok())
            .unwrap_or(here)
    }
}

impl Cpus {
    /// Moves `task`, which is being queued, to `place` if no queue holds it: `false` when
    /// its CPU's queue still has it (a wake while it runs), and the pin must wait.
    /// `Anywhere` leaves it with no CPU, so this enqueue chooses one.
    fn place(&self, task: &Arc<Task>, thread: &Arc<Thread>, place: Place) -> bool {
        let Some(old) = task.cpu().get() else {
            return match place {
                Place::Cpu(cpu) => task.cpu().set_if_is_none(cpu).is_ok(),
                Place::Anywhere => true,
            };
        };
        if place == Place::Cpu(old) {
            return true;
        }
        // Under its old queue's lock, so no enqueue queues it there meanwhile: one that
        // read the old CPU finds it changed once it has the lock, and retries.
        let mut rq = self.queues[old.as_usize()].disable_irq().lock();
        if rq.holds(task) {
            return false;
        }
        rq.pin(thread, false);
        match place {
            Place::Cpu(cpu) => task.cpu().set_anyway(cpu),
            Place::Anywhere => task.cpu().set_to_none(),
        }
        true
    }
}

impl Scheduler for Cpus {
    fn enqueue(&self, runnable: Arc<Task>, flags: EnqueueFlags) -> Option<CpuId> {
        let mut applied = None;
        if PENDING.load(Ordering::Acquire) != 0
            && let Some((thread, place)) = take_pin(&runnable)
        {
            if self.place(&runnable, &thread, place) {
                applied = Some((thread, place));
            } else {
                put_pin(&thread, place);
            }
        }
        // Only the first enqueue chooses a CPU. An assigned CPU is affinity,
        // not evidence that this task is still runnable: a waiter can already
        // be dequeued while its old context is completing the switch-out.
        let mut selected = self.select(flags);
        let (target, mut rq) = loop {
            let target = match runnable.cpu().set_if_is_none(selected) {
                Ok(()) => selected,
                Err(cpu) => cpu,
            };
            let mut rq = self.queues[target.as_usize()].disable_irq().lock();
            // A pin may have moved it while this waited for the lock.
            if runnable.cpu().get() != Some(target) {
                continue;
            }
            if let Some((thread, Place::Cpu(cpu))) = &applied
                && *cpu == target
            {
                rq.pin(thread, true);
            }
            // An unpinned thread on a pinned thread's core moves to a core with none,
            // while there is one; one this queue still holds stays for now.
            let unpinned = thread_of(&runnable).is_some_and(|t| !rq.pinned.contains_key(&t.tid));
            if PLACE_AWAY
                && unpinned
                && !rq.pinned.is_empty()
                && free_core()
                && !rq.holds(&runnable)
            {
                runnable.cpu().set_to_none();
                drop(rq);
                selected = self.select(flags);
                continue;
            }
            break (target, rq);
        };
        if rq.holds(&runnable) {
            return None;
        }
        // A CPU running its idle task, or one with its tick stretched, is asked to
        // reschedule; one still in its boot context takes the task when it yields.
        let idle = rq.current.as_ref().is_some_and(|c| rq.is_idle(c));
        let kick = idle || rq.stretched;
        rq.queue.push_back(runnable);
        rq.publish();
        kick.then_some(target)
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
        // An idle CPU switches no task, so it reports its quiescent state here.
        ostd::sync::report_quiescent_state();
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
    for cpu in 0..count {
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
            boot: cpu == 0,
            stretched: false,
            cpu,
            pinned: BTreeMap::new(),
        }));
        ALL_CPUS.fetch_or(bit(cpu), Ordering::Relaxed);
    }
    let cpus: &'static Cpus =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Cpus { queues, loads }));
    *SCHEDULER.lock() = Some(cpus);
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
fn take_pin(task: &Task) -> Option<(Arc<Thread>, Place)> {
    let thread = thread_of(task)?;
    let mut pins = PINS.lock();
    let (_, place) = pins.remove(&thread.tid)?;
    PENDING.store(pins.len(), Ordering::Release);
    Some((thread.clone(), place))
}

/// Records where `thread` is to run, replacing any pin it had; pins of threads that have
/// ended go.
fn put_pin(thread: &Arc<Thread>, place: Place) {
    let mut pins = PINS.lock();
    pins.retain(|_, (t, _)| t.strong_count() > 0);
    pins.insert(thread.tid, (Arc::downgrade(thread), place));
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

/// `thread_set_cpu`: runs `thread` on CPU `cpu` only, or, with `None`, unpinned: free to
/// run on any CPU, placed by the scheduler as a new thread is the next time it is queued.
/// `caller` is the calling thread. The caller placing itself on a CPU is there when this
/// returns: it sleeps until the next timer tick wakes it, and is queued there. Any other
/// change takes effect the next time the thread is queued: at once when it is asleep,
/// when it next sleeps when it runs.
pub fn set_cpu(caller: &Arc<Thread>, thread: &Arc<Thread>, cpu: Option<u32>) -> Result<(), Error> {
    let Some(cpu) = cpu else {
        put_pin(thread, Place::Anywhere);
        return Ok(());
    };
    let cpu = usize::try_from(cpu)
        .ok()
        .and_then(|c| CpuId::try_from(c).ok())
        .ok_or(Error::InvalidArgs)?;
    put_pin(thread, Place::Cpu(cpu));
    if !Arc::ptr_eq(caller, thread) {
        return Ok(());
    }
    if current_cpu() as usize == cpu.as_usize() {
        // Already there: no move records the pin, so it is recorded here.
        drop_pin(caller);
        if let Some(cpus) = *SCHEDULER.lock() {
            cpus.queues[cpu.as_usize()]
                .disable_irq()
                .lock()
                .pin(caller, true);
        }
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
