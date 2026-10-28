//! Readers for what the kernel loads at boot and on `process_map_image` (SDD 20,
//! f.1 and f.2): the initramfs, an uncompressed cpio archive in the newc format, and
//! static ELF64 executables. Both are pure functions over bytes, so they are tested
//! on the host; the kernel does the mapping.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cpio;
pub mod elf;
