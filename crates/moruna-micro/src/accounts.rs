//! The controller's `Sampler` on the kernel's accounts, exact figures in place of sampled
//! memory (`architecture/data-os.md` §6): stream C's module. Only stream C writes this file.
//!
//! The engine process is charged to the run's account (the supervisor's `run_granted` gives
//! it), so its own account's figures are the run's: the arena's memory objects, which are
//! charged whole when they are made, its heap and everything else it made. What a task holds
//! is charged to the task's own account, carved from the run's (`task.rs`), and is added to a
//! sample by the stage that holds it.

use moruna_sys::Usage;

/// An account's figures, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Charged to it now.
    pub used: u64,
    /// Set aside for the accounts carved from it: the tasks'.
    pub reserved: u64,
    /// Its limit: the run's budget.
    pub limit: u64,
    /// The most ever charged to it at once.
    pub peak: u64,
}

impl From<Usage> for Reading {
    fn from(u: Usage) -> Reading {
        Reading {
            used: u.used,
            reserved: u.reserved,
            limit: u.limit,
            peak: u.peak,
        }
    }
}

impl Reading {
    /// What can still be charged to it, or carved from it.
    pub fn room(&self) -> u64 {
        self.limit
            .saturating_sub(self.used)
            .saturating_sub(self.reserved)
    }
}

/// The run's account now: the engine's own (`account_info(0)`, answered by the kernel for the
/// calling process, never forwarded to the personality).
pub fn run() -> Result<Reading, String> {
    Usage::own()
        .map(Reading::from)
        .map_err(|e| format!("account_info: {}", e.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_is_the_limit_less_what_is_charged_and_set_aside() {
        let r = Reading {
            used: 30,
            reserved: 50,
            limit: 100,
            peak: 40,
        };
        assert_eq!(r.room(), 20);
        let over = Reading { used: 90, ..r };
        assert_eq!(over.room(), 0);
    }
}
