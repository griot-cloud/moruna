//! The standard kernels of `moruna-kernels` (MH 4.9) as the runnable kind `std`: built by name
//! and arguments, adjacent ones fused into one stage where their combination is one.

use std::any::Any;
use std::sync::Arc;

use moruna_kernel::{Kernel, Result};
use moruna_kernels::StdKernel;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Context, Declaration, Entry, Fact, Flow, Fuse, Isolation, Needs, Role, RunnableKind, Stage,
    check_pin, settings_schema,
};

/// The `std` kind.
pub struct Std;

/// A `std` entry's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The standard kernel's name.
    pub name: String,
    /// Its arguments, a JSON object whose keys the kernel's name decides (MH 4.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// The fingerprint the kernel must have, hex, optionally prefixed `algo:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl RunnableKind for Std {
    fn declaration(&self) -> Declaration {
        let mut settings = settings_schema::<Settings>(
            "std",
            vec![
                json!({"name": "cast", "args": {"columns": {"a": "double"}}}),
                json!({"name": "select", "args": {"columns": ["a", "b"]}}),
            ],
        );
        // The names this build has, so a screen offers them.
        if let Some(name) = settings.pointer_mut("/properties/name")
            && let Some(name) = name.as_object_mut()
        {
            name.insert("enum".into(), json!(moruna_kernels::NAMES));
        }
        Declaration {
            name: "std".into(),
            role: Role::Kernel,
            summary: "A standard kernel of moruna-kernels, built by name and arguments".into(),
            settings,
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::BySettings,
            isolation: Isolation::Builtin,
        }
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        let args = settings.args.unwrap_or(serde_json::Value::Null);
        let kernel = StdKernel::new(&settings.name, &args)
            .map_err(|e| crate::job::SpecError::new(entry.field().to_string(), e.to_string()))?;
        check_pin(entry, settings.fingerprint.as_deref(), &kernel)?;
        Ok(stage(kernel))
    }
}

fn stage(kernel: StdKernel) -> Stage {
    let kernel = Arc::new(kernel);
    let mut stage = Stage::of(Arc::clone(&kernel) as Arc<dyn Kernel>);
    stage.fuse = Some(Arc::new(Fusing(kernel)));
    stage
}

/// A standard kernel's stage, which joins the next when it is a standard kernel's and their
/// combination is one stage (MH 4.9).
struct Fusing(Arc<StdKernel>);

impl Fuse for Fusing {
    fn fuse(&self, next: &dyn Fuse) -> Option<Stage> {
        let next = next.as_any().downcast_ref::<Fusing>()?;
        moruna_kernels::fuse(&self.0, &next.0).map(stage)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
