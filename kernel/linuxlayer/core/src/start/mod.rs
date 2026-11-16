// SPDX-License-Identifier: MPL-2.0

//! A task's start (Moruna-written): what a task's launcher grants the personality, made into
//! the ports its filesystems and descriptors are built on (`fs::rootfs::TaskFs`,
//! `fs::task_streams`).
//!
//! - The image: `moruna.imagefs` 3.0 on the task's `filesystem` channel (`image.rs`).
//! - The plane: the task's input and output slots (`plane.rs`), as `kernel/linux` serves
//!   them (SDD 29 f.7, f.8).
//! - The text: lines in the engine's log ring (`text.rs`).
//!
//! The core's calls are in `moruna.rs`, on the target only; the rest runs in host tests.

#![cfg_attr(
    all(not(target_os = "none"), not(all(test, ktest))),
    expect(
        dead_code,
        reason = "the target's start uses them, and the host's tests"
    )
)]

#[cfg(target_os = "none")]
mod image;
#[cfg(target_os = "none")]
mod moruna;
mod plane;
#[cfg(ktest)]
mod test;
mod text;

#[cfg(target_os = "none")]
pub use self::moruna::{StartError, TaskStart, start_task};
use crate::prelude::*;

/// A task's program: its path on the image, its arguments (`argv[0]` included) and its
/// environment, exactly as its launcher gave them (`linuxlayer_launch::command`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskCommand {
    pub path: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
}

static COMMAND: spin::Once<TaskCommand> = spin::Once::new();

/// The task's command, if its launcher gave one.
pub(crate) fn command() -> Option<&'static TaskCommand> {
    COMMAND.get()
}

/// Records the task's command, once.
pub(crate) fn set_command(command: TaskCommand) {
    COMMAND.call_once(|| command);
}
