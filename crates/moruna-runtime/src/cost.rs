//! The runtime's cost model (AR sections 5 and 7): the observation store it keeps measurements
//! in, the model a setting chooses from [`CostModels`], and that model's fitted state loaded from
//! the store.
//!
//! [`CostSettings`] is the configuration, built by whoever embeds the runtime or from a
//! surface's settings (never from the environment); [`CostSettings::open`] makes it real, and
//! refuses, with the cause, a store that cannot be opened or a model that is not registered.
//! The opened [`Cost`] is what warm-up writes to and refits, and what a planner asks:
//! [`Cost::model`] is the model `moruna_plan::PlanInputs::with_model` takes.

use std::path::PathBuf;
use std::sync::Arc;

use moruna_cost::{
    CostError, CostModel, CostModels, FitReport, LocalStore, ModelId, ObservationStore, Readiness,
};
use moruna_kernel::{MorunaError, Result};

/// Where observations are kept (AR section 7).
#[derive(Clone)]
pub enum StoreSetting {
    /// The local-directory adapter, rooted at this directory.
    Directory(PathBuf),
    /// The embedder's own adapter.
    Embedder(Arc<dyn ObservationStore>),
}

/// The cost configuration: the store, the models a setting can name, and the one it names.
#[derive(Clone)]
pub struct CostSettings {
    /// Where observations and fitted state are kept.
    pub store: StoreSetting,
    /// The registry of models.
    pub models: CostModels,
    /// The model in use, by name.
    pub model: String,
}

impl CostSettings {
    /// The local store at `dir`, the standard models, `loglinear`.
    pub fn directory(dir: impl Into<PathBuf>) -> CostSettings {
        CostSettings::with_store(StoreSetting::Directory(dir.into()))
    }

    /// The embedder's `store`, the standard models, `loglinear`.
    pub fn embedder(store: Arc<dyn ObservationStore>) -> CostSettings {
        CostSettings::with_store(StoreSetting::Embedder(store))
    }

    fn with_store(store: StoreSetting) -> CostSettings {
        CostSettings {
            store,
            models: CostModels::standard(),
            model: moruna_cost::loglinear::NAME.to_string(),
        }
    }

    /// Open the configured authority for a live run, learning from the same history as checks.
    pub fn open_online(self) -> Result<Arc<moruna_cost::Online>> {
        self.open().map(Cost::into_online)
    }

    /// Open the store, make the model, and make it ready: its fitted state loaded from the store
    /// when it saved some, else fitted from the store's observations and saved. State that does
    /// not load is refitted from the observations, which are the truth, and the readiness says
    /// why. Refused, with the cause: a store that cannot be opened or read, a model the registry
    /// does not have.
    pub fn open(self) -> Result<Cost> {
        let refuse = |msg: String| MorunaError::Config { name: "cost", msg };
        let store: Arc<dyn ObservationStore> = match self.store {
            StoreSetting::Directory(dir) => Arc::new(LocalStore::open(&dir).map_err(|e| {
                refuse(format!(
                    "the observation store {} cannot be opened: {e}",
                    dir.display()
                ))
            })?),
            StoreSetting::Embedder(store) => store,
        };
        let store = store
            .fitting_store()
            .map_err(|error| refuse(error.to_string()))?
            .unwrap_or(store);
        let mut model = self
            .models
            .make(&self.model)
            .map_err(|e| refuse(e.to_string()))?;
        let readiness = ready(model.as_mut(), store.as_ref()).map_err(|e| {
            refuse(format!(
                "the cost model `{}` could not be made ready from the store: {e}",
                self.model
            ))
        })?;
        Ok(Cost {
            store,
            models: self.models,
            name: self.model,
            model,
            readiness,
        })
    }
}

