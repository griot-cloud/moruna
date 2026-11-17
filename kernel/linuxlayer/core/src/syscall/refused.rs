// SPDX-License-Identifier: MPL-2.0

//! The calls the personality refuses, and their counts (`architecture/data-os.md` §3a: the
//! data OS takes Talos's posture, so the carried layer keeps what a job's programs need and
//! refuses administrative calls).
//!
//! - An administrative call answers `EPERM`, as Linux answers a process without the
//!   privilege, before its handler runs: mounts (the old call and the new mount API),
//!   `pivot_root` and `chroot`, namespaces (`unshare`, `setns`, and `clone` with a
//!   `CLONE_NEW*` flag), `ptrace` and the calls that need its permission, modules, `kexec`,
//!   `reboot`, swap, every change of time, and the host and domain names.
//! - A call the table does not have answers `ENOSYS`.
//! - The socket calls answer from `no_socket.rs` (data-os.md §4: no network).
//!
//! Each is counted by number for the process that made it, as `kernel/linux` counts its
//! refusals (SDD 28 f.6), and reported when the process ends; the task's totals are
//! reported when the personality ends.

use alloc::format;

use crate::{prelude::*, process::Pid};

/// The administrative calls, refused with `EPERM`: number and name, in number order.
pub(crate) const ADMINISTRATIVE: &[(u64, &str)] = &[
    (101, "ptrace"),
    (155, "pivot_root"),
    (159, "adjtimex"),
    (161, "chroot"),
    (164, "settimeofday"),
    (165, "mount"),
    (166, "umount2"),
    (167, "swapon"),
    (168, "swapoff"),
    (169, "reboot"),
    (170, "sethostname"),
    (171, "setdomainname"),
    (175, "init_module"),
    (176, "delete_module"),
    (227, "clock_settime"),
    (246, "kexec_load"),
    (272, "unshare"),
    (305, "clock_adjtime"),
    (308, "setns"),
    (310, "process_vm_readv"),
    (311, "process_vm_writev"),
    (313, "finit_module"),
    (320, "kexec_file_load"),
    (428, "open_tree"),
    (429, "move_mount"),
    (430, "fsopen"),
    (431, "fsconfig"),
    (432, "fsmount"),
    (433, "fspick"),
    (438, "pidfd_getfd"),
    (442, "mount_setattr"),
];

/// The socket calls: answered by `no_socket.rs`, and counted.
const SOCKETS: &[(u64, &str)] = &[
    (41, "socket"),
    (42, "connect"),
    (43, "accept"),
    (44, "sendto"),
    (45, "recvfrom"),
    (46, "sendmsg"),
    (47, "recvmsg"),
    (48, "shutdown"),
    (49, "bind"),
    (50, "listen"),
    (51, "getsockname"),
    (52, "getpeername"),
    (53, "socketpair"),
    (54, "setsockopt"),
    (55, "getsockopt"),
    (288, "accept4"),
    (307, "sendmmsg"),
];

/// `clone` and `clone3`, refused with `EPERM` when they ask for a new namespace.
pub(crate) const CLONE: u64 = 56;
pub(crate) const CLONE3: u64 = 435;
const CLONES: &[(u64, &str)] = &[(CLONE, "clone"), (CLONE3, "clone3")];

/// A call's name, for the report.
pub(crate) fn name(number: u64) -> Option<&'static str> {
    [ADMINISTRATIVE, SOCKETS, CLONES]
        .iter()
        .flat_map(|list| list.iter())
        .find(|(n, _)| *n == number)
        .map(|(_, name)| *name)
}

/// Counts by number: per process, and the task's whole.
struct Counts {
    processes: BTreeMap<Pid, BTreeMap<u64, u64>>,
    task: BTreeMap<u64, u64>,
}

static COUNTS: SpinLock<Counts> = SpinLock::new(Counts {
    processes: BTreeMap::new(),
    task: BTreeMap::new(),
});

/// Counts one refusal of call `number` for process `pid`.
pub(crate) fn count(pid: Pid, number: u64) {
    let mut counts = COUNTS.lock();
    *counts
        .processes
        .entry(pid)
        .or_default()
        .entry(number)
        .or_insert(0) += 1;
    *counts.task.entry(number).or_insert(0) += 1;
}

/// Every call the task's programs made.
static CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many calls the task's programs made (linux-protocol's `Ended::served`).
pub(crate) fn calls() -> u64 {
    CALLS.load(core::sync::atomic::Ordering::Relaxed)
}

/// What happens to call `number` before its handler runs: an administrative call is counted
/// and refused with `EPERM`; a socket call is counted and served by `no_socket.rs`.
pub(crate) fn screen(pid: Pid, number: u64) -> Result<()> {
    CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if ADMINISTRATIVE.iter().any(|(n, _)| *n == number) {
        count(pid, number);
        return_errno_with_message!(
            Errno::EPERM,
            "an administrative call, refused (data-os.md §3a)"
        );
    }
    if SOCKETS.iter().any(|(n, _)| *n == number) {
        count(pid, number);
    }
    Ok(())
}

