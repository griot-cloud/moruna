//! The admission rule (f.3, SC-I2, SC-I3).
//!
//! A stage is admissible when its input queue's head is resident for the spec that stage wants,
//! its output queue is not full, and it is stateless or one of its instances is free. Among the
//! admissible stages a worker takes the one whose output queue holds the fewest bytes; ties
//! break toward the later stage, which is the one closer to the sink. The rule is evaluated at
//! every pick, over a `placement.stats()` snapshot cached for a millisecond, and takes no lock
//! of the scheduler's own.

use std::sync::atomic::Ordering;

use amoru_kernel::{Locality, StageId};

use crate::shared::Shared;

/// The stage this worker should serve next, or `None` when nothing is admissible (f.3).
pub(crate) fn pick(shared: &Shared) -> Option<StageId> {
    if shared.gate.load(Ordering::SeqCst) {
        return None;
    }
    let stats = shared.placement_stats();
    let mut best: Option<(StageId, u64)> = None;
    for (index, entry) in shared.stages.iter().enumerate() {
        let stage = index as StageId + 1;
        if !instance_free(shared, index) {
            continue;
        }
        if shared.placement.is_full(stage) {
            continue;
        }
        if !shared
            .placement
            .peek_resident(stage - 1, entry.spec, Locality::Any)
        {
            continue;
        }
        let out_bytes = Shared::queue_bytes(&stats, stage);
        entry.out_bytes.store(out_bytes, Ordering::SeqCst);
        // Fewest output bytes wins; `<=` breaks a tie toward the later stage (SC-I2).
        match best {
            Some((_, bytes)) if out_bytes > bytes => {}
            _ => best = Some((stage, out_bytes)),
        }
    }
    best.map(|(stage, _)| stage)
}

/// A stateless stage is always free; a stateful one is admissible only while a slot is free
/// (f.3). The acquire itself may still lose the race, which f.4 calls a race, not a policy.
fn instance_free(shared: &Shared, index: usize) -> bool {
    let Some(pool) = shared.stages[index].pool.as_ref() else {
        return true;
    };
    let slots = pool.slots.lock().unwrap_or_else(|e| e.into_inner());
    slots.iter().any(|slot| !slot.in_use)
}
