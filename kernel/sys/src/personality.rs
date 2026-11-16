//! The user-space personality's mechanisms (ABI 2.6; `kernel-is-the-machine.md` §4.1,
//! `architecture/data-os.md` §4): stream B1's module.
//!
//! The call numbers (`nr::PERSONALITY_SET` to `nr::THREAD_INTERRUPT`) and the entry frame's
//! layout (`moruna_abi::personality`) are frozen. Stream B1 writes this module: the
//! wrappers' arguments are its to fix, in its first commit, and stream B2 (the port of
//! Asterinas's Linux layer) builds against them. Until B1 lands, each answers
//! `Error::BadSyscall`. Only stream B1 writes this file.

use moruna_abi::personality;

use crate::{Result, Thread, check, raw};

/// The frame's length in bytes.
pub const FRAME_LEN: usize = personality::LEN;

/// Reads `thread`'s frame (its registers in `personality`'s layout) into `frame`.
pub fn thread_state_read(thread: &Thread, frame: &mut [u8; FRAME_LEN]) -> Result<()> {
    check(raw::thread_state_read(thread.0.raw(), frame)).map(|_| ())
}

/// Writes `thread`'s registers from `frame`.
pub fn thread_state_write(thread: &Thread, frame: &[u8; FRAME_LEN]) -> Result<()> {
    check(raw::thread_state_write(thread.0.raw(), frame)).map(|_| ())
}
