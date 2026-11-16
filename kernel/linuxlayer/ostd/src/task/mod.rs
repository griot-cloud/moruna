// SPDX-License-Identifier: MPL-2.0

//! Tasks in user space.
//!
//! A task is a thread of the personality's process. The core schedules threads, so there
//! is no run queue here: `run` starts a thread for the task, and `Task::current` reads
//! the calling thread's task slot (`backend`). A thread that enters the personality from
//! the program takes its task with [`Task::enter`].

pub mod atomic_mode;
mod preempt;
pub mod scheduler;
mod utils;

use core::{
    any::Any,
    borrow::Borrow,
    cell::Cell,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use self::utils::ForceSync;
pub use self::{
    preempt::{DisabledPreemptGuard, disable_preempt, halt_cpu},
    scheduler::info::{AtomicCpuId, TaskScheduleInfo},
};
use crate::{backend, prelude::*};

/// A task: a function run on a thread, and its data.
pub struct Task {
    #[expect(clippy::type_complexity)]
    func: ForceSync<Cell<Option<Box<dyn FnOnce() + Send>>>>,
    data: Box<dyn Any + Send + Sync>,
    local_data: ForceSync<Box<dyn Any + Send>>,
    schedule_info: TaskScheduleInfo,
    /// The thread running the task (`backend::thread_token`), zero when none does.
    thread: AtomicUsize,
}

impl core::fmt::Debug for Task {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Task").finish_non_exhaustive()
    }
}

impl Task {
    /// The current task, if the calling thread runs one.
    pub fn current() -> Option<CurrentTask> {
        let ptr = backend::current_slot() as *mut Task;
        // SAFETY: a non-zero slot holds a strong reference to the task the thread runs,
        // taken by `enter` and given back by `leave`.
        NonNull::new(ptr).map(|p| unsafe { CurrentTask::new(p) })
    }

    /// Makes `task` the calling thread's task until [`Task::leave`], and runs the
    /// post-schedule handler, as a switch to it would.
    pub fn enter(task: Arc<Task>) {
        let old = match backend::current_slot() {
            0 => None,
            // SAFETY: the slot held a strong reference from an earlier `enter`.
            old => Some(unsafe { Arc::from_raw(old as *const Task) }),
        };
        if let Some(old) = &old {
            old.thread.store(0, Ordering::Release);
        }
        task.thread
            .store(backend::thread_token(), Ordering::Release);
        backend::set_current_slot(Arc::into_raw(task) as usize);
        drop(old);
        if let Some(handler) = POST_SCHEDULE_HANDLER.get() {
            handler();
        }
    }

    /// Ends the calling thread's task, running the pre-schedule handler first, as a switch
    /// away from it would.
    pub fn leave() {
        if backend::current_slot() != 0
            && let Some(handler) = PRE_SCHEDULE_HANDLER.get()
        {
            handler(&crate::irq::disable_local());
        }
        let old = backend::current_slot();
        backend::set_current_slot(0);
        if old != 0 {
            // SAFETY: as in `enter`.
            let task = unsafe { Arc::from_raw(old as *const Task) };
            task.thread.store(0, Ordering::Release);
        }
    }

    /// Brings the task's thread back to the personality if it runs the program, as a
    /// kernel's interrupt would bring a CPU back to the kernel; a thread in the personality
    /// has its next wait return early instead (`moruna_sys::personality::thread_interrupt`).
    pub fn interrupt(&self) {
        let thread = self.thread.load(Ordering::Acquire);
        if thread != 0 {
            backend::interrupt(thread);
        }
    }

    /// Whether the task should yield: never, since the core preempts.
    pub fn need_yield() -> bool {
        false
    }

    /// Gives up the CPU for a moment.
    ///
    /// As in OSTD, where a context switch ends this CPU's part of a grace period, the
    /// RCU callbacks that are due run here: a task never yields in atomic mode.
    #[track_caller]
    pub fn yield_now() {
        backend::yield_now();
        let _ = crate::sync::reclaim();
    }

