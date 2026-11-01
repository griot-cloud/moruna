//! The scheduling policy the simulator plays (SDD 24 d.6): a port, so the adaptive scheduler's
//! policies (KD7) are played by the same simulator, and the first adapter, `admission`.

/// What the policy is shown about one task.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TaskStatus {
    /// Its units (batches).
    pub units: u32,
    /// The next unit it will start; its units start in order.
    pub next_unit: u32,
    /// Its units admitted and not yet finished.
    pub running: u32,
    /// Its units finished.
    pub done: u32,
    /// Bytes of its outputs some consumer still needs, in memory or on disk.
    pub output_bytes: f64,
    /// A unit's median duration.
    pub median_seconds: f64,
}

/// A task whose next unit could start now: its inputs are finished and its kind's scaling curve
/// allows another unit at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    /// The task, by its place in the workload.
    pub task: usize,
    /// The unit.
    pub unit: u32,
    /// The memory it reserves: its 95th-percentile peak, or its output when that is larger.
    pub need_bytes: f64,
    /// Bytes of its inputs that are on disk and must be read back first.
    pub reload_bytes: f64,
    /// Whether it fits beside what is held now, without spilling.
    pub fits: bool,
}

/// What the policy sees when a worker is free.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    /// The simulated time, seconds.
    pub now: f64,
    /// Workers free.
    pub free_workers: u32,
    /// Memory held: reservations, buffers in memory and state.
    pub held_bytes: f64,
    /// The memory budget.
    pub budget_bytes: f64,
    /// Every task, in the workload's order.
    pub tasks: &'a [TaskStatus],
    /// The units that could start now, at least one.
    pub candidates: &'a [Candidate],
}

/// A scheduling policy: which ready unit the next free worker takes. The simulator holds the
/// hard rules (a unit starts only when it fits, spilling by known-future eviction when it does
/// not) whatever the policy chooses.
pub trait Policy: Send {
    /// Its name.
    fn name(&self) -> &str;
    /// Forget anything learned in a previous simulation.
    fn reset(&mut self) {}
    /// The candidate to start, by its index in `view.candidates`; `None`: wait for the next
    /// event.
    fn choose(&mut self, view: &View<'_>) -> Option<usize>;
}

/// Today's admission rule (SDD 10 f.3, `moruna-scheduler`'s `pick.rs`), over the DAG: among the
/// candidates that fit, the task whose outputs hold the fewest bytes, ties toward the later task
/// (the one closer to the sinks, since the workload lists tasks so every edge goes forward); when
/// none fits, the same rule over every candidate, and the simulator makes room for it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Admission;

impl Admission {
    /// Its name.
    pub const NAME: &'static str = "admission";
}

impl Policy for Admission {
    fn name(&self) -> &str {
        Admission::NAME
    }

    fn choose(&mut self, view: &View<'_>) -> Option<usize> {
        let any_fits = view.candidates.iter().any(|c| c.fits);
        let mut best: Option<(usize, f64, usize)> = None;
        for (i, c) in view.candidates.iter().enumerate() {
            if any_fits && !c.fits {
                continue;
            }
            let bytes = view.tasks[c.task].output_bytes;
            match best {
                Some((_, held, task)) if bytes > held || (bytes == held && c.task < task) => {}
                _ => best = Some((i, bytes, c.task)),
            }
        }
        best.map(|(i, _, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(task: usize, fits: bool) -> Candidate {
        Candidate {
            task,
            unit: 0,
            need_bytes: 1.0,
            reload_bytes: 0.0,
            fits,
        }
    }

    #[test]
    fn admission_takes_the_emptiest_outputs_toward_the_sinks() {
        let mut tasks = vec![TaskStatus::default(); 3];
        tasks[0].output_bytes = 10.0;
        tasks[1].output_bytes = 5.0;
        tasks[2].output_bytes = 5.0;
        let candidates = [candidate(0, true), candidate(2, true), candidate(1, true)];
        let view = View {
            now: 0.0,
            free_workers: 1,
            held_bytes: 0.0,
            budget_bytes: 1.0,
            tasks: &tasks,
            candidates: &candidates,
        };
        let mut policy = Admission;
        policy.reset();
        assert_eq!(policy.name(), "admission");
        assert_eq!(
            policy.choose(&view),
            Some(1),
            "the later of the two emptiest"
        );
        let candidates = [candidate(0, true), candidate(2, false)];
        assert_eq!(
            policy.choose(&View {
                candidates: &candidates,
                ..view
            }),
            Some(0),
            "one that fits before one that does not"
        );
        let candidates = [candidate(0, false), candidate(1, false)];
        assert_eq!(
            policy.choose(&View {
                candidates: &candidates,
                ..view
            }),
            Some(1)
        );
        assert_eq!(
            policy.choose(&View {
                candidates: &[],
                ..view
            }),
            None
        );
    }
}
