//! The observation store (design section 7; SDD 24 d.4, e.5).
//!
//! Raw observations are the truth: a model fits from them, so a new model learns the history the
//! old one saw. Fitted state is a cache beside them, keyed by model id.

mod frame;
mod local;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use local::LocalStore;

use crate::error::Result;
use crate::estimate::{Outcome, Target, UnitCost};
use crate::inputs::{INPUTS_VERSION, Inputs};
use crate::key::CostKey;
use crate::model::ModelId;

/// Records kept per key before the older ones are summarised (section 7).
pub const RETENTION_PER_KEY: usize = 10_000;

/// How much raw history a store keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// The last this many records of a key are kept raw; older ones are folded into its
    /// summary.
    pub per_key: usize,
}

impl Default for Retention {
    fn default() -> Retention {
        Retention {
            per_key: RETENTION_PER_KEY,
        }
    }
}

/// One prediction a model made for a unit, kept with the unit's outcome so models are judged on
/// the same runs (section 5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Predicted {
    /// The model.
    pub model: ModelId,
    /// What it predicted.
    pub cost: UnitCost,
}

/// One measured unit (section 7): the key, what was known, what was measured (with its origin,
/// the record's basis), when, and which model decided.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// What ran.
    pub key: CostKey,
    /// What was known when it was costed.
    pub inputs: Inputs,
    /// What it measured.
    pub outcome: Outcome,
    /// When it finished, milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The model that decided, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// What each model in use predicted: the deciding one first, then any in shadow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predictions: Vec<Predicted>,
}

impl Observation {
    /// An observation with no predictions recorded.
    pub fn new(key: CostKey, inputs: Inputs, outcome: Outcome, at_ms: u64) -> Observation {
        Observation {
            key,
            inputs,
            outcome,
            at_ms,
            model: None,
            predictions: Vec::new(),
        }
    }

    /// Refuse what a store must not keep (CM-I6): inputs of another version, a measured amount
    /// that is not finite and non-negative, a kind that is not a kind's name.
    pub fn check(&self) -> std::result::Result<(), String> {
        if self.inputs.version != INPUTS_VERSION {
            return Err(format!(
                "inputs version {}; this build reads version {INPUTS_VERSION}",
                self.inputs.version
            ));
        }
        if !moruna_job::is_kind_name(&self.key.kind) {
            return Err(format!("`{}` is not a kind's name", self.key.kind));
        }
        self.outcome.check()
    }
}

/// The count, mean, spread and range of one quantity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Moments {
    /// Values.
    pub n: u64,
    /// Their mean.
    pub mean: f64,
    /// The sum of squared deviations from the mean (Welford).
    pub m2: f64,
    /// The least.
    pub min: f64,
    /// The greatest.
    pub max: f64,
}

impl Moments {
    /// Add one value.
    pub fn push(&mut self, x: f64) {
        if self.n == 0 {
            self.min = x;
            self.max = x;
        } else {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }

    /// The sample variance; zero below two values.
    pub fn variance(&self) -> f64 {
        if self.n > 1 {
            self.m2 / (self.n - 1) as f64
        } else {
            0.0
        }
    }
}

/// What a key's records older than the retention say, kept when the records are not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// The key.
    pub key: CostKey,
    /// Records summarised.
    pub count: u64,
    /// The earliest.
    pub first_at_ms: u64,
    /// The latest.
    pub last_at_ms: u64,
    /// Rows in.
    pub rows: Moments,
    /// Each measured target.
    pub targets: BTreeMap<Target, Moments>,
}

impl Summary {
    /// An empty summary of `key`.
    pub fn new(key: CostKey) -> Summary {
        Summary {
            key,
            count: 0,
            first_at_ms: 0,
            last_at_ms: 0,
            rows: Moments::default(),
            targets: BTreeMap::new(),
        }
    }

