//! Where a live run is, for a simulation to start from (SDD 24 f.13): what the scheduler's
//! lookahead plays forward (`architecture/adaptive-runtime.md` 9.4 step 3).

/// One task's place in a live run. Its units start in order, so the units finished are its
/// first `done`, and the ones under way the next `running.len()`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskStart {
    /// Units finished.
    pub done: u32,
    /// The units under way after them, each with the seconds of work it has left at full speed.
    pub running: Vec<f64>,
    /// Of the finished units whose output a consumer still needs, the ones whose output is on
    /// disk, by unit; the others' are in memory.
    pub on_disk: Vec<u32>,
}

/// A live run's place, task by task in the workload's order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Start {
    /// Every task.
    pub tasks: Vec<TaskStart>,
}

impl Start {
    /// A start where nothing has run: what [`super::Simulator::run`] starts from.
    pub fn fresh(tasks: usize) -> Start {
        Start {
            tasks: vec![TaskStart::default(); tasks],
        }
    }
}
