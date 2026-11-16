// SPDX-License-Identifier: MPL-2.0

//! Scheduling: the core's. What is left is the task's CPU record, and OSTD's scheduler
//! interface, so the layer's scheduler classes carry: a scheduler injected here is kept and
//! never driven, since the core decides what runs. Policies and nice values the program
//! sets are recorded by the layer as Linux reports them.

use alloc::sync::Arc;

use spin::Once;

use crate::{cpu::CpuId, task::Task};

/// Why a task is enqueued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueFlags {
    /// Spawn a new task.
    Spawn,
    /// Wake a sleeping task.
    Wake,
}

/// Why the current task is updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateFlags {
    /// Timer interrupt.
    Tick,
    /// Task waiting.
    Wait,
    /// Task yielding.
    Yield,
    /// Task exiting.
    Exit,
}

/// A scheduler, as OSTD defines one.
pub trait Scheduler<T = Task>: Sync + Send {
    /// Enqueues a runnable task.
    fn enqueue(&self, runnable: Arc<T>, flags: EnqueueFlags) -> Option<CpuId>;

    /// Runs `f` on the local run queue.
    fn local_rq_with(&self, f: &mut dyn FnMut(&dyn LocalRunQueue<T>));

    /// Runs `f` on the local run queue, mutably.
    fn mut_local_rq_with(&self, f: &mut dyn FnMut(&mut dyn LocalRunQueue<T>));
}

/// A CPU's run queue, as OSTD defines one.
pub trait LocalRunQueue<T = Task> {
    /// The running task.
    fn current(&self) -> Option<&Arc<T>>;

    /// Updates the running task; whether another should run.
    #[must_use]
    fn update_current(&mut self, flags: UpdateFlags) -> bool;

    /// The next task to run.
    fn pick_next(&mut self) -> &Arc<T> {
        self.try_pick_next().unwrap()
    }

    /// The next task to run, if any.
    fn try_pick_next(&mut self) -> Option<&Arc<T>>;

    /// Removes the running task.
    fn dequeue_current(&mut self) -> Option<Arc<T>>;
}

static SCHEDULER: Once<&'static dyn Scheduler<Task>> = Once::new();

/// Keeps the layer's scheduler; the core schedules, so it is never driven.
pub fn inject_scheduler(scheduler: &'static dyn Scheduler<Task>) {
    SCHEDULER.call_once(|| scheduler);
}

/// Enables preemption ticks on this CPU: the core preempts, so there is nothing to do.
pub fn enable_preemption_on_cpu() {}

pub mod info {
    //! The scheduling information a task keeps.

    use core::sync::atomic::{AtomicU32, Ordering};

    use crate::{cpu::CpuId, task::Task};

    /// The scheduling information of a task.
    #[derive(Debug)]
    pub struct TaskScheduleInfo {
        /// The CPU the task last ran on.
        pub cpu: AtomicCpuId,
    }

    /// An atomic `Option<CpuId>`.
    #[derive(Debug)]
    pub struct AtomicCpuId(AtomicU32);

    impl AtomicCpuId {
        const NONE: u32 = u32::MAX;

        /// Sets the CPU if none is set; returns the one set otherwise.
        pub fn set_if_is_none(&self, cpu_id: CpuId) -> Result<(), CpuId> {
            self.0
                .compare_exchange(
                    Self::NONE,
                    cpu_id.into(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .map(|_| ())
                .map_err(|prev| (prev as usize).try_into().unwrap())
        }

        /// Sets the CPU.
        pub fn set_anyway(&self, cpu_id: CpuId) {
            self.0.store(cpu_id.into(), Ordering::Relaxed);
        }

        /// Clears the CPU.
        pub fn set_to_none(&self) {
            self.0.store(Self::NONE, Ordering::Relaxed);
        }

        /// The CPU, if set.
        pub fn get(&self) -> Option<CpuId> {
            let val = self.0.load(Ordering::Relaxed);
            if val == Self::NONE {
                None
            } else {
                Some((val as usize).try_into().ok()?)
            }
        }
    }

    impl Default for AtomicCpuId {
        fn default() -> Self {
            Self(AtomicU32::new(Self::NONE))
        }
    }

    impl CommonSchedInfo for Task {
        fn cpu(&self) -> &AtomicCpuId {
            &self.schedule_info().cpu
        }
    }

    /// The scheduling information every task has.
    pub trait CommonSchedInfo {
        /// The CPU the task last ran on.
        fn cpu(&self) -> &AtomicCpuId;
    }
}
