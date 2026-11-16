// SPDX-License-Identifier: MPL-2.0

//! Linux's user segment selectors, which `ptrace` reports.

/// The user code segment selector Linux programs see.
pub const USER_CS_VALUE: usize = 0x33;
/// The user stack segment selector Linux programs see.
pub const USER_SS_VALUE: usize = 0x2b;
