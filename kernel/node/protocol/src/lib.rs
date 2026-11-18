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

/// What a node keeps of its memory for its services (system design 3A).
pub const SERVICES_BYTES: u64 = 512 << 20;
/// A check's account: the engine's `check` on one document, which runs beside a job
/// (system design 3A).
pub const CHECK_BYTES: u64 = 256 << 20;
/// What no job may have of the node's memory: its services' and a check's, 768 MiB.
pub const RESERVED_BYTES: u64 = SERVICES_BYTES + CHECK_BYTES;
/// The slot of the engine's channel to `api` for a job (`moruna serve --listen channel:engine`).
pub const ENGINE_CHANNEL: &str = "engine";
/// The slot of the engine's channel to `api` for a check (`moruna check-job --listen
/// channel:check`).
pub const CHECK_CHANNEL: &str = "check";

/// What a job needs of the node beyond its budget, as the engine's check answers it (item 3.2):
/// the hosts it reaches; each volume by its serial, `true` when the job writes it; and the
/// functions it uses, by name. `node-admission` keeps them with the job, for items 3.3, 3.4
/// and 3.6 to grant from.
pub type Needs = (Vec<String>, Vec<(String, bool)>, Vec<String>);

/// The most of each kind of need a job may name.
pub const MAX_NEEDS: usize = 64;

/// Whether `needs` are a job's: at most [`MAX_NEEDS`] of each, each named once, a host 1 to
/// 253 bytes, a serial or a function 1 to 128, all printable ASCII without spaces, and no `/`
/// in a serial.
pub fn valid_needs(needs: &Needs) -> bool {
    let (hosts, volumes, functions) = needs;
    let named = |name: &str, most: usize| {
        (1..=most).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_graphic())
    };
    let once = |names: &[&str]| {
        names
            .iter()
            .enumerate()
            .all(|(i, n)| !names[..i].contains(n))
    };
    let serials: Vec<&str> = volumes.iter().map(|(serial, _)| serial.as_str()).collect();
    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let functions: Vec<&str> = functions.iter().map(String::as_str).collect();
    [hosts.len(), serials.len(), functions.len()]
        .iter()
        .all(|count| *count <= MAX_NEEDS)
        && hosts.iter().all(|h| named(h, 253))
        && serials.iter().all(|s| named(s, 128) && !s.contains('/'))
        && functions.iter().all(|f| named(f, 128))
        && once(&hosts)
        && once(&serials)
        && once(&functions)
}

/// The most a job's budget may be on a node of `node_memory` bytes (the machine account's
/// limit): its memory less [`RESERVED_BYTES`], in whole pages. A job whose document names no
/// budget has all of it: a node runs one job.
pub fn budget_max(node_memory: u64) -> u64 {
    node_memory.saturating_sub(RESERVED_BYTES) & !4095
}

/// Whether `launch` takes a job of `budget` bytes on a node of `node_memory` bytes: `Invalid`
/// for no budget or one that is not whole pages, `Quota` for more than [`budget_max`].
pub fn admits(budget: u64, node_memory: u64) -> Result<(), admission::Error> {
    if budget == 0 || !budget.is_multiple_of(4096) {
        return Err(admission::Error::Invalid);
    }
    if budget > budget_max(node_memory) {
        return Err(admission::Error::Quota);
    }
    Ok(())
}

/// The engine's arguments for a job on a node of `node_memory` bytes:
/// `serve --listen channel:engine --budget-max N`, node mode with the node's bound.
pub fn serve_args(node_memory: u64) -> Vec<String> {
    alloc::vec![
        "serve".into(),
        "--listen".into(),
        alloc::format!("channel:{ENGINE_CHANNEL}"),
        "--budget-max".into(),
        alloc::format!("{}", budget_max(node_memory)),
    ]
}

/// The engine's arguments for a check on a node of `node_memory` bytes:
/// `check-job --listen channel:check --node channel:engine --budget-max N`, which reads the
/// document and its secrets' names from the channel and answers the refusals and the budget.
pub fn check_args(node_memory: u64) -> Vec<String> {
    alloc::vec![
        "check-job".into(),
        "--listen".into(),
        alloc::format!("channel:{CHECK_CHANNEL}"),
        "--node".into(),
        alloc::format!("channel:{ENGINE_CHANNEL}"),
        "--budget-max".into(),
        alloc::format!("{}", budget_max(node_memory)),
    ]
}

