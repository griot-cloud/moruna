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
//!
//! The process allocates outside the arena through the platform's own allocator, told to give
//! freed memory back (`allocator`). mimalloc was set here until 2026-09-27 (12 l), and it held
//! freed memory the process no longer used: at a 256 MiB budget, a contract write peaked at 1.8
//! times the ceiling on Linux and 2 times on macOS under mimalloc 3 (1.5 to 4.7 times under
//! mimalloc 2), against 1.1 to 1.2 times under the platform's, the same runs measured by the
//! operating system (F8.9).

#![deny(missing_docs)]
// `MorunaError` is the contracts crate's error and is 128 bytes wide; every crate in the
// workspace carries the same allow rather than boxing at every boundary.
#![allow(clippy::result_large_err)]

mod allocator;
pub mod cancel;
pub mod check;
pub mod checkpoint;
pub mod config;
pub mod elastic;
pub mod error;
pub mod host;
pub mod job;
pub mod observe;
pub mod report;
pub mod run;
pub mod spec;

pub use checkpoint::CheckpointHandle;
pub use elastic::ElasticBudget;
pub use error::{Result, RunError};
pub use job::JobSpec;
pub use observe::{Progress, RunObserver};
pub use run::Runtime;
pub use spec::{BuildCtx, Components, EngineMemory, RunSpec, SinkSpec, SourceSpec};

pub use moruna_discovery::{
    Discovered, DiscoveryInput, LimitsReading, LimitsSource, LimitsSubscriber, ManualLimitsSource,
};
pub use moruna_kernel::{CancelToken, ErrorPolicy, RunId, SizerKind};
pub use moruna_trace::{ExitReason, RunReport};