    /// Starts the task on a new thread.
    #[track_caller]
    pub fn run(self: &Arc<Self>) {
        let task = self.clone();
        let started = backend::spawn(Box::new(move || {
            // SAFETY: only the task's own thread takes its function, once.
            let func = unsafe { task.func.get() }.take();
            Task::enter(task);
            if let Some(func) = func {
                func();
            }
            Task::leave();
        }));
        assert!(started.is_ok(), "no thread for a new task");
    }

    /// The task's shared data.
    pub fn data(&self) -> &Box<dyn Any + Send + Sync> {
        &self.data
    }

    /// The task's scheduling information.
    pub fn schedule_info(&self) -> &TaskScheduleInfo {
        &self.schedule_info
    }
}

/// Options to create or spawn a task.
pub struct TaskOptions {
    func: Option<Box<dyn FnOnce() + Send>>,
    data: Option<Box<dyn Any + Send + Sync>>,
    local_data: Option<Box<dyn Any + Send>>,
}

impl TaskOptions {
    /// Options for a task that runs `func`.
    pub fn new<F>(func: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            func: Some(Box::new(func)),
            data: None,
            local_data: None,
        }
    }

    /// Sets the function.
    pub fn func<F>(mut self, func: F) -> Self
    where
        F: Fn() + Send + 'static,
    {
        self.func = Some(Box::new(func));
        self
    }

    /// Sets the shared data.
    pub fn data<T>(mut self, data: T) -> Self
    where
        T: Any + Send + Sync,
    {
        self.data = Some(Box::new(data));
        self
    }

    /// Sets the data only the task's own thread reaches.
    pub fn local_data<T>(mut self, data: T) -> Self
    where
        T: Any + Send,
    {
        self.local_data = Some(Box::new(data));
        self
    }

    /// Builds the task without starting it.
    pub fn build(self) -> Result<Task> {
        Ok(Task {
            func: ForceSync::new(Cell::new(self.func)),
            data: self.data.unwrap_or_else(|| Box::new(())),
            local_data: ForceSync::new(self.local_data.unwrap_or_else(|| Box::new(()))),
            schedule_info: TaskScheduleInfo {
                cpu: AtomicCpuId::default(),
            },
            thread: AtomicUsize::new(0),
        })
    }

    /// Builds the task and starts it.
    #[track_caller]
    pub fn spawn(self) -> Result<Arc<Task>> {
        let task = Arc::new(self.build()?);
        task.run();
        Ok(task)
    }
}

/// The current task.
#[derive(Debug)]
pub struct CurrentTask(NonNull<Task>);

impl !Send for CurrentTask {}
impl !Sync for CurrentTask {}

impl CurrentTask {
    /// # Safety
    ///
    /// `task` is the calling thread's task, kept alive while the thread runs it.
    unsafe fn new(task: NonNull<Task>) -> Self {
        Self(task)
    }

    /// The data only this task's thread reaches.
    pub fn local_data(&self) -> &(dyn Any + Send) {
        // SAFETY: only the task's own thread holds a `CurrentTask` for it.
        &**unsafe { self.local_data.get() }
    }

    /// A strong reference to the task.
    pub fn cloned(&self) -> Arc<Task> {
        let ptr = self.0.as_ptr();
        // SAFETY: the slot holds a strong reference, so the count is at least one.
        unsafe { Arc::increment_strong_count(ptr) };
        // SAFETY: the count was just raised for this reference.
        unsafe { Arc::from_raw(ptr) }
    }
}

impl Deref for CurrentTask {
    type Target = Task;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the task lives while its thread runs it.
        unsafe { self.0.as_ref() }
    }
}

impl AsRef<Task> for CurrentTask {
    fn as_ref(&self) -> &Task {
        self
    }
}

impl Borrow<Task> for CurrentTask {
    fn borrow(&self) -> &Task {
        self
    }
}

