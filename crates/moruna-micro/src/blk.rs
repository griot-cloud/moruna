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

use std::sync::{Arc, OnceLock};

use moruna_arena::HostBacking;
use moruna_kernel::{ObjectMetadata, Reactor};

use crate::arena::Objects;
use crate::devices;

/// What the engine was granted on the Moruna kernel: the arena's memory objects and every data
/// disk, brought up.
#[derive(Clone)]
pub struct Granted {
    objects: Arc<Objects>,
    inputs: Vec<disk::Disk>,
    output: Option<disk::Disk>,
}

impl Granted {
    /// The arena's memory, and each `disk.in.*` and `disk.out` slot this process was granted
    /// brought up; `None` off the Moruna kernel. The disks are brought up by the first run and
    /// are the process's: a later run of the same engine (`moruna serve`) is given them again.
    pub fn take() -> Result<Option<Granted>, String> {
        static GRANTED: OnceLock<Result<Option<Granted>, String>> = OnceLock::new();
        GRANTED
            .get_or_init(|| {
                if !crate::on_kernel() {
                    return Ok(None);
                }
                let objects = Objects::new();
                let mut inputs = Vec::new();
                let mut output = None;
                for slot in crate::start::slots() {
                    if slot.starts_with(devices::INPUT_PREFIX) {
                        inputs.push(device::open(&slot, &objects, false)?);
                    } else if slot == devices::OUTPUT {
                        output = Some(device::open(&slot, &objects, true)?);
                    }
                }
                Ok(Some(Granted::new(objects, inputs, output)))
            })
            .clone()
    }

    /// What was granted, brought up already.
    pub fn new(
        objects: Arc<Objects>,
        inputs: Vec<disk::Disk>,
        output: Option<disk::Disk>,
    ) -> Granted {
        Granted {
            objects,
            inputs,
            output,
        }
    }

    /// The arena's memory.
    pub fn backing(&self) -> Arc<dyn HostBacking> {
        Arc::clone(&self.objects) as Arc<dyn HostBacking>
    }

    /// The input disks.
    pub fn inputs(&self) -> &[disk::Disk] {
        &self.inputs
    }

    /// The output disk, when the run has one.
    pub fn output(&self) -> Option<&disk::Disk> {
        self.output.as_ref()
    }

    /// `reactor` and its metadata with the input disks in front of them; unchanged with none.
    #[allow(clippy::type_complexity)]
    pub fn reactor(
        &self,
        reactor: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
    ) -> std::io::Result<(Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>)> {
        if self.inputs.is_empty() {
            return Ok((reactor, metadata));
        }
        let disks = Arc::new(reactor::DiskReactor::new(
            reactor,
            metadata,
            self.inputs.iter().cloned(),
        )?);
        Ok((disks.clone(), Some(disks)))
    }

    /// The report's lines: how the arena's memory was mapped and what each disk moved.
    pub fn notes(&self) -> Vec<String> {
        let m = self.objects.mapped();
        let mut notes = vec![format!(
            "data OS: the arena made {} granted memory objects of {} bytes as the run needed \
             them, {} mapped with large pages and every page present, {} mapped plainly",
            m.regions, m.bytes, m.huge, m.plain
        )];
        for disk in self.inputs.iter().chain(self.output.iter()) {
            let s = disk.stats();
            notes.push(format!(
                "data OS: {} ({} of {} bytes): {} operations, {} requests, {} doorbells, {} \
                 bytes by DMA, {} bytes through bounce sectors, {} failed",
                disk.name(),
                disk.length(),
                disk.capacity(),
                s.ops,
                s.requests,
                s.notifies,
                s.dma_bytes,
                s.bounced_bytes,
                s.failed
            ));
        }
        notes
    }
}
