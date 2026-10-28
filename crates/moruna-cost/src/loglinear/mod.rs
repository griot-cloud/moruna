//! The default model, `loglinear` (design section 5.3; SDD 24 f.1 to f.3).
//!
//! Per [`CostKey`] and per target, recursive least squares on logarithms with forgetting, plus
//! a fixed overhead fitted at the smallest sizes; the ratios kept as ratios; and the fallbacks of
//! section 5.4 in order: the function's history, the kind's prior pooled over every function of
//! the kind, the declared default.

mod defaults;
mod features;
mod ratio;
mod rls;

use std::collections::HashMap;

use moruna_job::Role;
use serde::{Deserialize, Serialize};

pub use defaults::Defaults;

use crate::error::{CostError, Result};
use crate::estimate::{Basis, Estimate, Origin, Outcome, Target, Unit, UnitCost};
use crate::exact;
use crate::inputs::{Column, Inputs};
use crate::key::CostKey;
use crate::model::{CostModel, FitReport, ModelId};
use crate::registry::ModelAdapter;
use crate::store::ObservationSource;
use features::{BYTES_PER_ROW, COEFFICIENTS, DISTINCT, FeatureStats, Features, REFERENCE, ROWS};
use ratio::Ratio;
use rls::TargetFit;

/// The name a setting chooses this model by.
pub const NAME: &str = "loglinear";
/// The version of its fitted state's meaning.
pub const VERSION: u32 = 1;
/// The forgetting factor λ of a function's own fit: each unit's weight is multiplied by λ for
/// every unit after it, so the fit rests on about 1 / (1 - λ) = 50 recent units. After a
/// function's behaviour changes, the old units weigh a tenth after 115 new ones and a hundredth
/// after 230. A run's units arrive by the hundred, so a change is followed within a run, and
/// 50 units of noise of 10% pin a slope to about 1%.
pub const FORGETTING: f64 = 0.98;
/// The kind's prior does not forget: it is the median behaviour across every function of the
/// kind, and a fit that forgot would be the behaviour of whichever function ran last.
pub const KIND_FORGETTING: f64 = 1.0;

/// The targets regressed on size; rows and bytes out are ratios.
const REGRESSED: [Target; 7] = [
    Target::Duration,
    Target::WorkerTime,
    Target::PeakMemory,
    Target::RetainedMemory,
    Target::StateGrowth,
    Target::ReadBytes,
    Target::SpillBytes,
];

/// The z of the 95th percentile, for the spread of a size taken from history.
const Z95: f64 = 1.645;

/// What one function, or one kind pooled, has taught the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Fit {
    #[serde(with = "exact::float")]
    lambda: f64,
    features: FeatureStats,
    targets: Vec<TargetFit>,
    selectivity: Ratio,
    out_row_bytes: Ratio,
    /// Whether any unit came from a real run (else all came from warm-up).
    measured: bool,
}

impl Fit {
    fn new(lambda: f64, defaults: &Defaults, role: Role) -> Fit {
        let reference = reference_inputs();
        let targets = REGRESSED
            .iter()
            .map(|&t| {
                let floor = t.unit().floor();
                let at = defaults.estimate(t, &reference, role).p50;
                let mut prior = [0.0; COEFFICIENTS];
                prior[0] = (at + floor).ln();
                // What grows with what before anything is measured: time and memory with rows,
                // memory and disk traffic with row width, state with distinct keys.
                if t == Target::StateGrowth {
                    prior[DISTINCT + 1] = 1.0;
                } else {
                    prior[ROWS + 1] = 1.0;
                }
                if t.unit() == Unit::Bytes {
                    prior[BYTES_PER_ROW + 1] = 1.0;
                }
                TargetFit::new(prior)
            })
            .collect();
        Fit {
            lambda,
            features: FeatureStats::default(),
            targets,
            selectivity: Ratio::default(),
            out_row_bytes: Ratio::default(),
            measured: false,
        }
    }

    fn basis(&self) -> Basis {
        if self.measured {
            Basis::Measured
        } else {
            Basis::WarmUp
        }
    }

