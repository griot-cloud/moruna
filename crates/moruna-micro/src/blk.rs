//! The data disk's virtio-blk queues, driven by the engine itself with DMA into arena pages
//! (`architecture/data-os.md` §3, decision 2): stream C's module. Only stream C writes this
//! file and the files under `blk/`.
//!
//! The engine's supervisor grants each data device under its start-page name
//! ([`crate::devices`]): every `disk://NAME` input as `disk.in.NAME`, read only, and the run's
//! output disk as `disk.out`. The engine brings each device up as Moruna's own virtio-blk
//! driver process does, with `moruna-virtio`'s transport and split queues, and then moves
//! bytes with no message and no call per request (contract C7):
//!
//! - [`plan`]: a byte range as runs of whole sectors, its edges through a bounce sector;
//! - [`engine`]: one queue's requests, their bodies by DMA straight in and out of the
//!   operations' own memory, which on the kernel is the arena's pinned pages;
//! - [`disk`]: the I/O thread, polling while busy and asleep on a futex when idle;
//! - [`device`]: a granted device brought up, and its I/O thread started (kernel only);
//! - [`reactor`]: the reactor's `disk://` reads on those disks, planned splits read ahead
//!   by the scheduler as today (contract C2);
//! - [`layer`]: the output layer written to `disk.out` at the run's end (contract C3).

pub mod device;
pub mod disk;
pub mod engine;
pub mod layer;
pub mod plan;
pub mod reactor;

#[cfg(test)]
mod tests;
