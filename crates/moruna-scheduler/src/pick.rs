//! The admission rule (f.3, SC-I2, SC-I3), over the run's DAG (contracts d.16).
//!
//! A stage is admissible when one of its input queues' heads is resident for the spec that stage
//! wants, none of its output queues is full, and it is stateless or one of its instances is
//! free. Among the admissible stages a worker takes the one whose output queues hold the fewest
//! bytes; ties break toward the later stage, which is the one closer to the sinks (the stages
//! are numbered so every queue goes forward). Of that stage's input ports, the one whose queue
//! holds the most bytes is served, so the fullest input drains first; ties break toward the lower
//! port. The rule is evaluated at every pick, over a `placement.stats()` snapshot cached for a
//! millisecond, and takes no lock of the scheduler's own. For a linear chain it is the chain's
//! rule exactly: each stage has one input and one output.

use std::sync::atomic::Ordering;

use moruna_kernel::{Locality, StageId};

use crate::shared::Shared;

/// The stage this worker should serve next and the input port to take from, or `None` when
/// nothing is admissible (f.3).
pub(crate) fn pick(shared: &Shared, worker: u16) -> Option<(StageId, u16)> {
    if shared.gated() {
        return None;
    }
    let stats = shared.placement_stats();
    let mut best: Option<(StageId, u16, u64)> = None;
    for (index, entry) in shared.stages.iter().enumerate() {
        if !crate::instances::admissible(shared, index, worker) {
            continue;
        }
        if entry.outputs.iter().any(|&q| shared.placement.is_full(q)) {
            continue;
        }
        let mut port: Option<(u16, u64)> = None;
        for (at, &q) in entry.inputs.iter().enumerate() {
            if !shared.placement.peek_resident(q, entry.spec, Locality::Any) {
                continue;
            }
            let held = Shared::queue_bytes(&stats, q);
            if port.is_none_or(|(_, most)| held > most) {
                port = Some((at as u16, held));
            }
        }
        let Some((port, _)) = port else {
            continue;
        };
        let out_bytes: u64 = entry
            .outputs
            .iter()
            .map(|&q| Shared::queue_bytes(&stats, q))
            .sum();
        entry.out_bytes.store(out_bytes, Ordering::SeqCst);
        // Fewest output bytes wins; `<=` breaks a tie toward the later stage (SC-I2).
        match best {
            Some((_, _, bytes)) if out_bytes > bytes => {}
            _ => best = Some((entry.stage, port, out_bytes)),
        }
    }
    best.map(|(stage, port, _)| (stage, port))
}
