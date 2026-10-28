//! Models by name, chosen by a setting, run in shadow (design section 5.2; SDD 24 d.3, f.5).
//!
//! Shaped like the registry of kinds (SDD 18): a model is an adapter registered by name, a
//! setting names the one in use, and an unknown name is refused naming the models there are.
//! Swapping is a setting and a fit, never a change to the planner or the scheduler.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{CostError, Result};
use crate::estimate::{Outcome, Target};
use crate::inputs::Inputs;
use crate::key::CostKey;
use crate::loglinear::LogLinearAdapter;
use crate::model::{CostModel, FitReport, ModelId};
use crate::store::{Observation, ObservationSource, ObservationStore, Predicted};

/// Makes models of one name.
pub trait ModelAdapter: Send + Sync {
    /// The name a setting chooses it by; the [`ModelId`] name of every model it makes.
    fn name(&self) -> &str;
    /// A model with no history.
    fn make(&self) -> Box<dyn CostModel>;
}

/// The cost section of a job (`cost.model`, `cost.shadow`): which model decides, and which, if
/// any, predicts beside it to be judged on the same runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSetting {
    /// The model that decides; the embedder's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A model that predicts in shadow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<String>,
}

/// The registry of models.
#[derive(Clone, Default)]
pub struct CostModels {
    adapters: BTreeMap<String, Arc<dyn ModelAdapter>>,
}

impl CostModels {
    /// A registry with no models.
    pub fn empty() -> CostModels {
        CostModels::default()
    }

    /// Every model this build has: `loglinear` with the standard defaults.
    pub fn standard() -> CostModels {
        let mut models = CostModels::empty();
        models.adapters.insert(
            crate::loglinear::NAME.to_string(),
            Arc::new(LogLinearAdapter::default()),
        );
        models
    }

    /// Register an adapter: refused when its name is not a name or is taken.
    pub fn register(&mut self, adapter: Arc<dyn ModelAdapter>) -> Result<()> {
        let name = adapter.name().to_string();
        if !moruna_job::is_kind_name(&name) {
            return Err(CostError::BadName { name });
        }
        if self.adapters.contains_key(&name) {
            return Err(CostError::Taken { name });
        }
        self.adapters.insert(name, adapter);
        Ok(())
    }

    /// The registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }

    /// A new model of `name`, refused naming the registered ones when there is none.
    pub fn make(&self, name: &str) -> Result<Box<dyn CostModel>> {
        let Some(adapter) = self.adapters.get(name) else {
            let known: Vec<&str> = self.names().collect();
            return Err(CostError::UnknownModel {
                name: name.to_string(),
                known: if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                },
            });
        };
        let model = adapter.make();
        if model.id().name != name {
            return Err(CostError::Mismatch {
                adapter: name.to_string(),
                made: model.id(),
            });
        }
        Ok(model)
    }

    /// The models a setting chooses, each ready: its fitted state loaded from `store` when it
    /// saved some, else fitted from the store's observations and saved (section 5.2: on first
    /// use a new model fits over the stored observations).
    pub fn open(
        &self,
        setting: &CostSetting,
        default: &str,
        store: &dyn ObservationStore,
    ) -> Result<InUse> {
        let deciding = self.make(setting.model.as_deref().unwrap_or(default))?;
        let shadow = setting
            .shadow
            .as_deref()
            .map(|name| self.make(name))
            .transpose()?;
        let mut in_use = InUse {
            deciding,
            shadow,
            readiness: Vec::new(),
        };
        let deciding_ready = ready(in_use.deciding.as_mut(), store)?;
        in_use.readiness.push(deciding_ready);
        if let Some(shadow) = in_use.shadow.as_mut() {
            let shadow_ready = ready(shadow.as_mut(), store)?;
            in_use.readiness.push(shadow_ready);
        }
        Ok(in_use)
    }
}

/// How a model became ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Its saved state was loaded.
    Loaded(ModelId),
    /// It had saved none, and was fitted from the observations.
    Fitted(ModelId, FitReport),
    /// Its saved state could not be loaded, and it was fitted again; the reason says why.
    Refitted(ModelId, FitReport, String),
}

fn ready(model: &mut dyn CostModel, store: &dyn ObservationStore) -> Result<Readiness> {
    let id = model.id();
    let failed = match store.load_state(&id)? {
        Some(bytes) => match model.load(&bytes) {
            Ok(()) => return Ok(Readiness::Loaded(id)),
            Err(e) => Some(e.to_string()),
        },
        None => None,
    };
    let report = model.fit(store)?;
    store.save_state(&id, &model.save()?)?;
    Ok(match failed {
        Some(why) => Readiness::Refitted(id, report, why),
        None => Readiness::Fitted(id, report),
    })
}

/// What the models in use predicted for one unit.
#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    /// The deciding model's.
    pub deciding: Predicted,
    /// The shadow's, when one runs.
    pub shadow: Option<Predicted>,
}