    fn observe(&mut self, f: &Features, inputs: &Inputs, outcome: &Outcome) {
        let phi = self.features.phi(f);
        self.features.push(f, self.lambda);
        for (i, target) in REGRESSED.into_iter().enumerate() {
            if let Some(y) = outcome.get(target)
                && y.is_finite()
                && y >= 0.0
            {
                self.targets[i].observe(phi, y, target.unit().floor(), self.lambda);
            }
        }
        if let (Some(rows), Some(out)) = (inputs.data.rows, outcome.rows_out)
            && rows > 0
        {
            self.selectivity.push(out / rows as f64);
        }
        if let (Some(out), Some(bytes)) = (outcome.rows_out, outcome.bytes_out)
            && out > 0.0
        {
            self.out_row_bytes.push(bytes / out);
        }
        self.measured |= outcome.origin == Origin::Run;
    }

    /// Choose every target's overhead again (at the end of a fit).
    fn settle(&mut self) {
        for (i, target) in REGRESSED.into_iter().enumerate() {
            self.targets[i].choose_overhead(target.unit().floor(), self.lambda);
        }
    }

    /// The rows of the unit: given, or this history's typical rows, with a p95.
    fn rows(&self, inputs: &Inputs) -> Option<(f64, f64)> {
        if let Some(rows) = inputs.data.rows {
            return Some((rows as f64, rows as f64));
        }
        self.features.seen(ROWS).then(|| {
            let (mean, var) = self.features.moments(ROWS);
            (
                REFERENCE[ROWS] * mean.exp(),
                REFERENCE[ROWS] * (mean + Z95 * var.sqrt()).exp(),
            )
        })
    }
}

/// The unit the intercepts' priors are the cost of.
fn reference_inputs() -> Inputs {
    let mut inputs = Inputs::batch(
        REFERENCE[ROWS] as u64,
        (REFERENCE[ROWS] * REFERENCE[BYTES_PER_ROW]) as u64,
    );
    inputs.data.columns = vec![Column {
        name: "key".into(),
        distinct: Some(REFERENCE[DISTINCT] as u64),
        key: true,
        ..Column::default()
    }];
    inputs
}

/// One ratio's answer: median, p95, basis, observations.
type RatioAnswer = (f64, f64, Basis, u32);

fn count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The default model.
pub struct LogLinear {
    defaults: Defaults,
    keys: HashMap<CostKey, Fit>,
    kinds: HashMap<Role, HashMap<String, Fit>>,
}

/// The fitted state as saved.
#[derive(Serialize, Deserialize)]
struct Saved {
    model: ModelId,
    keys: Vec<(CostKey, Fit)>,
    kinds: Vec<(Role, String, Fit)>,
}

/// The first field of saved state, read before the rest so a state of another model is refused
/// as such.
#[derive(Deserialize)]
struct Header {
    model: ModelId,
}

impl LogLinear {
    /// A model with no history that falls back to `defaults`.
    pub fn new(defaults: Defaults) -> LogLinear {
        LogLinear {
            defaults,
            keys: HashMap::new(),
            kinds: HashMap::new(),
        }
    }

    /// Its id.
    pub fn model_id() -> ModelId {
        ModelId::new(NAME, VERSION)
    }

    /// The fixed overhead the function's fit of `target` holds, when it has one.
    pub fn overhead(&self, key: &CostKey, target: Target) -> Option<f64> {
        let i = REGRESSED.iter().position(|t| *t == target)?;
        self.keys.get(key).map(|fit| fit.targets[i].overhead())
    }

    fn learn(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome) {
        let f = Features::of(inputs);
        let defaults = &self.defaults;
        self.keys
            .entry(key.clone())
            .or_insert_with(|| Fit::new(FORGETTING, defaults, key.role))
            .observe(&f, inputs, outcome);
        self.kinds
            .entry(key.role)
            .or_default()
            .entry(key.kind.clone())
            .or_insert_with(|| Fit::new(KIND_FORGETTING, defaults, key.role))
            .observe(&f, inputs, outcome);
    }

