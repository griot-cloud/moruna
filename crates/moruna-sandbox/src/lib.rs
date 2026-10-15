//! Moruna component 17, the `.pex` half: a function package in a separate process that has none
//! of Moruna's access to the machine (FP 4).
//!
//! Design: `architecture/sdd/17-sandbox.md`. The child of a stage is a fork of a warm template
//! interpreter (f.9, SB-I10); after the fork and before any kernel code it confines itself: a
//! mount namespace holding only its tree, `/tmp`, the interpreter's own files and three devices
//! (SB-I4), no capabilities, Landlock, seccomp (SB-I5), and a cgroup Moruna made (SB-I6).
//! Batches never cross the control socket: Moruna places each in a shared mapping and the child's
//! Arrow reads it in place (SB-I7). The allocator guard runs inside the child against the child's
//! own memory and the stage ceiling Moruna writes to the counters page (SB-I8), and Moruna's
//! sampler sees the child between batches (SB-I9).
//!
//! The sandbox is Linux: namespaces, Landlock, seccomp, cgroups and memfd have no other home.
//! Elsewhere the crate builds, its protocol is the same, and a `.pex` is refused by name.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// Every fallible call returns the contract's `MorunaError`, whose size is the contract's.
#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::time::Duration;

pub mod protocol;

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod confine;
#[cfg(target_os = "linux")]
pub mod fdpass;
#[cfg(target_os = "linux")]
mod kernel;
#[cfg(target_os = "linux")]
pub mod mapping;
#[cfg(all(target_os = "linux", feature = "python"))]
pub mod python;
#[cfg(target_os = "linux")]
pub mod seccomp;
#[cfg(target_os = "linux")]
pub mod template;

#[cfg(target_os = "linux")]
pub use kernel::{SandboxKernel, SandboxState};

/// How long a child asked to stop has before `SIGKILL` (17 i).
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// What a host gives a `.pex` (17 e.6): the standard-build interpreter with `moruna` and
/// `pyarrow`, and a cgroup v2 directory with `memory` and `cpu` enabled for its children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxHost {
    /// The interpreter the template runs.
    pub python: PathBuf,
    /// The cgroup a stage's cgroup is made in.
    pub cgroup: PathBuf,
}

/// The refusal on a system without the sandbox.
pub const NOT_LINUX: &str = "a .pex runs in the sandbox, which needs Linux";

/// On a system that is not Linux there is no sandbox: building a `.pex` stage is refused.
#[cfg(not(target_os = "linux"))]
pub fn unavailable() -> moruna_kernel::MorunaError {
    moruna_kernel::MorunaError::Config {
        name: "sandbox",
        msg: NOT_LINUX.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_pex_is_refused_off_linux() {
        assert!(super::unavailable().to_string().contains("needs Linux"));
    }
}
