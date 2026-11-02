//! `moruna check <package>` (16 f.9, PK-I9): the seven steps of MH 4.9 through the package's own
//! runtime, never in process, so the verdict is the one a hosted run would give; and warm-up
//! (AR section 6) through that runtime too, each start-up a fresh opening of the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::{Kernel, MorunaError, Result};

use super::warmup::{Maker, WarmUp};
use super::{CheckOptions, CheckReport, check};
use crate::packaged::{self, PackageHost};

/// Opens a package again, as a run opens it, for warm-up's start-up (AR section 6.3).
struct Reopen {
    path: PathBuf,
    host: PackageHost,
    inputs: moruna_package::Inputs,
}

impl Maker for Reopen {
    fn make(&self) -> Result<Arc<dyn Kernel>> {
        let packaged = packaged::open(&self.path, None, &self.host, self.inputs.clone())?;
        packaged.bind_here()?;
        Ok(packaged.kernel())
    }
}

/// Check the package at `path`; the report's fingerprint is the file's `sha256`. Warm-up, when
/// given, makes the kernel again from the file for its start-up, and keys the cost by the
/// package's arguments, which are its settings (16 b).
pub fn check_package(
    path: &Path,
    seed: u64,
    warm_up: Option<WarmUp>,
    host: &PackageHost,
    inputs: moruna_package::Inputs,
) -> Result<CheckReport> {
    let packaged = packaged::open(path, None, host, inputs.clone())?;
    packaged.bind_here()?;
    let package = packaged.package().ok_or_else(|| {
        MorunaError::Plan("a check opens a package, and an image is not one".into())
    })?;
    let mut opts = CheckOptions::new(package.manifest.name.clone());
    opts.kind = packaged.kind();
    opts.fingerprint_scheme = "sha256";
    opts.seed = seed;
    opts.warm_up = warm_up.map(|mut warm| {
        warm.settings =
            crate::cost::registration_settings("package", &serde_json::json!({"args":inputs.args}));
        warm.outside = packaged.outside();
        warm.maker = Some(Arc::new(Reopen {
            path: path.to_path_buf(),
            host: host.clone(),
            inputs,
        }));
        warm
    });
    opts.memory_guard = Some(package.manifest.hints().memory_guard);
    let notes = packaged.notes();
    let mut report = check(packaged.kernel(), opts)?;
    report.notes.extend(notes);
    // 16 d.2: the declaration is in the report, so a host never parses the package.
    report.manifest = serde_json::from_str(&package.manifest.to_json()).ok();
    Ok(report)
}
