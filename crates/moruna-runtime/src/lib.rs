//! Moruna facade, the Rust side of component 12: wires components 2 to 11 into
//! `Runtime::run`; no logic of its own.
//!
//! Design: `architecture/sdd/12-python.md` sections d.1, f.1, f.2, f.5, f.7 and l, and the
//! run lifecycle of the preamble's section 4.4, which is the order every step here follows.
//!
//! ```text
//! discover -> arena -> reactor -> trace -> sources and sinks -> kernels -> placement
//!   -> Scheduler::new -> init_instances -> Controller::new -> prepare -> probe -> start
//!   -> scheduler.run -> controller.stop -> trace.finish -> report
//! ```
//!
//! A resumed run differs in exactly the steps 12 f.7 names: the manifest header is read
//! first, `Placement::restore` runs before the scheduler is built, `apply_resume_point`
//! replaces `init_instances`, `probe_missing` replaces `probe_all` and `run_resumed`
//! replaces `run`.

#![deny(missing_docs)]
// `MorunaError` is the contracts crate's error and is 128 bytes wide; every crate in the
// workspace carries the same allow rather than boxing at every boundary.
#![allow(clippy::result_large_err)]

pub mod cancel;
pub mod checkpoint;
pub mod config;
pub mod error;
pub mod report;
pub mod run;
pub mod spec;

pub use checkpoint::CheckpointHandle;
pub use error::{Result, RunError};
pub use run::Runtime;
pub use spec::{BuildCtx, Components, RunSpec, SinkSpec, SourceSpec};

pub use moruna_discovery::{Discovered, DiscoveryInput};
pub use moruna_kernel::{CancelToken, ErrorPolicy, RunId, SizerKind};
pub use moruna_trace::{ExitReason, RunReport};

/// The process allocator for everything outside the arena (12 l). It is set here rather than
/// in `moruna-py` because the Python module links this crate, so one setting covers both.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