fn ready(
    model: &mut dyn CostModel,
    store: &dyn ObservationStore,
) -> std::result::Result<Readiness, CostError> {
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

/// The cost configuration, opened: the store and the model in use, ready.
pub struct Cost {
    store: Arc<dyn ObservationStore>,
    models: CostModels,
    name: String,
    model: Box<dyn CostModel>,
    readiness: Readiness,
}

impl Cost {
    /// Hand the already warmed/fitted model and its exact observation store to live learning.
    /// No second model, default store, sampled memory observation or refit is substituted.
    pub fn into_online(self) -> Arc<moruna_cost::Online> {
        Arc::new(moruna_cost::Online::new(
            moruna_cost::InUse::new(self.model, None),
            self.store,
        ))
    }

    /// The model in use: what a planner asks (`PlanInputs::with_model`) and warm-up fits.
    pub fn model(&self) -> &dyn CostModel {
        self.model.as_ref()
    }

    /// The model in use, to observe a measured unit.
    pub fn model_mut(&mut self) -> &mut dyn CostModel {
        self.model.as_mut()
    }

    /// Its id.
    pub fn model_id(&self) -> ModelId {
        self.model.id()
    }

    /// The store.
    pub fn store(&self) -> &Arc<dyn ObservationStore> {
        &self.store
    }

    /// The registry the model was chosen from.
    pub fn models(&self) -> &CostModels {
        &self.models
    }

    /// How the model became ready when it was opened, or last refitted.
    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    /// Fit the model again from everything the store holds now, save its state beside the
    /// records, and use it: what follows a warm-up's or a run's observations being written.
    pub fn refit(&mut self) -> std::result::Result<FitReport, CostError> {
        let mut model = self.models.make(&self.name)?;
        let report = model.fit(self.store.as_ref())?;
        self.store.save_state(&model.id(), &model.save()?)?;
        self.readiness = Readiness::Fitted(model.id(), report.clone());
        self.model = model;
        Ok(report)
    }
}

/// A stage's stable settings identity, before its real input schema is resolved.
#[derive(Clone, Debug)]
pub struct StageCost {
    /// The registered kind; a fused executable has an explicitly composite kind.
    pub kind: String,
    /// Canonical settings digest, identical to registration for an unfused task.
    pub settings: moruna_cost::Digest,
}

/// One live model/store authority and the stages whose real measurements teach it.
#[derive(Clone)]
pub struct RunLearning {
    /// Shared online state; may be transferred directly from `WarmUp::cost.into_online()`.
    pub online: Arc<moruna_cost::Online>,
    /// One identity per executable kernel stage, in scheduler order.
    pub stages: Vec<StageCost>,
}

/// Settings already used by function registration: packages have explicit invocation args;
/// Std/Python executable configuration is in their canonical fingerprint (including Python
/// declarations/lock bytes, excluding import aliases). Location, pins and sandbox metadata
/// are not new model dimensions. Other registered kinds retain their declared settings.
pub fn registration_settings(kind: &str, settings: &serde_json::Value) -> moruna_cost::Digest {
    let arguments = match kind {
        "package" | "wasm" | "pex" => settings
            .get("args")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        "std" | "python" => serde_json::json!({}),
        _ => settings.clone(),
    };
    crate::check::warmup::settings_digest(&arguments)
}

impl RunLearning {
    /// Match the built plan's executable stages to canonical logical settings, retaining fusion.
    /// Logical task names are deliberately excluded: renaming identical work is not new work.
    pub fn for_job(
        online: Arc<moruna_cost::Online>,
        job: &moruna_job::Job,
        parts: &crate::job::Parts,
    ) -> Result<Self> {
        let mut stages = Vec::with_capacity(parts.stages.len());
        for members in &parts.stages {
            let tasks = members
                .iter()
                .map(|&at| job.tasks.get(at))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    MorunaError::Plan("cost stage names an absent logical task".into())
                })?;
            if tasks.is_empty()
                || tasks
                    .iter()
                    .any(|task| task.role != moruna_job::Role::Kernel)
            {
                return Err(MorunaError::Plan(
                    "cost stage must contain executable kernel tasks".into(),
                ));
            }
            let (kind, settings) = if tasks.len() == 1 {
                (
                    tasks[0].kind.clone(),
                    registration_settings(
                        &tasks[0].kind,
                        &serde_json::Value::Object(tasks[0].settings.clone()),
                    ),
                )
            } else {
                ("fused".to_string(), crate::check::warmup::settings_digest(&serde_json::Value::Array(tasks.iter().map(|task| {
                    serde_json::json!({"kind": task.kind, "settings": registration_settings(&task.kind,&serde_json::Value::Object(task.settings.clone()))})
                }).collect())))
            };
            stages.push(StageCost { kind, settings });
        }
        Ok(Self { online, stages })
    }
    /// Actual executable keys installed in the live scheduler; registration conformance uses
    /// this same entry point, not a second reconstruction of fingerprints/schema/settings.
    pub fn keys(
        &self,
        kernels: &[Arc<dyn moruna_kernel::Kernel>],
        topology: &moruna_kernel::Topology,
        schemas: &[moruna_kernel::SourceSchema],
    ) -> Result<Vec<moruna_cost::CostKey>> {
        if self.stages.len() != kernels.len() {
            return Err(MorunaError::Plan(
                "cost identities must match executable kernel stages".into(),
            ));
        }
        self.stages
            .iter()
            .enumerate()
            .map(|(at, identity)| {
                let stage = (at + 1) as moruna_kernel::StageId;
                let input = topology.inputs(stage).first().copied().ok_or_else(|| {
                    MorunaError::Plan(format!("cost stage {stage} has no input schema"))
                })?;
                let schema = schemas.get(usize::from(input)).ok_or_else(|| {
                    MorunaError::Plan(format!("cost stage {stage} schema is absent"))
                })?;
                Ok(moruna_cost::CostKey {
                    role: moruna_job::Role::Kernel,
                    kind: identity.kind.clone(),
                    fingerprint: moruna_cost::Digest(kernels[at].fingerprint().0),
                    settings: identity.settings,
                    schema: moruna_cost::Digest(schema.hash()),
                })
            })
            .collect()
    }
}

