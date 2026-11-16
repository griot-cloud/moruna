// SPDX-License-Identifier: MPL-2.0

//! A forked Linux process as a Moruna process of its own (`architecture/data-os.md` §4,
//! Moruna-written): a `process_clone` of its parent's process, sharing the task's handle
//! table, which shows the child's address space; the child's main thread starts in it, and
//! reaping the child gives it back.

use ostd::mm::vm_space::ProgramProcess;

use crate::{
    prelude::*,
    process::{Pid, Process},
    vm::vmar::VmarHandle,
};

/// A clone not yet a Linux process's: given back if the fork fails before then.
pub(super) struct CoreChild(u32);

impl Drop for CoreChild {
    fn drop(&mut self) {
        ostd::backend::task_release(self.0);
    }
}

/// The Moruna process of a fork's child: a clone of the parent's, showing `child`, the
/// space just forked from `parent`.
pub(super) fn clone_for(parent: &VmarHandle, child: &VmarHandle) -> Result<CoreChild> {
    let process = child.vm_space().show_cloned(parent.vm_space(), |source| {
        ostd::backend::fork_process(source.raw()).map(ProgramProcess::from_raw)
    })?;
    Ok(CoreChild(process.raw()))
}

/// The Moruna processes of the forked children not yet reaped.
static CHILDREN: SpinLock<BTreeMap<Pid, u32>> = SpinLock::new(BTreeMap::new());

/// Makes `core` the Moruna process of `child`: its main thread starts there.
pub(super) fn adopt(child: &Arc<Process>, core: CoreChild) {
    let raw = core.0;
    core::mem::forget(core);
    child.tasks().lock().main().set_home(raw);
    CHILDREN.lock().insert(child.pid(), raw);
}

/// Gives back the Moruna process of `pid`, reaped.
pub(super) fn reaped(pid: Pid) {
    let raw = CHILDREN.lock().remove(&pid);
    if let Some(raw) = raw {
        ostd::backend::task_release(raw);
    }
}