moruna_svc::protocol! {
    /// Jobs execute the node's configured engine image. Every launch supplies a
    /// writable account capability and an explicit carved child budget. The server
    /// grants no filesystem, network, or process root implicitly to the engine.
    pub protocol admission {
        name = "moruna.node.admission";
        major = 1;
        minor = 1;
        requests {
            /// Launch one engine; names and grants correspond exactly. Config is
            /// the existing KD2 engine configuration; names cannot shadow reserved
            /// console/account/config slots. Linux/directory are explicit grants.
            /// `needs` are the job's as its check answered them (`Needs`), kept with
            /// the job for the grants of items 3.3, 3.4 and 3.6; `Invalid` when they
            /// are not `valid_needs`.
            1 => fn launch(account: Handle, budget: u64, config: Handle, names: Vec<String>, grants: Vec<Handle>, services: Vec<String>, needs: Needs) -> u64;
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
            /// Start the engine's check on one document (system design 3A), as `launch`
            /// starts a job, in a `CHECK_BYTES` account carved from the server's own check
            /// account, so that it runs beside a job. One check at a time: `Limit` until the
            /// last one is released. Status, cancel, wait and release take its id as a job's.
            6 => fn check(config: Handle, names: Vec<String>, grants: Vec<Handle>, services: Vec<String>) -> u64;
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
    fn a_jobs_budget_is_the_nodes_memory_less_768_mib() {
        assert_eq!(RESERVED_BYTES, 768 << 20);
        let node = 8u64 << 30;
        assert_eq!(budget_max(node), (8 << 30) - (768 << 20));
        // Whole pages, and nothing for a node smaller than what it keeps.
        assert_eq!(budget_max(RESERVED_BYTES + 4096 + 100), 4096);
        assert_eq!(budget_max(RESERVED_BYTES), 0);
        assert_eq!(budget_max(1 << 20), 0);
        assert_eq!(admits(budget_max(node), node), Ok(()));
        assert_eq!(admits(4096, node), Ok(()));
        assert_eq!(
            admits(budget_max(node) + 4096, node),
            Err(admission::Error::Quota)
        );
        assert_eq!(admits(0, node), Err(admission::Error::Invalid));
        assert_eq!(admits(4097, node), Err(admission::Error::Invalid));
        assert_eq!(admits(4096, RESERVED_BYTES), Err(admission::Error::Quota));
    }

    #[test]
    fn a_jobs_needs_are_bounded_and_each_named_once() {
        let s = |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).into()).collect() };
        let none: Needs = (Vec::new(), Vec::new(), Vec::new());
        assert!(valid_needs(&none));
        let every: Needs = (
            s(&["s3.example.com", "acct.blob.core.windows.net"]),
            alloc::vec![("vol-in".into(), false), ("vol-out".into(), true)],
            s(&["std", "taxi.clean"]),
        );
        assert!(valid_needs(&every));
        let with = |change: &dyn Fn(&mut Needs)| {
            let mut needs = every.clone();
            change(&mut needs);
            valid_needs(&needs)
        };
        assert!(!with(&|n| n.0.push("s3.example.com".into())));
        assert!(!with(&|n| n.1.push(("vol-in".into(), true))));
        assert!(!with(&|n| n.2.push("std".into())));
        assert!(!with(&|n| n.0.push(String::new())));
        assert!(!with(&|n| n.0.push("a b".into())));
        assert!(!with(&|n| n.0.push("h".repeat(254))));
        assert!(with(&|n| n.0.push("h".repeat(253))));
        assert!(!with(&|n| n.1.push(("a/b".into(), false))));
        assert!(!with(&|n| n.2.push("f".repeat(129))));
        let many = |count: usize| -> Needs {
            (
                (0..count).map(|i| alloc::format!("h{i}")).collect(),
                Vec::new(),
                Vec::new(),
            )
        };
        assert!(valid_needs(&many(MAX_NEEDS)));
        assert!(!valid_needs(&many(MAX_NEEDS + 1)));
    }

    #[test]
    fn the_engine_serves_and_checks_on_its_channels_with_the_nodes_bound() {
        let node = 4u64 << 30;
        let max = alloc::format!("{}", budget_max(node));
        assert_eq!(
            serve_args(node),
            [
                "serve",
                "--listen",
                "channel:engine",
                "--budget-max",
                max.as_str()
            ]
        );
        assert_eq!(
            check_args(node),
            [
                "check-job",
                "--listen",
                "channel:check",
                "--node",
                "channel:engine",
                "--budget-max",
                max.as_str()
            ]
        );
    }

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
