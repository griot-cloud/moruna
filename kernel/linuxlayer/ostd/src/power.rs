// SPDX-License-Identifier: MPL-2.0

//! Ending the personality's process, which is how a personality powers off.

use spin::Once;

/// How the system stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCode {
    /// Success.
    Success,
    /// Failure.
    Failure,
}

static RESTART_HANDLER: Once<fn(ExitCode)> = Once::new();
static POWEROFF_HANDLER: Once<fn(ExitCode)> = Once::new();

/// Injects what runs before a restart.
pub fn inject_restart_handler(handler: fn(ExitCode)) {
    RESTART_HANDLER.call_once(|| handler);
}

/// Injects what runs before a power-off.
pub fn inject_poweroff_handler(handler: fn(ExitCode)) {
    POWEROFF_HANDLER.call_once(|| handler);
}

/// Runs the power-off handler, then ends the process.
pub fn poweroff(code: ExitCode) -> ! {
    if let Some(handler) = POWEROFF_HANDLER.get() {
        handler(code);
    }
    exit(code)
}

/// Restarts: a personality cannot, so it runs the restart handler and ends with `code`.
pub fn restart(code: ExitCode) -> ! {
    if let Some(handler) = RESTART_HANDLER.get() {
        handler(code);
    }
    exit(code)
}

fn exit(code: ExitCode) -> ! {
    crate::backend::exit(match code {
        ExitCode::Success => 0,
        ExitCode::Failure => 1,
    })
}