    /// A ratio's answer by the fallbacks.
    fn ratio(
        own: Option<&Fit>,
        kind: Option<&Fit>,
        pick: fn(&Fit) -> &Ratio,
        declared: (f64, f64),
    ) -> RatioAnswer {
        if let Some(fit) = own
            && pick(fit).n() > 0
        {
            let (p50, p95) = pick(fit).percentiles();
            return (p50, p95, fit.basis(), count(pick(fit).n()));
        }
        if let Some(fit) = kind
            && pick(fit).n() > 0
        {
            let (p50, p95) = pick(fit).percentiles();
            return (p50, p95, Basis::Prior, count(pick(fit).n()));
        }
        (declared.0, declared.1, Basis::Default, 0)
    }
}

impl CostModel for LogLinear {
    fn id(&self) -> ModelId {
        LogLinear::model_id()
    }

    fn predict(&self, key: &CostKey, inputs: &Inputs) -> UnitCost {
        let f = Features::of(inputs);
        let own = self.keys.get(key);
        let kind = self
            .kinds
            .get(&key.role)
            .and_then(|kinds| kinds.get(key.kind.as_str()));
        let own_ctx = own.map(|fit| fit.features.context(&f));
        let kind_ctx = kind.map(|fit| fit.features.context(&f));
        let mut cost = UnitCost::zero();
        for (i, target) in REGRESSED.into_iter().enumerate() {
            let floor = target.unit().floor();
            let answer = |fit: &Fit, ctx: &features::Context, basis: Basis| {
                let t = &fit.targets[i];
                let (p50, p95, extrapolated) = t.predict(ctx, floor);
                Estimate {
                    p50,
                    p95,
                    unit: target.unit(),
                    basis,
                    observations: count(t.n()),
                    extrapolated,
                }
            };
            *cost.get_mut(target) = match (own, &own_ctx, kind, &kind_ctx) {
                (Some(fit), Some(ctx), _, _) if fit.targets[i].n() > 0 => {
                    answer(fit, ctx, fit.basis())
                }
                (_, _, Some(fit), Some(ctx)) if fit.targets[i].n() > 0 => {
                    answer(fit, ctx, Basis::Prior)
                }
                _ => self.defaults.estimate(target, inputs, key.role),
            };
        }

        // Rows and bytes out: the unit's rows times a ratio (section 5.3).
        let rows = own
            .and_then(|fit| fit.rows(inputs).map(|r| (r, fit.basis())))
            .or_else(|| kind.and_then(|fit| fit.rows(inputs).map(|r| (r, Basis::Prior))));
        let (sel50, sel95, sel_basis, sel_n) = match inputs.data.selectivity {
            Some(measured) if measured.is_finite() && measured >= 0.0 => {
                let widen = own
                    .map(|fit| fit.selectivity.percentiles())
                    .filter(|(p50, p95)| *p50 > 0.0 && p95 >= p50)
                    .map_or(1.0, |(p50, p95)| p95 / p50);
                let n = own.map_or(0, |fit| count(fit.selectivity.n()));
                (measured, measured * widen, Basis::Measured, n)
            }
            _ => LogLinear::ratio(
                own,
                kind,
                |fit| &fit.selectivity,
                (
                    self.defaults.selectivity,
                    self.defaults.selectivity * self.defaults.spread,
                ),
            ),
        };
        let (row50, row95, rows_basis) = match (inputs.data.rows, rows) {
            (Some(r), _) => (r as f64, r as f64, Basis::Measured),
            (None, Some(((p50, p95), basis))) => (p50, p95, basis),
            (None, None) => (
                self.defaults.rows,
                self.defaults.rows * self.defaults.unknown_size_spread,
                Basis::Default,
            ),
        };
        let rows_out = Estimate {
            p50: row50 * sel50,
            p95: (row95 * sel95).max(row50 * sel50),
            unit: Unit::Rows,
            basis: sel_basis.max(rows_basis),
            observations: sel_n,
            extrapolated: false,
        };
        let width = inputs
            .bytes_per_row()
            .unwrap_or(self.defaults.bytes_per_row);
        let (w50, w95, w_basis, w_n) = LogLinear::ratio(
            own,
            kind,
            |fit| &fit.out_row_bytes,
            (width, width * self.defaults.spread),
        );
        cost.rows_out = rows_out;
        cost.bytes_out = Estimate {
            p50: rows_out.p50 * w50,
            p95: (rows_out.p95 * w95).max(rows_out.p50 * w50),
            unit: Unit::Bytes,
            basis: rows_out.basis.max(w_basis),
            observations: w_n,
            extrapolated: false,
        };
        cost
    }