/// A setup every test needs, which a kernel's boot would have done: registered with
/// [`crate::ktest_setup!`] and run once, before the first test.
#[doc(hidden)]
pub struct KtestSetup(pub fn());

inventory::collect!(KtestSetup);

/// Runs a test's body in a task of its own, as OSTD's test runner does.
#[doc(hidden)]
pub fn __run_ktest<R>(body: impl FnOnce() -> R) -> R {
    // Tests run after boot, as OSTD's do: a start with one gibibyte of usable memory.
    crate::boot::init(crate::boot::BootInfo {
        memory_regions: alloc::vec![crate::boot::memory_region::MemoryRegion::new(
            0,
            1 << 30,
            crate::boot::memory_region::MemoryRegionType::Usable,
        )],
        ..Default::default()
    });
    let task = Arc::new(TaskOptions::new(|| {}).data(()).build().unwrap());
    Task::enter(task);
    static SETUP: spin::Once<()> = spin::Once::new();
    SETUP.call_once(|| {
        for setup in inventory::iter::<KtestSetup> {
            (setup.0)();
        }
    });
    let result = body();
    Task::leave();
    result
}

static PRE_SCHEDULE_HANDLER: spin::Once<fn(&crate::irq::DisabledLocalIrqGuard)> = spin::Once::new();
static POST_SCHEDULE_HANDLER: spin::Once<fn()> = spin::Once::new();

/// Injects a handler run when a task leaves its thread, as OSTD runs it before a context
/// switch.
pub fn inject_pre_schedule_handler(handler: fn(&crate::irq::DisabledLocalIrqGuard)) {
    PRE_SCHEDULE_HANDLER.call_once(|| handler);
}

/// Injects a handler run when a task enters a thread, as OSTD runs it after a context
/// switch: the core runs each task's thread with no switch between tasks, so entering is
/// when a task is switched in.
pub fn inject_post_schedule_handler(handler: fn()) {
    POST_SCHEDULE_HANDLER.call_once(|| handler);
}

#[cfg(ktest)]
mod test {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::{
        prelude::*,
        sync::Waiter,
        task::{Task, TaskOptions},
    };

    #[ktest]
    fn spawn_task_sees_itself() {
        let (waiter, waker) = Waiter::new_pair();
        let seen = Arc::new(AtomicBool::new(false));
        let seen2 = seen.clone();
        let task = TaskOptions::new(move || {
            let me = Task::current().unwrap();
            assert_eq!(*me.data().downcast_ref::<u32>().unwrap(), 7);
            seen2.store(true, Ordering::Release);
            waker.wake_up();
        })
        .data(7u32)
        .spawn()
        .unwrap();
        waiter.wait();
        assert!(seen.load(Ordering::Acquire));
        drop(task);
    }

    #[ktest]
    fn enter_and_leave() {
        // A test runs in a task of its own.
        assert!(Task::current().is_some());
        Task::leave();
        assert!(Task::current().is_none());
        let task = Arc::new(TaskOptions::new(|| {}).data(()).build().unwrap());
        Task::enter(task.clone());
        assert!(Arc::ptr_eq(&Task::current().unwrap().cloned(), &task));
        Task::leave();
        assert!(Task::current().is_none());
        assert_eq!(Arc::strong_count(&task), 1);
    }

    #[ktest]
    fn interrupt_reaches_a_running_task_only() {
        let interrupts = crate::backend::interrupts;
        let outer = Task::current().unwrap().cloned();
        let task = Arc::new(TaskOptions::new(|| {}).data(()).build().unwrap());
        let before = interrupts();
        task.interrupt();
        assert_eq!(interrupts(), before);
        Task::enter(task.clone());
        outer.interrupt();
        assert_eq!(interrupts(), before);
        task.interrupt();
        assert_eq!(interrupts(), before + 1);
        Task::enter(task.clone());
        task.interrupt();
        assert_eq!(interrupts(), before + 2);
        Task::leave();
        task.interrupt();
        assert_eq!(interrupts(), before + 2);
        Task::enter(outer);
    }
}
