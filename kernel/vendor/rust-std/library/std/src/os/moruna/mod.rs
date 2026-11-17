//! moruna: Moruna-specific definitions (kernel/vendor/rust-std/README.md).
//!
//! `moruna_sys` is std's own copy of Moruna's system crate, the one std's platform layer runs
//! on, re-exported as Hermit's std re-exports `hermit_abi`. A program reaches the kernel's calls
//! through it, and so shares std's state: the start page, the start slots already taken (std
//! takes `log`), the heap and the clock page. A second copy of the crate in the program's own
//! graph would have none of that.

#![stable(feature = "rust1", since = "1.0.0")]
#![deny(unsafe_op_in_unsafe_fn)]

#[allow(unused_extern_crates)]
#[stable(feature = "rust1", since = "1.0.0")]
pub extern crate moruna_sys;
