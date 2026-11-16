// SPDX-License-Identifier: MPL-2.0

//! OSDK's heap allocator's per-CPU boxes. In user space the global allocator is
//! `moruna-sys`'s, so only the boxes are kept.

#![no_std]

pub use ostd::{CpuLocalBox, alloc_cpu_local};