    fn observe(&mut self, key: &CostKey, inputs: &Inputs, outcome: &Outcome) {
        self.learn(key, inputs, outcome);
    }

    fn fit(&mut self, history: &dyn ObservationSource) -> Result<FitReport> {
        self.keys.clear();
        self.kinds.clear();
        let mut report = FitReport::default();
        history.scan_all(&mut |h| {
            if !h.observations.is_empty() {
                report.keys += 1;
            }
            report.observations += h.observations.len() as u64;
            report.torn += h.torn;
            report.summarised += h.summary.as_ref().map_or(0, |s| s.count);
            for o in &h.observations {
                self.learn(&o.key, &o.inputs, &o.outcome);
            }
            if let Some(fit) = self.keys.get_mut(&h.key) {
                fit.settle();
            }
            Ok(())
        })?;
        for kinds in self.kinds.values_mut() {
            for fit in kinds.values_mut() {
                fit.settle();
            }
        }
        Ok(report)
    }

    fn save(&self) -> Result<Vec<u8>> {
        let mut keys: Vec<(CostKey, Fit)> = self
            .keys
            .iter()
            .map(|(k, f)| (k.clone(), f.clone()))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        let mut kinds: Vec<(Role, String, Fit)> = self
            .kinds
            .iter()
            .flat_map(|(role, fits)| {
                fits.iter()
                    .map(move |(name, fit)| (*role, name.clone(), fit.clone()))
            })
            .collect();
        kinds.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let saved = Saved {
            model: self.id(),
            keys,
            kinds,
        };
        serde_json::to_vec(&saved).map_err(|e| CostError::State {
            model: self.id(),
            reason: format!("not serialisable: {e}"),
        })
    }

    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let unreadable = |reason: String| CostError::State {
            model: LogLinear::model_id(),
            reason,
        };
        let header: Header =
            serde_json::from_slice(bytes).map_err(|e| unreadable(format!("no model id: {e}")))?;
        if header.model != self.id() {
            return Err(CostError::WrongModel {
                expected: self.id(),
                found: header.model,
            });
        }
        let saved: Saved = serde_json::from_slice(bytes).map_err(|e| unreadable(e.to_string()))?;
        let all = saved
            .keys
            .iter()
            .map(|(_, f)| f)
            .chain(saved.kinds.iter().map(|(_, _, f)| f));
        for fit in all {
            if fit.targets.len() != REGRESSED.len() {
                return Err(unreadable(format!(
                    "{} targets where {} were expected",
                    fit.targets.len(),
                    REGRESSED.len()
                )));
            }
        }
        self.keys = saved.keys.into_iter().collect();
        self.kinds.clear();
        for (role, name, fit) in saved.kinds {
            self.kinds.entry(role).or_default().insert(name, fit);
        }
        Ok(())
    }
}

/// The registry's adapter for `loglinear`, with the defaults its models fall back to.
#[derive(Clone, Debug, Default)]
pub struct LogLinearAdapter {
    /// The declared default.
    pub defaults: Defaults,
}

impl ModelAdapter for LogLinearAdapter {
    fn name(&self) -> &str {
        NAME
    }

    fn make(&self) -> Box<dyn CostModel> {
        Box::new(LogLinear::new(self.defaults.clone()))
    }
}
