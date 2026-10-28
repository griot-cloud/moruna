//! The scheduler (SDD 20, f.11): one run queue per CPU, an idle task per CPU, and a
//! time slice, injected into OSTD before any task runs.
//!
//! OSTD's default scheduler queues every task on the boot processor, so a kernel that
//! wants its other CPUs must bring its own. This one places a new task on the CPU with
//! the fewest runnable tasks, preferring the current one on a tie; places a woken task
//! on the waker's CPU when nothing else is runnable there (the waker of a request or a
//! reply usually blocks next, so the two take turns on one CPU instead of waking a
//! halted one for every message), and on the least loaded CPU otherwise; runs each
//! CPU's queue first in, first out; preempts a task that has run for a
//! slice while another waits on its CPU; and gives each CPU an idle task that halts
//! until an interrupt, so a CPU with nothing to run sleeps instead of spinning. A wake
//! that queues a task on a CPU running its idle task asks that CPU to reschedule. A
//! CPU's boot context is never preempted (the boot processor's runs the whole boot);
//! it moves to a task when it yields at the end of its entry.
//!
//! It also records which CPUs ran process threads (the post-schedule hook calls
//! [`ran_process_thread`]), so a boot can show the processes ran across every CPU.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ostd::{
    cpu::{CpuId, PinCurrentCpu, num_cpus},
    sync::SpinLock,
    task::{
        Task, TaskOptions, disable_preempt, halt_cpu,
        scheduler::{
            EnqueueFlags, LocalRunQueue, Scheduler, UpdateFlags, enable_preemption_on_cpu,
            info::CommonSchedInfo, inject_scheduler,
        },
    },
    util::id_set::Id,
};

/// Timer ticks (1 ms each) a task runs before another task waiting on its CPU may
/// take the CPU from it.
const SLICE_TICKS: u32 = 10;

/// The most CPUs whose use [`ran_process_thread`] records.
pub const CPUS_MAX: usize = 64;

/// Bit `i` is set once CPU `i` has run a process thread.
static RAN: AtomicU64 = AtomicU64::new(0);

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
        if let Some(task) = &current {
            task.cpu().set_to_none();
        }
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

impl Scheduler for Cpus {
    fn enqueue(&self, runnable: Arc<Task>, flags: EnqueueFlags) -> Option<CpuId> {
        // As OSTD's own scheduler does: a task that still names a CPU is still in that
        // CPU's queue (it is parking, or was never dequeued), and goes back there.
        let selected = self.select(flags);
        let (still_queued, target) = match runnable.cpu().set_if_is_none(selected) {
            Ok(()) => (false, selected),
            Err(cpu) => (true, cpu),
        };
        let mut rq = self.queues[target.as_usize()].disable_irq().lock();
        if still_queued && runnable.cpu().set_if_is_none(target).is_err() {
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
