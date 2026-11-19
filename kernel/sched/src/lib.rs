//! The scheduler's policy (SDD 20 f.11, SDD 23 f.12), the part that decides and so is tested
//! on the host. The kernel's `core/src/sched.rs` is its adapter to OSTD: it keeps one [`Queue`]
//! per CPU under that CPU's lock, passes the clock in, and turns each answer into OSTD's calls
//! (a reschedule asked of a CPU, a tick stretched or brought back).
//!
//! - **Order.** A CPU runs the tasks woken from a wait before the tasks a slice or a wake
//!   preempted and the new ones, each kind first in, first out; at most [`WOKEN_IN_A_ROW`]
//!   woken tasks run in a row while another waits, so a task that never sleeps still runs.
//! - **The slice.** A task that has run [`SLICE_TICKS`] ticks gives way to any task waiting on
//!   its CPU.
//! - **A wake.** A task woken from a wait by an interrupt or by a task on another CPU
//!   ([`Why::Woken`]) takes its CPU from the running task once that task has run
//!   [`WAKE_MIN_NS`] since it was picked, on every CPU, the boot processor's included. The CPU
//!   is asked to reschedule at once ([`Queue::push`] says so); a task that has run less gives
//!   way at the first tick after it reaches the minimum (1.11e: a woken thread used to wait
//!   out the rest of the running thread's 10 ms slice). These are a network completion's and
//!   a worker's wakes, which must run while every CPU computes.
//! - **A wake by the running task** ([`Why::WokenByCurrent`]: the waker is the task running on
//!   the woken task's CPU) does not preempt that waker: the woken task runs first at the next
//!   pick, within [`WOKEN_IN_A_ROW`], and the slice still applies. A producer that wakes its
//!   own consumer on its CPU keeps that CPU, so the two batch (the ring plane's engine fills a
//!   group of batches before its task drains them) instead of switching once a batch. Against
//!   any other task that takes the CPU later the woken task is an ordinary woken one, and the
//!   minimum applies: a request passing between threads of one CPU, each waking the next and
//!   waiting, is not held for a slice when the cap gives a computing thread its turn.
//! - **The idle task** gives way to anything, at once.
//! - **The tick** of a CPU other than the boot processor is stretched to [`STRETCH`] periods
//!   while it runs one task or none, and comes back when a task is queued there: such a CPU
//!   is always asked to reschedule.
//! - **Placement** ([`select`]): a task that names no CPU goes to its waker's CPU when at most
//!   the waker is runnable there, and otherwise (and a new task always) to the CPU with the
//!   fewest runnable tasks.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
};

/// Timer ticks (1 ms each) a task runs before another task waiting on its CPU may take it.
pub const SLICE_TICKS: u32 = 10;

/// What a task runs, in nanoseconds since it was picked, before a task woken from a wait may
/// take its CPU: 500 us. It bounds the switches wakes force on a computing thread at 2,000 a
/// second per CPU, under 1% of a CPU at a few microseconds a switch, while keeping a wake's
/// wait under one tick (1 ms).
pub const WAKE_MIN_NS: u64 = 500_000;

/// Woken tasks that may run in a row while a preempted or new task waits on their CPU: a task
/// that never sleeps still runs at least once every `WOKEN_IN_A_ROW + 1` turns.
pub const WOKEN_IN_A_ROW: u32 = 4;

/// The timer periods (1 ms each) a CPU other than the boot processor waits between ticks while
/// it runs one task or none (C5): what bounds a grace period, and a thread's wait for such a
/// CPU when it is in user mode with no other way into the kernel.
pub const STRETCH: u32 = 100;

/// Why a task is queued on a CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// New, or moved there.
    New,
    /// Woken from a wait by an interrupt, or by a task on another CPU.
    Woken,
    /// Woken from a wait by the task running on that CPU, which it does not preempt.
    WokenByCurrent,
}

/// A task woken from a wait, and the task that woke it if that was its CPU's running task: the
/// one task it does not take the CPU from at the minimum. Held weakly: a waker that has ended
/// is no one's waker, and its address is not given to another task while this waits.
type Woken<T> = (Arc<T>, Option<Weak<T>>);

