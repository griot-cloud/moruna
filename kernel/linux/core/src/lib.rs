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
//! - [`refused`]: the calls refused, counted;
//! - [`events`]: `eventfd` and `epoll`, an event loop's own descriptors (SDD 30 f.4);
//! - [`machine`]: what a program granted a memory budget is told its machine has (SDD 30 f.3).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod elf;
pub mod events;
pub mod fd;
pub mod machine;
pub mod mmap;
pub mod path;
pub mod refused;
pub mod stack;
pub mod table;
pub mod types;
