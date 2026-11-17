// SPDX-License-Identifier: MPL-2.0

//! Powering off: ending the personality's process.

use crate::power::ExitCode;

/// Ends the process with `code`.
pub fn try_poweroff(code: ExitCode) {
    crate::backend::exit(match code {
        ExitCode::Success => 0,
        ExitCode::Failure => 1,
    })
}
