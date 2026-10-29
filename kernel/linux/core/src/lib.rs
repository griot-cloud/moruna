//! The Linux personality's logic (SDD 28): `no_std`, safe, and host tested. The server
//! (`kernel/linux/server`) is this over the kernel's forward and the image filesystem.
//!
//! - [`table`]: every call, by number, and who answers it;
//! - [`types`]: Linux's structures and constants, as bytes;
//! - [`elf`]: what a program's headers say to map, interpreter included;
//! - [`stack`]: the first stack, as `execve` lays it out;
//! - [`mmap`]: how a mapping of a file is made;
//! - [`fd`]: file descriptors;
//! - [`path`]: paths from the working directory or an open directory;
//! - [`refused`]: the calls refused, counted.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod elf;
pub mod fd;
pub mod mmap;
pub mod path;
pub mod refused;
pub mod stack;
pub mod table;
pub mod types;