    /// Fold one record in.
    pub fn fold(&mut self, observation: &Observation) {
        if self.count == 0 {
            self.first_at_ms = observation.at_ms;
            self.last_at_ms = observation.at_ms;
        } else {
            self.first_at_ms = self.first_at_ms.min(observation.at_ms);
            self.last_at_ms = self.last_at_ms.max(observation.at_ms);
        }
        self.count += 1;
        if let Some(rows) = observation.inputs.data.rows {
            self.rows.push(rows as f64);
        }
        for target in Target::ALL {
            if let Some(value) = observation.outcome.get(target) {
                self.targets.entry(target).or_default().push(value);
            }
        }
    }
}

/// One key's history: its summary, its raw records oldest first, and how many records were
/// skipped as torn writes.
#[derive(Clone, Debug, PartialEq)]
pub struct History {
    /// The key.
    pub key: CostKey,
    /// The records older than the retention, when there are any.
    pub summary: Option<Summary>,
    /// The kept records, oldest first; at most the retention's `per_key`.
    pub observations: Vec<Observation>,
    /// Records skipped because a write was cut short (a crash mid-append).
    pub torn: u64,
}

/// What a model reads to fit: every key's history.
pub trait ObservationSource {
    /// One key's history; empty when the key has none.
    fn scan(&self, key: &CostKey) -> Result<History>;
    /// Every key's history, one key at a time.
    fn scan_all(&self, each: &mut dyn FnMut(History) -> Result<()>) -> Result<()>;
}

/// The port an embedder implements (section 7): append-only records and fitted state beside
/// them. Appends from many threads and many runs at once are allowed.
pub trait ObservationStore: ObservationSource + Send + Sync {
    /// Keep one observation.
    fn append(&self, observation: &Observation) -> Result<()>;
    /// Keep a model's fitted state, replacing what that model saved before.
    fn save_state(&self, model: &ModelId, state: &[u8]) -> Result<()>;
    /// The fitted state `model` saved, if it saved any.
    fn load_state(&self, model: &ModelId) -> Result<Option<Vec<u8>>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Origin;
    use crate::key::Digest;
    use moruna_job::Role;

    fn key() -> CostKey {
        CostKey {
            role: Role::Kernel,
            kind: "std".into(),
            fingerprint: Digest([1; 32]),
            settings: Digest([2; 32]),
            schema: Digest([3; 32]),
        }
    }

    #[test]
    fn moments_and_summary() {
        let mut m = Moments::default();
        assert_eq!(m.variance(), 0.0);
        for x in [2.0, 4.0, 6.0] {
            m.push(x);
        }
        assert_eq!((m.n, m.mean, m.min, m.max), (3, 4.0, 2.0, 6.0));
        assert!((m.variance() - 4.0).abs() < 1e-12);
        let mut s = Summary::new(key());
        let mut outcome = Outcome::new(Origin::Run);
        outcome.set(Target::Duration, 1.0);
        s.fold(&Observation::new(
            key(),
            Inputs::batch(10, 100),
            outcome.clone(),
            50,
        ));
        s.fold(&Observation::new(key(), Inputs::default(), outcome, 20));
        assert_eq!((s.count, s.first_at_ms, s.last_at_ms), (2, 20, 50));
        assert_eq!(s.rows.n, 1);
        assert_eq!(s.targets[&Target::Duration].n, 2);
    }

    #[test]
    fn observation_check() {
        let ok = Observation::new(key(), Inputs::default(), Outcome::new(Origin::Run), 1);
        assert!(ok.check().is_ok());
        let mut bad = ok.clone();
        bad.inputs.version = 99;
        assert!(bad.check().unwrap_err().contains("version 99"));
        let mut bad = ok.clone();
        bad.key.kind = "Not A Kind".into();
        assert!(bad.check().unwrap_err().contains("kind's name"));
        let mut bad = ok;
        bad.outcome.set(Target::Duration, f64::INFINITY);
        assert!(bad.check().is_err());
        assert_eq!(Retention::default().per_key, RETENTION_PER_KEY);
    }
}
