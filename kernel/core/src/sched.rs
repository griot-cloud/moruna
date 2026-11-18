//! The scheduler (SDD 20, f.11): one run queue per CPU, an idle task per CPU, and a
//! time slice, injected into OSTD before any task runs. Its policy is `moruna-sched`'s,
//! tested on the host; this is its adapter to OSTD: a [`moruna_sched::Queue`] per CPU
//! under that CPU's lock, the kernel's clock passed in, and each answer turned into
//! OSTD's calls.
//!
//! OSTD's default scheduler queues every task on the boot processor, so a kernel that
//! wants its other CPUs must bring its own. This one places a new task on the CPU with
//! the fewest runnable tasks, preferring the current one on a tie; retains that CPU
//! across waits and wakes. OSTD dequeues a parking task before its old context has
//! finished switching out, so migrating it at that point could select a task still
//! in the foreground on another CPU. New tasks still spread across all CPUs. Each CPU
//! runs the tasks woken from a wait before the ones a slice or a wake preempted and the
//! new ones, at most [`moruna_sched::WOKEN_IN_A_ROW`] woken in a row while another waits;
//! preempts a task that has run for a slice while another waits on its CPU; and gives
//! each CPU an idle task that halts until an interrupt, so a CPU with nothing to run
//! sleeps instead of spinning. A CPU's boot context is never preempted (the boot
//! processor's runs the whole boot); it moves to a task when it yields at the end of its
//! entry.
//!
//! A wake by an interrupt or from another CPU (item 1.11e) takes the CPU from the running
//! task once that task has run [`moruna_sched::WAKE_MIN_NS`] since it was picked, on every
//! CPU, the boot processor's included: the wake asks the CPU to reschedule at once (OSTD's
//! inter-processor interrupt, then its preemption check on the way back to user mode); a
//! task that has run less gives way at the first tick past the minimum. A wake onto a CPU
//! running its idle task asks it to reschedule, as before. A woken thread used to wait out
//! the rest of the running thread's 10 ms slice, on the boot processor always and
//! elsewhere whenever its CPU had a second runnable thread.
//!
//! A wake made by the task running on the woken task's CPU (in task context, on that CPU)
//! does not preempt it: the woken task runs first at the next pick, and the slice still
//! applies. A producer that wakes its own consumer keeps its CPU, so the two batch: the
//! ring plane's engine fills a group of batches before its task drains them, where a
//! switch at each bell cost a call a batch on one CPU.
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
//! A CPU other than the boot processor that runs one thread, or none, stretches its tick
//! to [`moruna_sched::STRETCH`] timer periods (C5, measured: each tick costs a pinned CPU
//! about 19 us under KVM): nothing on it needs the tick then, since only the boot
//! processor's tick serves the deadline queue. A thread queued on a stretched CPU asks it
//! to reschedule at once, which brings its tick back to every period. Grace periods do
//! not wait for the tick: a CPU passes a quiescent state each time it goes back to user
//! mode and each time its idle task wakes (OSTD, `ostd-adaptation/NOTICE.md`).

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use moruna_abi::Error;
use moruna_sched::{Queue, Why};
use ostd::{
    cpu::{CpuId, PinCurrentCpu, num_cpus},
    irq::InterruptLevel,
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

/// One CPU's run queue: the policy's queue, and its count of runnable tasks published.
struct RunQueue {
    queue: Queue<Task>,
    /// Runnable tasks on this CPU other than its idle task, read without the lock by
    /// [`Cpus::select`].
    load: Arc<AtomicUsize>,
}

impl RunQueue {
    fn publish(&self) {
        self.load.store(self.queue.load(), Ordering::Relaxed);
    }

    /// Stretches this CPU's tick while nothing waits on it, and brings it back when
    /// something does. Called on the CPU itself, with its queue's lock held.
    fn retick(&mut self) {
        if let Some(periods) = self.queue.retick() {
            ostd::timer::set_tick_stretch(periods);
        }
    }
}

impl LocalRunQueue for RunQueue {
    fn current(&self) -> Option<&Arc<Task>> {
        self.queue.current()
    }

    fn update_current(&mut self, flags: UpdateFlags) -> bool {
        match flags {
            // The boot context (no current task) is never preempted: it leaves for a
            // task only when it yields, once the boot is done.
            UpdateFlags::Tick => {
                let preempt = self.queue.tick(crate::clock::now());
                self.retick();
                preempt
            }
            // The current task is leaving its CPU: the next one is the policy's, or the
            // idle task.
            UpdateFlags::Wait | UpdateFlags::Exit => true,
            // A yield from the boot context (no current task) moves to a task.
            UpdateFlags::Yield => self.queue.yield_now(),
        }
    }

    fn try_pick_next(&mut self) -> Option<&Arc<Task>> {
        // OSTD asks after a tick or a request to reschedule (the policy decides whether
        // the running task gives way), and after a yield, a wait or an exit (it does).
        let switched = self.queue.pick(crate::clock::now());
        self.publish();
        // A refusal still brings the tick back: a woken task waits for the minimum.
        self.retick();
        if switched { self.queue.current() } else { None }
    }

    fn dequeue_current(&mut self) -> Option<Arc<Task>> {
        let current = self.queue.leave();
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
    /// Where a task that names no CPU goes ([`moruna_sched::select`]).
    fn select(&self, flags: EnqueueFlags) -> CpuId {
        let here = disable_preempt().current_cpu();
        let best = moruna_sched::select(
            here.as_usize(),
            self.loads.len(),
            flags == EnqueueFlags::Wake,
            |cpu| self.loads[cpu].load(Ordering::Relaxed),
        );
        CpuId::try_from(best).unwrap_or(here)
    }
}

impl Cpus {
    /// Moves `task`, which is being queued, to `place` if no queue holds it: `false` when
    /// its CPU's queue still has it (a wake while it runs), and the pin must wait.
    /// `Anywhere` leaves it with no CPU, so this enqueue chooses one.
    fn place(&self, task: &Arc<Task>, place: Place) -> bool {
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
        let rq = self.queues[old.as_usize()].disable_irq().lock();
        if rq.queue.holds(task) {
            return false;
        }
        match place {
            Place::Cpu(cpu) => task.cpu().set_anyway(cpu),
            Place::Anywhere => task.cpu().set_to_none(),
        }
        true
    }
}

impl Scheduler for Cpus {
    fn enqueue(&self, runnable: Arc<Task>, flags: EnqueueFlags) -> Option<CpuId> {
        if PENDING.load(Ordering::Acquire) != 0
            && let Some((thread, place)) = take_pin(&runnable)
            && !self.place(&runnable, place)
        {
            put_pin(&thread, place);
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
        if rq.queue.holds(&runnable) {
            return None;
        }
        // The policy says whether the CPU is asked to reschedule: one running its idle
        // task or with its tick stretched, and one whose task has run the minimum when
        // an interrupt or another CPU woke this one; one still in its boot context takes
        // the task when it yields. Task code running on the target CPU is its current
        // task: that wake is the running task's own, and does not preempt it.
        let why = match flags {
            EnqueueFlags::Spawn => Why::New,
            EnqueueFlags::Wake
                if InterruptLevel::current().is_task_context()
                    && disable_preempt().current_cpu() == target =>
            {
                Why::WokenByCurrent
            }
            EnqueueFlags::Wake => Why::Woken,
        };
        let ask = rq.queue.push(runnable, why, crate::clock::now());
        rq.publish();
        ask.then_some(target)
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
            queue: Queue::new(Arc::new(task), cpu == 0),
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