/// Refuses call `number` (`clone` or `clone3`) for process `pid` when `new_namespace`.
pub(crate) fn screen_clone(pid: Pid, number: u64, new_namespace: bool) -> Result<()> {
    if new_namespace {
        count(pid, number);
        return_errno_with_message!(Errno::EPERM, "a new namespace, refused (data-os.md §3a)");
    }
    Ok(())
}

/// Counts call `number`, which the table does not have, for process `pid`.
pub(crate) fn unknown(pid: Pid, number: u64) {
    count(pid, number);
}

/// The first native number (`moruna_abi::nr::BASE`); every Linux number is below it.
const NATIVE_BASE: u64 = moruna_abi::nr::BASE;

/// The answer to native call `number`, if it is one: the core sends program mode's native
/// calls that are not on its allowlist here (`architecture/data-os.md` §4), where they are
/// counted and answered with the core's `BadSyscall`, as a native call returns an error.
pub(crate) fn native(pid: Pid, number: u64) -> Option<isize> {
    if number < NATIVE_BASE {
        return None;
    }
    count(pid, number);
    Some(-(moruna_abi::Error::BadSyscall as isize))
}

/// `name(number) xcount`, joined by commas, in number order; `none` for none.
pub(crate) fn render(counts: &[(u64, u64)]) -> String {
    if counts.is_empty() {
        return String::from("none");
    }
    counts
        .iter()
        .map(|(n, c)| match name(*n) {
            Some(name) => format!("{name}({n}) x{c}"),
            None if *n >= NATIVE_BASE => format!("native {}({n}) x{c}", n - NATIVE_BASE),
            None => format!("{n} x{c}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Takes process `pid`'s counts, in number order.
pub(crate) fn take_process(pid: Pid) -> Vec<(u64, u64)> {
    COUNTS
        .lock()
        .processes
        .remove(&pid)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// The task's counts so far, in number order.
pub(crate) fn task() -> Vec<(u64, u64)> {
    COUNTS.lock().task.iter().map(|(n, c)| (*n, *c)).collect()
}

/// Reports process `pid`'s refusals when it ends, if it had any.
pub(crate) fn report_process(pid: Pid) {
    let counts = take_process(pid);
    if !counts.is_empty() {
        ostd::early_println!("linux: process {pid} refused: {}", render(&counts));
    }
}

/// Reports the task's refusals when the personality ends.
pub(crate) fn report_task() {
    ostd::early_println!("linux: the task refused: {}", render(&task()));
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn administrative_calls_are_refused_and_counted() {
        let pid = 9_001;
        for (number, _) in ADMINISTRATIVE {
            let err = screen(pid, *number).unwrap_err();
            assert_eq!(err.error(), Errno::EPERM);
        }
        assert!(screen(pid, 0).is_ok()); // read
        assert!(screen(pid, 41).is_ok()); // socket: served, counted
        unknown(pid, 9_999);
        assert!(screen_clone(pid, CLONE, false).is_ok());
        assert_eq!(
            screen_clone(pid, CLONE3, true).unwrap_err().error(),
            Errno::EPERM
        );
        let counts = take_process(pid);
        assert_eq!(counts.len(), ADMINISTRATIVE.len() + 3);
        assert!(counts.contains(&(165, 1)));
        assert!(counts.contains(&(41, 1)));
        assert!(counts.contains(&(9_999, 1)));
        assert!(counts.contains(&(CLONE3, 1)));
        assert!(take_process(pid).is_empty());
        assert!(task().iter().any(|(n, c)| *n == 165 && *c >= 1));
        report_process(pid);
        report_task();
    }

    #[ktest]
    fn the_list_is_sorted_and_named() {
        assert!(ADMINISTRATIVE.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(SOCKETS.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(name(166), Some("umount2"));
        assert_eq!(name(41), Some("socket"));
        assert_eq!(name(CLONE), Some("clone"));
        assert_eq!(name(9_999), None);
    }

    #[ktest]
    fn native_calls_off_the_core_s_list_are_refused_and_counted() {
        let pid = 9_003;
        assert_eq!(native(pid, 0), None);
        assert_eq!(native(pid, NATIVE_BASE - 1), None);
        let process_map = NATIVE_BASE + 33;
        assert_eq!(
            native(pid, process_map),
            Some(-(moruna_abi::Error::BadSyscall as isize))
        );
        native(pid, process_map);
        let counts = take_process(pid);
        assert_eq!(counts, [(process_map, 2)]);
        assert_eq!(render(&counts), "native 33(65569) x2");
    }

    #[ktest]
    fn rendered_as_kernel_linux_renders() {
        assert_eq!(render(&[]), "none");
        let pid = 9_002;
        count(pid, 165);
        count(pid, 165);
        count(pid, 41);
        count(pid, 9_999);
        assert_eq!(
            render(&take_process(pid)),
            "socket(41) x1, mount(165) x2, 9999 x1"
        );
        report_process(pid);
    }
}
