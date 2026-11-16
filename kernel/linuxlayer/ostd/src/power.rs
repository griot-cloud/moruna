// SPDX-License-Identifier: MPL-2.0

//! Ending the personality's process, which is how a personality powers off.

/// How the system stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCode {
    /// Success.
    Success,
    /// Failure.
    Failure,
}

/// Ends the process.
pub fn poweroff(code: ExitCode) -> ! {
    crate::backend::exit(match code {
        ExitCode::Success => 0,
        ExitCode::Failure => 1,
    })
}

/// Restarts: a personality cannot, so it ends with failure.
pub fn restart() -> ! {
    poweroff(ExitCode::Failure)
}