/// Whether `waker` is `task`.
fn woke<T>(waker: &Option<Weak<T>>, task: &Arc<T>) -> bool {
    waker
        .as_ref()
        .is_some_and(|w| core::ptr::eq(w.as_ptr(), Arc::as_ptr(task)))
}

/// One CPU's run queue. `T` is the task; tasks are told apart by their `Arc`'s address.
pub struct Queue<T> {
    /// The running task: a task, the idle task, or `None` for the CPU's boot context.
    current: Option<Arc<T>>,
    /// Tasks woken from a wait, taken first.
    woken: VecDeque<Woken<T>>,
    /// Tasks a slice or a wake preempted, and new tasks.
    rest: VecDeque<Arc<T>>,
    /// Woken tasks run in a row while `rest` waited.
    woken_run: u32,
    idle: Arc<T>,
    /// Ticks since the current task was picked.
    ticks: u32,
    /// When the current task was picked, on the caller's clock (nanoseconds).
    picked_at: u64,
    /// Set by a yield that gives the CPU away: the next pick switches.
    yielding: bool,
    /// Set when this CPU was asked to reschedule to switch at once: the next pick switches.
    kicked: bool,
    /// The boot processor, which never stretches its tick: it serves the deadline queue.
    boot: bool,
    /// Whether this CPU's tick is stretched now.
    stretched: bool,
}

impl<T> Queue<T> {
    /// A CPU's queue, in its boot context, with its idle task.
    pub fn new(idle: Arc<T>, boot: bool) -> Queue<T> {
        Queue {
            current: None,
            woken: VecDeque::new(),
            rest: VecDeque::new(),
            woken_run: 0,
            idle,
            ticks: 0,
            picked_at: 0,
            yielding: false,
            kicked: false,
            boot,
            stretched: false,
        }
    }

    /// The running task, `None` in the boot context.
    pub fn current(&self) -> Option<&Arc<T>> {
        self.current.as_ref()
    }

    /// Whether `task` is this CPU's idle task.
    pub fn is_idle(&self, task: &Arc<T>) -> bool {
        Arc::ptr_eq(task, &self.idle)
    }

