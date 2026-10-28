//! Building a run from a job document (MH 4.1).
//!
//! The document itself, [`moruna_job::JobSpec`], is `moruna-job`'s: reading it, writing it and its
//! canonical form run nothing. [`build::build`] turns it into the facade's [`crate::RunSpec`],
//! and it is the only place a `RunSpec` is built from a description, whether the description
//! came from a file (`moruna run`), from a socket (`moruna serve`) or from the arguments of
//! `moruna.run(...)` (12 f.3). One code path, so a run from a file and the same run from Python
//! are the same run (H1).

pub mod build;
pub mod canonical;
pub mod translate;

use moruna_job::SpecError;
use moruna_kernel::MorunaError;

pub use build::{BuildOptions, Built, build};
pub use canonical::digest;

/// A refusal of the document as the runtime's error, `Config { name: "spec" }`: the text is the
/// refusal's own, `spec refused: <field>: <reason>`, which is what `moruna run` prints and exits 2
/// with (MH 4.1).
pub fn refusal(refused: SpecError) -> MorunaError {
    MorunaError::Config {
        name: "spec",
        msg: refused.to_string(),
    }
}

#[cfg(test)]
mod tests;
