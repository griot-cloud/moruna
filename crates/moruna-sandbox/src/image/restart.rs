//! An instance its cgroup kills, started again (system design 3A, item 6.3v), as the node starts
//! again a task that passed its account (`moruna_micro::task`, 6.3g), by the one rule both
//! follow (`moruna_adapters::instances::restarted_limit`).
//!
//! - **Passing its room** is read from the instance's cgroup: its process ended and its
//!   `memory.events` counts an `oom_kill`, never guessed from how it ended.
//! - **The need** is the cgroup's `memory.peak` at the kill, read before the cgroup goes. **The
//!   room** is what the stage's ceiling leaves beside the other instances' limits, the
//!   instance's own returned. **The margin** is that of the line the stage is sized by, none
//!   without one.
//! - **The new account** is the larger of twice the one it had and the need widened by the
//!   margin, under half the room. One that does not grow, or a kernel that keeps state, fails
//!   the unit with the error it had.
//! - **Otherwise** a fresh process in a fresh cgroup at the grown account is given the slice the
//!   old one was answering; the slices after it follow in order, and the answers already taken
//!   stand. The grown account is the instance's from then on, and the other instances keep
//!   theirs; a second kill follows the same rule from it. The stage's note says each restart as
//!   the node's does.

use std::sync::Arc;

use moruna_adapters::instances;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{InitCtx, Kernel, MorunaError, Payload, Result, ResumePolicy};

use super::{ImageKernel, ImageState, kernel_error, lock};

impl ImageKernel {
    /// `batch` to the instance and its answer back ([`ImageState::exchange`]); given again to
    /// the instance started again when its cgroup killed it while it answered.
    pub(super) fn exchange(&self, st: &mut ImageState, batch: RecordBatch) -> Result<Payload> {
        loop {
            match st.exchange(batch.clone()) {
                Err(cause) => self.restart(st, cause)?,
                answered => return answered,
            }
        }
    }

    /// The instance of `st`, which failed with `cause`, started again at a larger account when
    /// its cgroup killed it (the module's rule); `cause` when it did not, its kernel keeps
    /// state, or its account cannot grow.
    fn restart(&self, st: &mut ImageState, cause: MorunaError) -> Result<()> {
        let killed = st.child.exit.is_some() && st.child.cgroup.oom_kills() > 0;
        if !killed || !matches!(self.hints().resume, ResumePolicy::Reinit) {
            return Err(cause);
        }
        // Read before the cgroup goes with the instance's place.
        let peak = st.child.cgroup.memory_peak_ever().unwrap_or(0);
        let limit = st.place.limit();
        let room = st.place.room();
        let margin = lock(&self.room)
            .as_ref()
            .and_then(|room| room.line.as_ref())
            .map_or(0.0, moruna_cost::Memory::margin);
        let Some(grown) = instances::restarted_limit(limit, peak, margin, room) else {
            return Err(cause);
        };
        let instance = st.index;
        lock(&self.restarted).insert(instance, grown);
        let ctx = InitCtx {
            instance,
            device: None,
            alloc: Arc::clone(&st.alloc),
        };
        let fresh = self.start(&ctx).map_err(|again| {
            kernel_error(format!(
                "{cause}; and its instance, which its cgroup killed, could not be started again: \
                 {again}"
            ))
        })?;
        // The old instance goes: its process first, then its plane, then its cgroup.
        *st = fresh;
        // In lockstep, the one posted and not answered is the slice it was answering.
        let posted = 1;
        let said = format!(
            "task {instance} passed its account of {limit} bytes (killed by its memory limit, \
             its peak {peak} bytes): started again with an account of {grown} bytes, its \
             {posted} unanswered pieces posted again"
        );
        tracing::warn!(target: "sandbox", image = %self.name, "{said}");
        lock(&self.restarts).push(said);
        Ok(())
    }
}
