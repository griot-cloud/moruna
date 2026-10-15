//! `moruna check <package>` (16 f.9, PK-I9): the seven steps of MH 4.9 through the package's own
//! runtime, never in process, so the verdict is the one a hosted run would give.

use std::path::{Path, PathBuf};

use moruna_kernel::Result;

use super::{CheckOptions, CheckReport, check};
use crate::packaged::{self, PackageHost};

/// Check the package at `path`; the report's fingerprint is the file's `sha256`.
pub fn check_package(
    path: &Path,
    seed: u64,
    profiles_dir: Option<PathBuf>,
    host: &PackageHost,
) -> Result<CheckReport> {
    let packaged = packaged::open(path, None, host)?;
    packaged.bind_here()?;
    let package = packaged.package();
    let mut opts = CheckOptions::new(package.manifest.name.clone());
    opts.kind = packaged.kind();
    opts.fingerprint_scheme = "sha256";
    opts.seed = seed;
    opts.profiles_dir = profiles_dir;
    opts.memory_guard = Some(package.manifest.hints().memory_guard);
    let notes = packaged.notes();
    let mut report = check(packaged.kernel(), opts)?;
    report.notes.extend(notes);
    Ok(report)
}