/// The models a run uses: one decides, one may predict in shadow, and both predictions are kept
/// with the outcome.
pub struct InUse {
    deciding: Box<dyn CostModel>,
    shadow: Option<Box<dyn CostModel>>,
    readiness: Vec<Readiness>,
}

impl InUse {
    /// Models in use, as they are (already fitted or loaded by the caller).
    pub fn new(deciding: Box<dyn CostModel>, shadow: Option<Box<dyn CostModel>>) -> InUse {
        InUse {
            deciding,
            shadow,
            readiness: Vec::new(),
        }
    }

    /// The model that decides: what the scheduler's loop calls `predict` on.
    pub fn deciding(&self) -> &dyn CostModel {
        self.deciding.as_ref()
    }

    /// The model in shadow.
    pub fn shadow(&self) -> Option<&dyn CostModel> {
        self.shadow.as_deref()
    }

    /// How each model became ready when it was opened, the deciding one first.
    pub fn readiness(&self) -> &[Readiness] {
        &self.readiness
    }

    /// Both models' predictions for one unit.
    pub fn predict(&self, key: &CostKey, inputs: &Inputs) -> Prediction {
        Prediction {
            deciding: Predicted {
                model: self.deciding.id(),
                cost: self.deciding.predict(key, inputs),
            },
            shadow: self.shadow.as_ref().map(|m| Predicted {
                model: m.id(),
                cost: m.predict(key, inputs),
            }),
        }
    }

    /// A unit finished: both models learn it, and it is kept with what each predicted.
    pub fn settle(
        &mut self,
        store: &dyn ObservationStore,
        key: &CostKey,
        inputs: &Inputs,
        prediction: Option<&Prediction>,
        outcome: &Outcome,
        at_ms: u64,
    ) -> Result<()> {
        let mut observation = Observation::new(key.clone(), inputs.clone(), outcome.clone(), at_ms);
        observation.model = Some(self.deciding.id());
        if let Some(p) = prediction {
            observation.predictions.push(p.deciding.clone());
            observation.predictions.extend(p.shadow.clone());
        }
        store.append(&observation)?;
        self.deciding.observe(key, inputs, outcome);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.observe(key, inputs, outcome);
        }
        Ok(())
    }

    /// Save each model's fitted state beside the records.
    pub fn save(&self, store: &dyn ObservationStore) -> Result<()> {
        store.save_state(&self.deciding.id(), &self.deciding.save()?)?;
        if let Some(shadow) = &self.shadow {
            store.save_state(&shadow.id(), &shadow.save()?)?;
        }
        Ok(())
    }
}

/// One model's record against the outcomes, per target.
#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    /// The target.
    pub target: Target,
    /// Units it predicted and that measured the target.
    pub n: u64,
    /// The median of `|log(p50 + floor) - log(measured + floor)|`.
    pub median_log_error: f64,
    /// The share of units whose measured value was at or below the p95.
    pub p95_coverage: f64,
}

/// How each model that predicted did, on the same units.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Comparison {
    /// By model, each with its scores by target.
    pub models: BTreeMap<ModelId, Vec<Score>>,
}

/// Judge every model that left predictions in `source` against the outcomes (section 5.2:
/// a swap is judged on the same runs).
pub fn compare(source: &dyn ObservationSource) -> Result<Comparison> {
    let mut errors: BTreeMap<(ModelId, Target), (Vec<f64>, u64)> = BTreeMap::new();
    source.scan_all(&mut |h| {
        for o in &h.observations {
            for p in &o.predictions {
                for target in Target::ALL {
                    let Some(measured) = o.outcome.get(target) else {
                        continue;
                    };
                    let e = p.cost.get(target);
                    let floor = target.unit().floor();
                    let slot = errors.entry((p.model.clone(), target)).or_default();
                    slot.0
                        .push(((e.p50 + floor).ln() - (measured + floor).ln()).abs());
                    if measured <= e.p95 {
                        slot.1 += 1;
                    }
                }
            }
        }
        Ok(())
    })?;
    let mut comparison = Comparison::default();
    for ((model, target), (mut errs, covered)) in errors {
        errs.sort_by(f64::total_cmp);
        let n = errs.len();
        comparison.models.entry(model).or_default().push(Score {
            target,
            n: n as u64,
            median_log_error: errs[n / 2],
            p95_coverage: covered as f64 / n as f64,
        });
    }
    Ok(comparison)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_has_the_default_model() {
        let models = CostModels::standard();
        assert_eq!(models.names().collect::<Vec<_>>(), vec!["loglinear"]);
        let model = models.make("loglinear").expect("made");
        assert_eq!(model.id(), crate::loglinear::LogLinear::model_id());
        let none = CostModels::empty()
            .make("loglinear")
            .err()
            .expect("refused");
        assert!(none.to_string().contains("has none"), "{none}");
        let unknown = models.make("nope").err().expect("refused");
        assert!(unknown.to_string().contains("has loglinear"), "{unknown}");
    }
}
