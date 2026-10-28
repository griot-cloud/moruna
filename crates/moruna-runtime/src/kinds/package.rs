//! Function packages (16) as the runnable kind `package`: one file, pinned by its fingerprint,
//! run by the runtime its extension names, a `.wasm` in its sandbox or a `.pex` in a confined
//! child (17, decision FP1).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moruna_kernel::{MorunaError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::ports::{self, PortDoc};
use super::{Context, Entry, RunnableKind, Stage, check_pin};
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};

/// The `package` kind.
pub struct Package;

/// A `package` entry's settings (16 d.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The package's file, `.wasm` or `.pex`, on a disk the run attaches.
    pub path: String,
    /// The file's `sha256`, as `moruna check` prints it (`sha256:<hex>`).
    pub fingerprint: String,
    /// The arguments of a kernel built by them (16 b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// A directory the kernel reads, read only (16 b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assets: Option<String>,
    /// For a `.wasm`: the ports its `call` may name, and where each goes (MH 4.10). A name not
    /// listed here is refused when the stage calls it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<BTreeMap<String, PortDoc>>,
}

impl RunnableKind for Package {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "package".into(),
            role: Role::Kernel,
            summary: "A function package, one .wasm or .pex file pinned by its sha256".into(),
            settings: settings_schema::<Settings>(
                "package",
                vec![
                    json!({"path": "/disk/functions/shout.wasm",
                           "fingerprint": "sha256:9f2c0d6a58e1b7c3f4a5e6d7c8b9a0f1e2d3c4b5a6978899aabbccddeeff0011"}),
                    json!({"path": "/disk/functions/classify.pex",
                           "fingerprint": "sha256:0011ffeeddccbbaa99887766554433221100ffeeddccbbaa9988776655443322",
                           "args": {"threshold": 0.5}, "assets": "/disk/models/classifier"}),
                ],
            ),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs {
                model_port: Fact::BySettings,
                ..Needs::nothing()
            },
            deterministic: Fact::BySettings,
            preserves_columns: Fact::BySettings,
            isolation: Isolation::Confined,
        }
    }

    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage> {
        let settings: Settings = entry.settings()?;
        let routes = match &settings.ports {
            Some(listed) => ports::routes(entry, listed, cx)?,
            None => BTreeMap::new(),
        };
        let inputs = moruna_package::Inputs {
            args: settings.args,
            assets: settings.assets.as_deref().map(PathBuf::from),
        };
        // 16 PK-I2: opened, hashed and pinned before any stage is built; its runtime made (16
        // f.8).
        let packaged = crate::packaged::open(
            Path::new(&settings.path),
            Some(&settings.fingerprint),
            cx.package_host(),
            inputs,
        )
        .map_err(|e| match e {
            MorunaError::Plan(msg) => entry.refuse("fingerprint", msg),
            MorunaError::Config { msg, .. } if msg.starts_with("package: args:") => {
                entry.refuse("args", msg)
            }
            MorunaError::Config { msg, .. } if msg.starts_with("package: assets:") => {
                entry.refuse("assets", msg)
            }
            other => other,
        })?;
        cx.notes().extend(packaged.notes());
        check_pin(
            entry,
            Some(&settings.fingerprint),
            packaged.kernel().as_ref(),
        )?;
        if settings.ports.is_some() && packaged.kind() != "wasm" {
            return Err(entry.refuse(
                "ports",
                "only a .wasm package calls ports, through its `call` import",
            ));
        }
        let mut stage = Stage::of(packaged.kernel());
        // MH 4.10: every package stage knows its ports, none listed or some, so a call to a name
        // it does not list is refused by name.
        let bound = packaged.clone();
        stage.placed = Some(Box::new(move |id| {
            bound.bind_ports(moruna_kernel::StagePorts::new(id, routes));
        }));
        stage.packaged = Some(packaged);
        Ok(stage)
    }
}