/// One native store/model session used by registration, SDK and live engine execution.
/// Reloading replaces the session's future-run model; existing runs retain their actual Arc.
pub struct Session {
    settings: CostSettings,
    online: std::sync::Mutex<Option<Arc<moruna_cost::Online>>>,
}
impl Session {
    /// Construct without opening another authority or selecting a fallback store.
    pub fn new(settings: CostSettings) -> Self {
        Self {
            settings,
            online: std::sync::Mutex::new(None),
        }
    }
    /// Existing configured native authority, for the registration harness.
    pub fn settings(&self) -> CostSettings {
        self.settings.clone()
    }
    /// Load the actual fitted state once and share it with all future consumers.
    pub fn online(&self) -> Result<Arc<moruna_cost::Online>> {
        if let Some(online) = self
            .online
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(online);
        }
        let opened = self.settings.clone().open_online()?;
        Ok(self
            .online
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(opened)
            .clone())
    }
    /// Reload after real registration measurements have been written/fitted. Call before
    /// launch; this never mutates an already running model through another run's handle.
    pub fn refresh(&self) -> Result<()> {
        let opened = self.settings.clone().open_online()?;
        *self.online.lock().unwrap_or_else(|e| e.into_inner()) = Some(opened);
        Ok(())
    }
}

/// Where the local store is kept when a surface is given no directory:
/// `~/.moruna/observations`; `None` on a host with no home.
pub fn default_store_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().map(|home| home.join(".moruna").join("observations"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("moruna-cost-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn cm_t57_the_cost_configuration_opens_or_refuses() {
        let dir = scratch("open");
        let cost = CostSettings::directory(&dir).open().expect("opens");
        assert!(
            matches!(cost.readiness(), Readiness::Fitted(..)),
            "an empty store is fitted"
        );
        assert_eq!(cost.model_id(), moruna_cost::LogLinear::model_id());
        assert_eq!(cost.models().names().collect::<Vec<_>>(), vec!["loglinear"]);
        let again = CostSettings::embedder(Arc::clone(cost.store()))
            .open()
            .expect("opens");
        assert!(
            matches!(again.readiness(), Readiness::Loaded(_)),
            "saved state is loaded"
        );
        std::fs::write(dir.join("models").join("loglinear@1.state"), b"not state")
            .expect("corrupt");
        let mut refitted = CostSettings::directory(&dir).open().expect("opens");
        let Readiness::Refitted(_, _, why) = refitted.readiness() else {
            panic!(
                "corrupt state is refitted, saying why: {:?}",
                refitted.readiness()
            )
        };
        assert!(!why.is_empty());
        let report = refitted.refit().expect("refits");
        assert_eq!(report.observations, 0);
        assert!(matches!(refitted.readiness(), Readiness::Fitted(..)));
        let _ = refitted.model_mut();

        let mut unknown = CostSettings::directory(&dir);
        unknown.model = "nope".into();
        let err = unknown.open().err().expect("refused").to_string();
        assert!(err.contains("registry has loglinear"), "{err}");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").expect("file");
        let err = CostSettings::directory(file.join("store"))
            .open()
            .err()
            .expect("refused");
        assert!(err.to_string().contains("cannot be opened"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(default_store_dir().is_none_or(|d| d.ends_with(".moruna/observations")));
    }
    #[test]
    fn registration_model_transfers_to_live_learning_and_the_same_store_without_sampled_peak() {
        use moruna_cost::{CostKey, Digest, Inputs, Origin, Outcome, Target};
        let dir = scratch("shared-live");
        let mut cost = CostSettings::directory(&dir).open().unwrap();
        let store = cost.store().clone();
        let key = CostKey {
            role: moruna_job::Role::Kernel,
            kind: "std".into(),
            fingerprint: Digest([1; 32]),
            settings: Digest([2; 32]),
            schema: Digest([3; 32]),
        };
        let inputs = Inputs::default();
        let mut outcome = Outcome::new(Origin::WarmUp);
        outcome
            .set(Target::Duration, 0.01)
            .set(Target::RowsOut, 3.0);
        cost.model_mut().observe(&key, &inputs, &outcome);
        let warmed = cost.model().predict(&key, &inputs);
        let online = cost.into_online();
        let predicted = online.predict(&key, &inputs);
        assert_eq!(
            predicted.deciding.cost, warmed,
            "the actual warmed model is transferred, not reopened from stale state"
        );
        let mut live = Outcome::new(Origin::Run);
        live.set(Target::Duration, 0.01).set(Target::RowsOut, 3.0);
        online
            .settle(&key, &inputs, Some(&predicted), &live, 1)
            .unwrap();
        online.flush().unwrap();
        let history = store.scan(&key).unwrap();
        assert_eq!(history.observations.len(), 1);
        assert_eq!(
            history.observations[0].outcome.get(Target::PeakMemory),
            None
        );
        assert_eq!(
            history.observations[0].outcome.get(Target::Duration),
            Some(0.01)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
