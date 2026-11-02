//! The engine on the Moruna microkernel (SDD 30): one engine, the same code on a Linux node and
//! on the kernel.
//!
//! On the kernel the engine is a Linux program: the root workspace's `moruna`, built static for
//! musl, which the Linux personality runs unmodified (SDD 28). Its starter (the engine's
//! supervisor, from the boot manifest) asks the personality to run it with `run_granted`
//! (`moruna.linux` 3.0), so the process holds the handles the manifest names, on a start page
//! whose address its auxiliary vector carries, and is told its machine has the memory budget the
//! manifest names. Everything a Linux program does (threads, files, clocks, its event loop) goes
//! through the personality as for any program; what only the kernel has, the engine reaches with
//! the kernel's own calls on those handles:
//!
//! - [`start`]: whether this process is on the Moruna kernel, and its granted handles;
//! - [`vsock`]: the host protocol's connection to the embedder, over the vsock driver
//!   (`moruna.driver.vsock`), as the `Read` and `Write` halves the host protocol already speaks;
//! - [`image`]: the image the kernel attached: its record, granted as `image`, and the seals
//!   of the layers the personality's programs start on, which an `image` entry's reference is
//!   checked against before its task starts;
//! - [`task`]: an `image` stage as a task of the data plane: a process the personality starts
//!   with `run_plane`, whose batches move through [`moruna_plane::KernelPlane`] as pages, whose
//!   port calls go to the stage's ports (the embedder's, over the host protocol), and whose end,
//!   refusals and cancellation are the stage's, as on Linux.
//!
//! The kernel's calls exist only there, for x86-64 programs: on every other target the crate
//! says so ([`on_kernel`] is `false`) and has nothing else.

#![deny(missing_docs)]
#![deny(unsafe_code)]
// `MorunaError` is the contracts' error, carried by value in every `Result` of the engine
// (contracts 01 d.13), as in the other crates of the workspace.
#![allow(clippy::result_large_err)]

pub mod layout;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod image;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod ring;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod start;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod task;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod vsock;

/// Whether this process runs on the Moruna kernel, started with a start page (SDD 30 d.2).
pub fn on_kernel() -> bool {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        start::kernel().is_some()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        false
    }
}

/// Native PEX own-account and actual record-plane transport.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod pex;

/// Actual source filesystem directory-capability delegation.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod filesystem;