    /// Whether `task` is this CPU's current task or waits in its queue.
    pub fn holds(&self, task: &Arc<T>) -> bool {
        self.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, task))
            || self.woken.iter().any(|(t, _)| Arc::ptr_eq(t, task))
            || self.rest.iter().any(|t| Arc::ptr_eq(t, task))
    }

    /// Runnable tasks other than the idle task: the waiting ones and a running one.
    pub fn load(&self) -> usize {
        let busy = self.current.as_ref().is_some_and(|c| !self.is_idle(c));
        self.woken.len() + self.rest.len() + usize::from(busy)
    }

    /// Whether this CPU's tick is stretched.
    pub fn stretched(&self) -> bool {
        self.stretched
    }

    fn waiting(&self) -> bool {
        !self.woken.is_empty() || !self.rest.is_empty()
    }

    /// What the current task has run at `now`.
    fn ran(&self, now: u64) -> u64 {
        now.saturating_sub(self.picked_at)
    }

    /// Whether the running task gives way at `now`: the idle task to any task; a task to a
    /// woken task it did not wake itself once it has run [`WAKE_MIN_NS`], and to any once it
    /// has run a slice. The boot context never does: it leaves when it yields.
    fn gives_way(&self, now: u64) -> bool {
        match &self.current {
            None => false,
            Some(c) if self.is_idle(c) => self.waiting(),
            Some(c) => {
                (self.woken.iter().any(|(_, waker)| !woke(waker, c))
                    && self.ran(now) >= WAKE_MIN_NS)
                    || (self.waiting() && self.ticks >= SLICE_TICKS)
            }
        }
    }

    /// Queues `task` here at `now`, for `why`. Returns whether this CPU is to be asked to
    /// reschedule now: one running its idle task, or whose tick is stretched; and one whose task
    /// has run [`WAKE_MIN_NS`] when an interrupt or another CPU woke `task`. One in its boot
    /// context takes the task when it yields.
    pub fn push(&mut self, task: Arc<T>, why: Why, now: u64) -> bool {
        let (ask, at_once) = match &self.current {
            None => (self.stretched, false),
            Some(c) if self.is_idle(c) => (true, true),
            Some(_) => match why {
                // Woken elsewhere past the minimum: it takes the CPU at once.
                Why::Woken if self.ran(now) >= WAKE_MIN_NS => (true, true),
                // Woken elsewhere before it: the tick comes back and gives it the CPU at the
                // minimum. Woken by the running task: the tick comes back for the slice.
                Why::Woken | Why::WokenByCurrent => (self.stretched, false),
                // A new task takes a stretched CPU at once, as it always has.
                Why::New => (self.stretched, self.stretched),
            },
        };
        match why {
            Why::New => self.rest.push_back(task),
            Why::Woken => self.woken.push_back((task, None)),
            Why::WokenByCurrent => {
                // Its waker is the running task; the idle task and the boot context wake no one.
                let waker = self
                    .current
                    .as_ref()
                    .filter(|c| !self.is_idle(c))
                    .map(Arc::downgrade);
                self.woken.push_back((task, waker));
            }
        }
        self.kicked |= at_once;
        ask
    }

    /// A tick at `now`: whether the running task is to give way.
    pub fn tick(&mut self, now: u64) -> bool {
        self.ticks = self.ticks.saturating_add(1);
        self.kicked || self.gives_way(now)
    }

    /// A yield: whether the CPU goes to another task (one waits, or this is the boot context,
    /// which moves to a task). If so, the next pick switches.
    pub fn yield_now(&mut self) -> bool {
        self.yielding = self.waiting() || self.current.is_none();
        self.yielding
    }

    /// The running task leaves the CPU (it waits or exits): it is taken out, and the next pick
    /// chooses another.
    pub fn leave(&mut self) -> Option<Arc<T>> {
        self.current.take()
    }

    /// Picks the next task at `now`, if the running one is to give way: always after it left
    /// or yielded, and in the boot context; otherwise when it was asked to switch at once or
    /// [`Queue::tick`]'s rule says so. A task that gives way waits behind the preempted ones.
    /// Returns whether the current task changed.
    pub fn pick(&mut self, now: u64) -> bool {
        // Both requests end with this pick, whatever it decides.
        let yielded = core::mem::take(&mut self.yielding);
        let kicked = core::mem::take(&mut self.kicked);
        let forced = self.current.is_none() || yielded || kicked;
        if !forced && !self.gives_way(now) {
            return false;
        }
        // A running task that gives way stays runnable: it is one more that waits.
        let stays = self.current.as_ref().is_some_and(|c| !self.is_idle(c));
        let next = match self.take_next(stays) {
            Some(task) => task,
            None if self.current.is_none() => self.idle.clone(),
            None => return false,
        };
        self.ticks = 0;
        self.picked_at = now;
        if let Some(previous) = self.current.replace(next)
            && !self.is_idle(&previous)
        {
            self.rest.push_back(previous);
        }
        true
    }

    /// The next task to run: a woken one, unless [`WOKEN_IN_A_ROW`] of them have run in a row
    /// while another waited. `stays` says the running task gives way and waits too: the woken
    /// task that takes the CPU from it is the first of the row, so a task that never sleeps
    /// runs once every `WOKEN_IN_A_ROW + 1` turns, not one more.
    fn take_next(&mut self, stays: bool) -> Option<Arc<T>> {
        let others = stays || !self.rest.is_empty();
        // After a full row a preempted task has its turn (one always waits by then).
        let capped = others && self.woken_run >= WOKEN_IN_A_ROW && !self.rest.is_empty();
        if !self.woken.is_empty() && !capped {
            self.woken_run = if others { self.woken_run + 1 } else { 0 };
            self.woken.pop_front().map(|(task, _)| task)
        } else {
            self.woken_run = 0;
            self.rest.pop_front()
        }
    }

    /// Stretches the tick while nothing waits and a task (or the idle task) runs, on a CPU
    /// other than the boot processor, and brings it back when something waits. Returns the
    /// timer periods between ticks when they change.
    pub fn retick(&mut self) -> Option<u32> {
        let stretch = !self.boot && self.current.is_some() && !self.waiting();
        if stretch == self.stretched {
            return None;
        }
        self.stretched = stretch;
        Some(if stretch { STRETCH } else { 1 })
    }
}

