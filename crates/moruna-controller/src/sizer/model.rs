//! `ModelSizer`: the morsel the run's cost model sizes (f.16; adaptive-runtime 0, G4; SDD 24
//! f.10).
//!
//! When a run has a cost model the embedder installs this sizer for each stage
//! (`Controller::set_sizer_factory`), so the morsel decision is the model's: `moruna_cost::morsel`
//! weighs the stage's per-batch overhead against its per-row cost, within the memory one unit
//! may take and the workers waiting for work. It is asked inside the controller's envelope (the
//! envelope's top is the memory a unit may take, and the answer is held to the envelope), so the
//! controller never has to clamp it and the two never pull against each other; the controller
//! still clamps, scores and may replace it like any sizer (RC-I2, f.8). It learns nothing itself:
//! the scheduler settles every unit into the model (10 f.22), the unit's peak included, which is
//! what the predicted peak it reports is scored against.

use std::sync::Arc;

use moruna_cost::{Basis, CostKey, Inputs, Online};

use super::{Envelope, Observation, Proposal, Sizer, SizerOutcome};
use crate::PlanSummary;

/// The units of a model's history after which this sizer trusts itself fully.
const TRUSTED_AFTER: f32 = 50.0;

/// The model sizer of one stage (module docs).
pub struct ModelSizer {
    online: Arc<Online>,
    key: CostKey,
    rows: u64,
    bytes_per_row: f64,
    confidence: f32,
}

impl ModelSizer {
    /// The sizer of the stage `key` names, asking `online`'s deciding model, over the rows and
    /// bytes per row of the run's `plan`.
    pub fn new(online: Arc<Online>, key: CostKey, plan: &PlanSummary) -> ModelSizer {
        let bytes_per_row = if plan.total_rows > 0 {
            plan.total_bytes as f64 / plan.total_rows as f64
        } else {
            0.0
        };
        ModelSizer {
            online,
            key,
            rows: plan.total_rows,
            bytes_per_row,
            confidence: 0.0,
        }
    }
}

impl Sizer for ModelSizer {
    fn propose(&mut self, obs: &Observation, envelope: &Envelope) -> Proposal {
        // RC-I3: the sizer honours damping itself.
        if obs.completions_since_adjust < obs.damping {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        }
        // Bytes per row: the plan's, else the last morsel's.
        let width = if self.bytes_per_row > 0.0 {
            self.bytes_per_row
        } else if obs.features.rows > 0 {
            obs.features.bytes as f64 / obs.features.rows as f64
        } else {
            return Proposal {
                morsel_target: obs.target,
                predicted_peak: None,
            };
        };
        let rows = self.rows.max(obs.features.rows).max(1);
        let probe = moruna_cost::morsel::PROBE_ROWS.min(rows);
        let inputs = Inputs::batch(probe, (probe as f64 * width).round() as u64);
        let morsel = self.online.morsel(
            &self.key,
            &inputs,
            rows,
            envelope.max as f64,
            u32::from(obs.active_workers.max(1)),
        );
        let bytes = (morsel.rows as f64 * width).round() as u64;
        let (target, _) = envelope.clamp(bytes);
        let (basis, seen) = {
            let p = self
                .online
                .predict(&self.key, &Inputs::batch(morsel.rows, bytes));
            (
                p.deciding.cost.peak_memory.basis,
                p.deciding.cost.duration.observations,
            )
        };
        self.confidence = (seen as f32 / TRUSTED_AFTER).min(1.0);
        // The peak it expects is the model's only once the model has measured peaks: a declared
        // default is no prediction, and is reported as none.
        let predicted_peak = matches!(basis, Basis::Measured | Basis::WarmUp)
            .then(|| morsel.peak.p50.at(target as f64 / width).max(0.0).round() as u64);
        Proposal {
            morsel_target: target,
            predicted_peak,
        }
    }

    fn observe(&mut self, _obs: &Observation, _outcome: &SizerOutcome) {}

    fn confidence(&self) -> f32 {
        self.confidence
    }

    fn name(&self) -> &'static str {
        "model"
    }
}
