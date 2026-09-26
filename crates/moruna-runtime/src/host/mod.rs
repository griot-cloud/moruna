//! Moruna as a process a host starts and hears from (MH 4.2, 4.3; MH).
//!
//! `moruna run <spec.json>` executes one job document; `moruna serve --listen <address>` waits
//! for one on a socket. Either way the process speaks the protocol of MH 4.3 to at most one
//! peer, writes the report file of MH 4.1 whatever happens, and exits with a code of MH 4.2.
//! Everything here is the Rust half; the binary with the Python adapter compiled in is
//! `moruna-py`'s, and it calls [`cli::main`] with a loader that can import Python kernels.

pub mod cli;
pub mod protocol;
pub mod session;
pub mod transport;

use moruna_kernel::MorunaError;

/// The exit codes of MH 4.2 (MH 4.2).
pub mod exit {
    /// The run completed.
    pub const COMPLETED: i32 = 0;
    /// The run failed for a reason none of the codes below names: a source or sink that could
    /// not be read or written, an unsupported path.
    pub const FAILED: i32 = 1;
    /// The document was refused, with the field named; also a command line that is not one.
    pub const SPEC_REFUSED: i32 = 2;
    /// The budget was refused (the parent's P5 diagnostic).
    pub const BUDGET_REFUSED: i32 = 3;
    /// A kernel failed and the error policy ended the run.
    pub const KERNEL_ERROR: i32 = 4;
    /// Resume was refused.
    pub const RESUME_REFUSED: i32 = 5;
    /// The run was cancelled.
    pub const CANCELLED: i32 = 130;
}

/// The exit code for an error (MH 4.2).
pub fn exit_code(error: &MorunaError) -> i32 {
    match error {
        MorunaError::Config { name, .. } if name.starts_with("budget") => exit::BUDGET_REFUSED,
        MorunaError::Config { .. } | MorunaError::Plan(_) => exit::SPEC_REFUSED,
        MorunaError::Budget { .. } | MorunaError::Alloc { .. } => exit::BUDGET_REFUSED,
        MorunaError::Kernel { .. } => exit::KERNEL_ERROR,
        MorunaError::Resume(_) => exit::RESUME_REFUSED,
        MorunaError::Cancelled => exit::CANCELLED,
        MorunaError::Source { .. }
        | MorunaError::Sink(_)
        | MorunaError::Io { .. }
        | MorunaError::Staging(_)
        | MorunaError::Convert(_)
        | MorunaError::Unsupported(_) => exit::FAILED,
    }
}

/// The text a host reads for an error: a refused document's own sentence
/// (`spec refused: <field>: <reason>`, MH 4.1), anything else as the error displays itself.
pub fn diagnostic(error: &MorunaError) -> String {
    match error {
        MorunaError::Config { name: "spec", msg } => msg.clone(),
        other => other.to_string(),
    }
}

/// The version `hello` and `moruna --version` report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_its_code() {
        let config = |name| MorunaError::Config {
            name,
            msg: String::new(),
        };
        assert_eq!(exit_code(&config("spec")), exit::SPEC_REFUSED);
        assert_eq!(exit_code(&config("budget.host")), exit::BUDGET_REFUSED);
        assert_eq!(
            exit_code(&MorunaError::Plan(String::new())),
            exit::SPEC_REFUSED
        );
        assert_eq!(
            exit_code(&MorunaError::Resume(String::new())),
            exit::RESUME_REFUSED
        );
        assert_eq!(exit_code(&MorunaError::Cancelled), exit::CANCELLED);
        assert_eq!(exit_code(&MorunaError::Sink(String::new())), exit::FAILED);
        assert_eq!(
            exit_code(&MorunaError::Staging(String::new())),
            exit::FAILED
        );
        assert_eq!(exit_code(&MorunaError::Unsupported("rdma")), exit::FAILED);
        assert_eq!(diagnostic(&config("spec")), "");
        assert_eq!(diagnostic(&MorunaError::Cancelled), "cancelled");
    }
}