/// Where a task that names no CPU goes, from CPU `here` among `count` CPUs whose runnable
/// tasks `load` gives: a woken one to `here` if at most its waker is runnable there, else
/// (and a new one always) the CPU with the fewest, `here` on a tie, then the first after it.
pub fn select(here: usize, count: usize, woken: bool, load: impl Fn(usize) -> usize) -> usize {
    let start_load = load(here);
    if woken && start_load <= 1 {
        return here;
    }
    let (mut best, mut best_load) = (here, start_load);
    for step in 1..count {
        let cpu = (here + step) % count;
        let l = load(cpu);
        if l < best_load {
            best = cpu;
            best_load = l;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::{sync::Arc, vec, vec::Vec};

    const MS: u64 = 1_000_000;

    fn queue(boot: bool) -> (Queue<u32>, Vec<Arc<u32>>) {
        let tasks = (1..=6).map(Arc::new).collect();
        (Queue::new(Arc::new(0), boot), tasks)
    }

    fn running(q: &Queue<u32>) -> u32 {
        **q.current().expect("a current task")
    }

    /// The CPU's first task, picked from the boot context (which yields) at `now`.
    fn start(q: &mut Queue<u32>, task: &Arc<u32>, now: u64) {
        assert!(
            !q.push(task.clone(), Why::New, now),
            "the boot context is not asked"
        );
        assert!(q.yield_now());
        assert!(q.pick(now));
        assert_eq!(running(q), **task);
    }

    #[test]
    fn the_boot_context_takes_the_first_task_when_it_yields_and_never_gives_way() {
        let (mut q, t) = queue(true);
        assert!(q.current().is_none());
        assert!(!q.tick(0));
        assert!(
            !q.push(t[0].clone(), Why::Woken, 0),
            "a wake does not ask it to reschedule"
        );
        assert!(!q.tick(50 * MS), "nor does the tick, after any time");
        assert!(q.yield_now());
        assert!(q.pick(50 * MS));
        assert_eq!(running(&q), 1);
        assert_eq!(q.load(), 1);
    }

    #[test]
    fn the_boot_context_with_nothing_queued_moves_to_the_idle_task() {
        let (mut q, _) = queue(false);
        assert!(q.yield_now(), "the boot context always leaves");
        assert!(q.pick(0));
        assert!(q.is_idle(q.current().unwrap()));
        assert_eq!(q.load(), 0);
    }

    #[test]
    fn new_tasks_run_first_in_first_out_a_slice_each() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        assert!(!q.push(t[1].clone(), Why::New, 0));
        assert!(!q.push(t[2].clone(), Why::New, 0));
        for tick in 1..SLICE_TICKS {
            assert!(!q.tick(u64::from(tick) * MS), "tick {tick}");
        }
        assert!(q.tick(10 * MS), "a slice is over");
        assert!(q.pick(10 * MS));
        assert_eq!(running(&q), 2);
        // The slice starts again: task 3 waits for it.
        assert!(!q.tick(11 * MS));
        assert!(
            !q.pick(11 * MS),
            "a spurious pick before the slice changes nothing"
        );
        for tick in 2..=SLICE_TICKS {
            q.tick(u64::from(10 + tick) * MS);
        }
        assert!(q.pick(20 * MS));
        assert_eq!(running(&q), 3);
        assert!(q.holds(&t[0]) && q.holds(&t[1]) && q.holds(&t[2]));
        assert!(!q.holds(&t[3]));
        assert_eq!(q.load(), 3);
    }

    #[test]
    fn a_task_alone_keeps_its_cpu_past_its_slice() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        for tick in 1..=3 * SLICE_TICKS {
            assert!(!q.tick(u64::from(tick) * MS));
        }
        assert!(!q.pick(30 * MS));
        assert!(!q.yield_now(), "a yield with nothing waiting keeps the CPU");
        assert_eq!(running(&q), 1);
    }

    #[test]
    fn a_wake_takes_the_cpu_at_once_from_a_task_past_the_minimum_on_the_boot_processor() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        let now = WAKE_MIN_NS;
        assert!(
            q.push(t[1].clone(), Why::Woken, now),
            "the CPU is asked to reschedule"
        );
        assert!(q.pick(now), "and the woken task takes it");
        assert_eq!(running(&q), 2);
        // It runs, then waits again: the computing task is back.
        assert!(q.leave().is_some());
        assert!(q.pick(now + 10_000));
        assert_eq!(running(&q), 1);
    }

    #[test]
    fn a_wake_before_the_minimum_runs_at_the_first_tick_past_it() {
        let (mut q, t) = queue(false);
        start(&mut q, &t[0], 0);
        assert_eq!(q.retick(), Some(STRETCH));
        // Back to every period while something waits; then a task picked at 1 ms.
        q.push(t[1].clone(), Why::New, 0);
        assert_eq!(q.retick(), Some(1));
        for tick in 1..=SLICE_TICKS {
            q.tick(u64::from(tick) * MS);
        }
        assert!(q.pick(10 * MS));
        assert_eq!(running(&q), 2);
        // Woken 0.1 ms after the pick: not yet.
        assert!(!q.push(t[2].clone(), Why::Woken, 10 * MS + 100_000));
        assert!(!q.pick(10 * MS + 100_000));
        assert!(!q.tick(10 * MS + 400_000), "still under the minimum");
        assert!(q.tick(10 * MS + WAKE_MIN_NS), "at the minimum");
        assert!(q.pick(10 * MS + WAKE_MIN_NS));
        assert_eq!(
            running(&q),
            3,
            "the woken task, before task 1, which a slice preempted"
        );
    }

    #[test]
    fn a_stretched_cpu_is_asked_but_waits_for_the_minimum() {
        let (mut q, t) = queue(false);
        start(&mut q, &t[0], 0);
        assert_eq!(q.retick(), Some(STRETCH));
        assert!(q.stretched());
        assert!(
            q.push(t[1].clone(), Why::Woken, 100_000),
            "asked, so its tick comes back"
        );
        assert!(
            !q.pick(100_000),
            "but its task keeps the CPU to the minimum"
        );
        assert_eq!(q.retick(), Some(1));
        assert!(!q.stretched());
        assert!(q.tick(WAKE_MIN_NS));
        assert!(q.pick(WAKE_MIN_NS));
        assert_eq!(running(&q), 2);
    }

    #[test]
    fn a_new_task_takes_a_stretched_cpu_at_once() {
        let (mut q, t) = queue(false);
        start(&mut q, &t[0], 0);
        q.retick();
        assert!(q.push(t[1].clone(), Why::New, 100_000));
        assert!(q.tick(100_000), "the request holds until the pick");
        assert!(q.pick(100_000));
        assert_eq!(running(&q), 2);
        assert_eq!(q.retick(), Some(1), "task 1 waits: the tick is back");
    }

    #[test]
    fn a_new_task_on_a_busy_cpu_waits_for_the_slice() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        assert!(
            !q.push(t[1].clone(), Why::New, 50 * MS),
            "not asked, even long after the pick"
        );
        assert!(!q.pick(50 * MS));
        for tick in 1..SLICE_TICKS {
            assert!(!q.tick(50 * MS + u64::from(tick)));
        }
        assert!(q.tick(51 * MS));
    }

    #[test]
    fn the_idle_task_gives_way_at_once() {
        let (mut q, t) = queue(false);
        assert!(q.yield_now());
        assert!(q.pick(0));
        assert!(!q.tick(MS), "with nothing to run it stays");
        assert!(!q.pick(MS));
        assert!(q.push(t[0].clone(), Why::New, 2 * MS));
        assert!(q.pick(2 * MS));
        assert_eq!(running(&q), 1);
        assert_eq!(q.load(), 1, "the idle task is not queued behind it");
        assert!(q.leave().is_some());
        assert!(q.pick(3 * MS));
        assert!(q.is_idle(q.current().unwrap()));
        assert!(
            q.push(t[1].clone(), Why::Woken, 3 * MS + 1),
            "woken onto an idle CPU"
        );
        assert!(
            q.tick(3 * MS + 2),
            "the idle task gives way at any tick too"
        );
    }

    #[test]
    fn a_request_to_switch_made_stale_by_a_pick_is_dropped() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        assert!(q.push(t[1].clone(), Why::Woken, MS));
        // Task 1 waits before the request arrives: the woken task is picked anyway.
        q.leave();
        assert!(q.pick(MS));
        assert_eq!(running(&q), 2);
        // The request lands: the task just picked keeps the CPU.
        assert!(!q.pick(MS + 1_000));
        assert_eq!(running(&q), 2);
    }

    #[test]
    fn woken_tasks_go_first_at_most_four_in_a_row() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        // Task 2 is preempted work waiting; tasks 3 to 6 and then task 1 are woken.
        q.push(t[1].clone(), Why::New, 0);
        let mut order = vec![];
        let mut now = 0;
        for task in &t[2..] {
            q.push(task.clone(), Why::Woken, now);
        }
        // Each woken task runs and waits again; task 1 (the first) leaves too.
        q.leave();
        q.push(t[0].clone(), Why::Woken, now);
        while q.load() > 0 {
            now += MS;
            assert!(q.pick(now) || q.current().is_some());
            let c = running(&q);
            order.push(c);
            q.leave();
        }
        assert_eq!(order, vec![3, 4, 5, 6, 2, 1]);
    }

    #[test]
    fn woken_tasks_alone_run_in_order_without_a_limit() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        for task in &t[1..] {
            q.push(task.clone(), Why::Woken, 0);
        }
        let mut order = vec![];
        q.leave();
        for step in 1..=5u64 {
            assert!(q.pick(step * MS));
            order.push(running(&q));
            q.leave();
        }
        assert_eq!(order, vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_task_preempted_by_a_wake_waits_behind_the_preempted_ones() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        q.push(t[1].clone(), Why::New, 0);
        for tick in 1..=SLICE_TICKS {
            q.tick(u64::from(tick) * MS);
        }
        assert!(q.pick(10 * MS));
        assert_eq!(running(&q), 2);
        assert!(q.push(t[2].clone(), Why::Woken, 11 * MS));
        assert!(q.pick(11 * MS));
        assert_eq!(running(&q), 3);
        q.leave();
        assert!(q.pick(11 * MS + 1));
        assert_eq!(
            running(&q),
            1,
            "task 1 was ahead of task 2, which the wake preempted"
        );
    }

    #[test]
    fn the_boot_processor_never_stretches_its_tick() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        assert_eq!(q.retick(), None);
        assert!(!q.stretched());
        q.leave();
        q.pick(MS);
        assert_eq!(q.retick(), None, "nor on its idle task");
    }

    #[test]
    fn a_cpu_stretches_with_one_task_or_none_and_not_in_its_boot_context() {
        let (mut q, t) = queue(false);
        assert_eq!(q.retick(), None, "the boot context ticks every period");
        start(&mut q, &t[0], 0);
        assert_eq!(q.retick(), Some(STRETCH));
        assert_eq!(q.retick(), None, "no change, no call");
        q.push(t[1].clone(), Why::New, 0);
        assert_eq!(q.retick(), Some(1));
        q.leave();
        q.pick(MS);
        q.leave();
        q.pick(2 * MS);
        assert!(q.is_idle(q.current().unwrap()));
        assert_eq!(
            q.retick(),
            Some(STRETCH),
            "the idle task alone stretches too"
        );
    }

    #[test]
    fn a_yield_moves_to_a_waiting_task_and_the_yielder_waits_behind_it() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        q.push(t[1].clone(), Why::New, 0);
        assert!(q.yield_now());
        assert!(q.pick(MS), "a yield switches though the slice is not over");
        assert_eq!(running(&q), 2);
        assert!(q.holds(&t[0]));
    }

    #[test]
    fn the_clock_before_it_starts_counts_nothing_against_a_task() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        // A clock that reads zero (not started) or earlier than the pick: no minimum reached.
        assert!(!q.push(t[1].clone(), Why::Woken, 0));
        assert!(!q.tick(0));
    }

    #[test]
    fn a_wake_by_the_running_task_keeps_its_cpu_to_the_slice_then_runs_first() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        q.push(t[1].clone(), Why::New, 0);
        // Task 1, long past the minimum, wakes task 3 on its own CPU: it keeps the CPU.
        assert!(
            !q.push(t[2].clone(), Why::WokenByCurrent, 5 * MS),
            "not asked"
        );
        assert!(!q.pick(5 * MS));
        for tick in 1..SLICE_TICKS {
            assert!(!q.tick(5 * MS + u64::from(tick) * 100_000), "tick {tick}");
        }
        assert!(q.tick(6 * MS), "the slice still applies");
        assert!(q.pick(6 * MS));
        assert_eq!(
            running(&q),
            3,
            "and the woken task goes first, before task 2"
        );
    }

    #[test]
    fn a_wake_by_the_running_task_runs_first_when_it_waits() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        q.push(t[1].clone(), Why::New, 0);
        for task in &t[2..4] {
            assert!(!q.push(task.clone(), Why::WokenByCurrent, 5 * MS));
        }
        assert!(!q.tick(5 * MS));
        q.leave();
        let mut order = vec![];
        for step in 1..=3u64 {
            assert!(q.pick(5 * MS + step));
            order.push(running(&q));
            q.leave();
        }
        assert_eq!(
            order,
            vec![3, 4, 2],
            "the woken in order, then the preempted one"
        );
    }

    #[test]
    fn a_wake_by_the_running_task_brings_a_stretched_tick_back_without_a_switch() {
        let (mut q, t) = queue(false);
        start(&mut q, &t[0], 0);
        assert_eq!(q.retick(), Some(STRETCH));
        assert!(
            q.push(t[1].clone(), Why::WokenByCurrent, 5 * MS),
            "asked, for the tick"
        );
        assert!(!q.pick(5 * MS), "but no switch");
        assert_eq!(q.retick(), Some(1));
        assert_eq!(running(&q), 1);
    }

    #[test]
    fn a_wake_from_elsewhere_still_preempts_beside_one_by_the_running_task() {
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        q.push(t[1].clone(), Why::WokenByCurrent, MS);
        assert!(
            q.push(t[2].clone(), Why::Woken, MS),
            "an interrupt's wake asks at once"
        );
        assert!(q.pick(MS));
        assert_eq!(
            running(&q),
            2,
            "the woken tasks in the order they were woken"
        );
        // Task 3, woken elsewhere, takes the CPU from task 2 at the minimum.
        assert!(!q.tick(MS + 100_000));
        assert!(q.tick(MS + WAKE_MIN_NS));
        assert!(q.pick(MS + WAKE_MIN_NS));
        assert_eq!(running(&q), 3);
        // Task 1 waits behind them; nothing woken is left to preempt task 3 early.
        assert!(!q.tick(MS + 2 * WAKE_MIN_NS));
    }

    #[test]
    fn a_chain_of_wakes_on_one_cpu_is_not_held_for_a_slice_by_a_computing_task() {
        // Task 1 computes. Tasks 2 and 3 pass a request back and forth on its CPU: each wakes
        // the other and waits, as a client and its server do.
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        let mut now = MS;
        assert!(
            q.push(t[1].clone(), Why::Woken, now),
            "the request arrives by an interrupt"
        );
        assert!(q.pick(now));
        let mut hops = vec![];
        while running(&q) != 1 {
            let me = running(&q);
            hops.push(me);
            let peer = if me == 2 { &t[2] } else { &t[1] };
            now += 10_000;
            assert!(
                !q.push(peer.clone(), Why::WokenByCurrent, now),
                "its own wake asks for nothing"
            );
            q.leave();
            assert!(q.pick(now));
        }
        // After four woken tasks in a row (the first took the CPU from task 1), the computing
        // task has its turn, and task 2, woken by task 3, waits.
        assert_eq!(hops, vec![2, 3, 2, 3]);
        assert!(q.holds(&t[1]));
        // Task 1 did not wake it: it gives way at the minimum, not at its slice.
        assert!(!q.tick(now + WAKE_MIN_NS - 1), "under the minimum");
        assert!(q.tick(now + WAKE_MIN_NS), "at the minimum");
        assert!(q.pick(now + WAKE_MIN_NS));
        assert_eq!(running(&q), 2, "the chain goes on");
        // And again: four more hops, then the computing task for the minimum.
        now += WAKE_MIN_NS;
        let mut more = vec![];
        while running(&q) != 1 {
            let me = running(&q);
            more.push(me);
            let peer = if me == 2 { &t[2] } else { &t[1] };
            now += 10_000;
            q.push(peer.clone(), Why::WokenByCurrent, now);
            q.leave();
            assert!(q.pick(now));
        }
        assert_eq!(more, vec![2, 3, 2, 3]);
        assert!(q.tick(now + WAKE_MIN_NS));
    }

    #[test]
    fn a_task_does_not_preempt_its_waker_when_the_waker_has_the_cpu_back() {
        // Task 1 wakes five tasks on its own CPU, then its slice ends.
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        for consumer in &t[1..] {
            assert!(!q.push(consumer.clone(), Why::WokenByCurrent, MS));
        }
        for tick in 1..SLICE_TICKS {
            assert!(
                !q.tick(u64::from(tick) * MS),
                "tick {tick}: its own wakes do not preempt it"
            );
        }
        assert!(q.tick(10 * MS), "its slice is over");
        // Four of them run in a row, each waiting again.
        let mut now = 10 * MS;
        let mut order = vec![];
        for _ in 0..4 {
            assert!(q.pick(now));
            order.push(running(&q));
            q.leave();
            now += 10_000;
        }
        assert_eq!(order, vec![2, 3, 4, 5]);
        // The cap gives task 1 its turn, and task 6, which task 1 woke, still waits: it does
        // not take the CPU from its waker at the minimum, only at the slice.
        assert!(q.pick(now));
        assert_eq!(running(&q), 1);
        assert!(q.holds(&t[5]));
        assert!(!q.tick(now + WAKE_MIN_NS));
        for tick in 2..SLICE_TICKS {
            assert!(!q.tick(now + u64::from(tick) * MS), "tick {tick}");
        }
        assert!(q.tick(now + 10 * MS), "the slice still applies");
        assert!(q.pick(now + 10 * MS));
        assert_eq!(running(&q), 6);
    }

    #[test]
    fn a_task_that_never_sleeps_runs_once_every_five_turns_beside_woken_ones() {
        // Task 1 computes alone; tasks 2 to 6 are woken by interrupts, one as each runs.
        let (mut q, t) = queue(true);
        start(&mut q, &t[0], 0);
        let mut now = MS;
        let mut turns = vec![];
        let mut next = 1;
        for _ in 0..10 {
            // The running task has run the minimum when the next wake comes.
            now += WAKE_MIN_NS;
            q.push(t[next].clone(), Why::Woken, now);
            if running(&q) != 1 {
                q.leave();
            }
            assert!(q.pick(now));
            turns.push(running(&q));
            next = if next == 5 { 1 } else { next + 1 };
        }
        // Four woken tasks, then task 1 (a woken one waiting), and again.
        assert_eq!(turns, vec![2, 3, 4, 5, 1, 6, 2, 3, 4, 1]);
    }

    #[test]
    fn a_wake_with_no_task_running_names_no_waker() {
        // The adapter never says "by the running task" from the boot context or the idle task,
        // but a task woken so is an ordinary woken one.
        let (mut q, t) = queue(true);
        assert!(!q.push(t[0].clone(), Why::Woken, 0));
        assert!(
            !q.push(t[1].clone(), Why::WokenByCurrent, 0),
            "the boot context"
        );
        assert!(q.yield_now());
        assert!(q.pick(0));
        assert_eq!(running(&q), 1);
        assert!(
            q.tick(WAKE_MIN_NS),
            "task 2 takes the CPU from task 1 at the minimum"
        );
        // The idle task.
        let (mut q, t) = queue(false);
        assert!(q.yield_now());
        assert!(q.pick(0));
        assert!(q.push(t[0].clone(), Why::WokenByCurrent, MS));
        assert!(q.push(t[1].clone(), Why::WokenByCurrent, MS));
        assert!(q.pick(MS));
        assert_eq!(running(&q), 1);
        assert!(q.tick(MS + WAKE_MIN_NS));
    }

    #[test]
    fn select_keeps_a_wake_with_its_waker_and_spreads_the_rest() {
        let loads = [1usize, 0, 2, 0];
        let load = |cpu: usize| loads[cpu];
        assert_eq!(select(0, 4, true, load), 0, "only the waker runs there");
        assert_eq!(
            select(0, 4, false, load),
            1,
            "a new task to the first idle CPU after"
        );
        assert_eq!(select(2, 4, true, load), 3, "the waker's CPU is busy");
        assert_eq!(select(3, 4, false, load), 3, "a tie keeps the current CPU");
        assert_eq!(select(0, 1, false, |_| 5), 0, "one CPU");
    }
}
