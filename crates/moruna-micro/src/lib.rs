//! The engine on the Moruna microkernel (SDD 30): one engine, the same code on a Linux node and
//! on the kernel.
//!
//! On the kernel the engine is a Linux program: the root workspace's `moruna`, built static for
//! musl, which the Linux personality runs unmodified (SDD 28). Its starter (the engine's
//! supervisor, from the boot manifest) asks the personality to run it with `run_granted`
//! (`moruna.linux` 4.0), so the process holds the handles the manifest names, on a start page
//! whose address its auxiliary vector carries, and is told its machine has the memory budget the
//! manifest names. Everything a Linux program does (threads, files, clocks, its event loop) goes
//! through the personality as for any program; what only the kernel has, the engine reaches with
//! the kernel's own calls on those handles:
//!
//! - [`start`]: whether this process is on the Moruna kernel, and its granted handles;
//! - [`vsock`]: the host protocol's connection to the embedder, over the vsock driver
//!   (`moruna.driver.vsock`), as the `Read` and `Write` halves the host protocol already speaks;
//! - [`channel`]: the host protocol on a node, over the channel `node-admission` starts the
//!   engine with, whose other end is the node's `api` (system design 3A);
//! - [`image`]: the image the kernel attached: its record, granted as `image`, and the seals
//!   of the layers the personality's programs start on, which an `image` entry's reference is
//!   checked against before its task starts;
//! - [`task`]: an `image` stage as a task of the data plane: a process the personality starts
//!   with `run_plane`, whose batches move through [`moruna_plane::KernelPlane`] as pages, whose
//!   port calls go to the stage's ports (the embedder's, over the host protocol), and whose end,
//!   refusals and cancellation are the stage's, as on Linux;
//! - [`functions`]: the registered functions a job uses (item 3.6), each a read-only memory
//!   object `node-admission` gave the engine, read in place: a native program the stage maps,
//!   or a Moruna image archive the image filesystem attaches as its task's root.
//!
//! The kernel's calls exist only there, for x86-64 programs: on every other target the crate
//! says so ([`on_kernel`] is `false`) and has nothing else.

#![deny(missing_docs)]
#![deny(unsafe_code)]
// `MorunaError` is the contracts' error, carried by value in every `Result` of the engine
// (contracts 01 d.13), as in the other crates of the workspace.
#![allow(clippy::result_large_err)]

pub mod devices;
pub mod functions;
pub mod layout;

// The data OS's engine-side modules (`architecture/data-os-streams.md`): each is written by
// one stream only. Stream C: `arena`, `blk`, `accounts`. Stream P: `rings`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod accounts;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod arena;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod blk;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod rings;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod channel;
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

// Item 3.3: volumes by serial, on every host; their driver's client on the kernel only.
pub mod volume;

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

/// The most bytes [`granted_data`] reads.
pub const GRANTED_DATA_MAX: u64 = 1 << 20;

/// The text of the data handle granted under `slot` (a file of the boot's, or one a service
/// wrote, padded with zeros to whole pages): its bytes before the first zero, at most
/// [`GRANTED_DATA_MAX`]. Off the Moruna kernel no handle is granted.
pub fn granted_data(slot: &str) -> Result<Vec<u8>, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use moruna_sys::{Memory, map};
        const PAGE: u64 = 4096;
        if !on_kernel() {
            return Err(format!(
                "no `{slot}` was granted: this process is not on the Moruna kernel"
            ));
        }
        let memory =
            Memory(start::take(slot).ok_or_else(|| {
                format!("no `{slot}` was granted (it holds {:?})", start::slots())
            })?);
        let mut out = Vec::new();
        let mut page = vec![0u8; PAGE as usize];
        let mut at = 0;
        while at < GRANTED_DATA_MAX {
            let mapping = memory.map(at, PAGE, map::READ).map_err(|e| {
                format!("`{slot}` has no zero byte in its {at} bytes: {}", e.name())
            })?;
            mapping
                .read(0, &mut page)
                .map_err(|e| format!("reading `{slot}`: {}", e.name()))?;
            if let Some(end) = page.iter().position(|b| *b == 0) {
                out.extend_from_slice(&page[..end]);
                return Ok(out);
            }
            out.extend_from_slice(&page);
            at += PAGE;
        }
        Err(format!("`{slot}` is larger than {GRANTED_DATA_MAX} bytes"))
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        Err(format!(
            "no `{slot}` was granted: data slots exist only on the Moruna kernel"
        ))
    }
}

/// Native PEX own-account and actual record-plane transport.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod pex;

/// Actual source filesystem directory-capability delegation.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod filesystem;
