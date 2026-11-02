//! A listener capability authorizes admission; no ambient credentials are accepted.
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{string::String, vec::Vec};
use moruna_svc::Handle;

pub const MAX_JOBS: usize = 64;
pub use moruna_node::MAX_JOB_GRANTS as MAX_GRANTS;
pub const RUNNING: u32 = 0;
pub const TERMINATED: u32 = 1;
pub const CANCELLED: u32 = 2;

moruna_svc::protocol! {
    /// Jobs execute the node's configured engine image. Every launch supplies a
    /// writable account capability and an explicit carved child budget. The server
    /// grants no filesystem, network, or process root implicitly to the engine.
    pub protocol admission {
        name = "moruna.node.admission";
        major = 1;
        minor = 0;
        requests {
            /// Launch one engine; names and grants correspond exactly. Config is
            /// the existing KD2 engine configuration; names cannot shadow reserved
            /// console/account/config slots. Linux/directory are explicit grants.
            1 => fn launch(account: Handle, budget: u64, config: Handle, names: Vec<String>, grants: Vec<Handle>, services: Vec<String>) -> u64;
            /// Actual process state; termination does not claim a successful exit.
            2 => fn status(job: u64) -> u32;
            /// Kill the caller's job. A returned success means kill was accepted;
            /// status/wait reports actual termination separately.
            3 => fn cancel(job: u64) -> ();
            /// Complete only when actual process termination is observed.
            4 => fn wait(job: u64) -> u32;
            /// Release a completed job's retained metadata/account handle. Running
            /// jobs must be cancelled and observed terminated before release.
            5 => fn release(job: u64) -> ();
        }
        events {}
        errors {
            1 => Invalid;
            2 => Quota;
            3 => Unavailable;
            4 => NotFound;
            5 => Limit;
        }
    }
}

pub fn valid_grants(names: &[String], count: usize) -> bool {
    names.len() == count
        && count <= MAX_GRANTS
        && names.iter().enumerate().all(|(i, n)| {
            !n.is_empty()
                && n.len() <= 32
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && !["console", "account", "config"].contains(&n.as_str())
                && !names[..i].contains(n)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_names_cannot_shadow_authority_or_alias() {
        assert!(!valid_grants(&["account".into()], 1));
        assert!(!valid_grants(&["linux".into(), "linux".into()], 2));
        assert!(!valid_grants(&["linux".into()], 0));
        assert!(valid_grants(&["linux".into(), "directory".into()], 2));
    }
}

moruna_svc::protocol! {
    /// Private boot-granted node owner control, never an admission client grant.
    pub protocol control {
        name = "moruna.node.control";
        major = 1;
        minor = 0;
        requests {
            /// Stop admissions, kill all owned engines and reply only after their
            /// actual process termination signals. The supervisor owns fallback
            /// escrow handles if this service itself has failed.
            1 => fn shutdown() -> ();
            /// Revoke one explicit trusted boot grant. Never installs new rights.
            2 => fn revoke(grant_id:u64) -> ();
        }
        events {}
        errors { 1 => Unavailable; }
    }
}
