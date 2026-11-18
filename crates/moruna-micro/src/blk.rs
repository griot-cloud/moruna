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
//!   by the scheduler as today (contract C2), and the sinks' writes to the output layer;
//! - [`out`]: the output layer, written onto `disk.out` as the sinks write and finished in
//!   place at the run's end (contract C3).

pub mod device;
pub mod disk;
pub mod engine;
pub mod out;
pub mod plan;
pub mod reactor;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use moruna_arena::HostBacking;
use moruna_kernel::{Allocator, ObjectMetadata, Reactor};

use crate::arena::Objects;
use crate::devices;
use crate::volume::{io::Volume, layer::Files};

/// What the engine was granted on the Moruna kernel: the arena's memory objects and every data
/// disk, brought up.
#[derive(Clone)]
pub struct Granted {
    objects: Arc<Objects>,
    inputs: Vec<disk::Disk>,
    output: Option<disk::Disk>,
    /// The current run's output layer, made with its reactor.
    layer: Arc<Mutex<Option<Arc<out::Output>>>>,
    /// The volumes the node granted (item 3.3): each source's layer, read only.
    sources: Vec<Arc<Files>>,
    /// Each sink's volume, which the run writes a layer onto.
    sink_volumes: Vec<Volume>,
    /// The layers the current run writes onto its sinks' volumes, made with its reactor.
    sinks: Arc<Mutex<Vec<Arc<out::Output>>>>,
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
                let mut volumes = false;
                for slot in crate::start::slots() {
                    if slot.starts_with(devices::INPUT_PREFIX) {
                        inputs.push(device::open(&slot, &objects, false)?);
                    } else if slot == devices::OUTPUT {
                        output = Some(device::open(&slot, &objects, true)?);
                    } else if slot == crate::volume::GRANTS_SLOT {
                        volumes = true;
                    }
                }
                let (sources, sinks) = if volumes {
                    granted_volumes(&objects)?
                } else {
                    (Vec::new(), Vec::new())
                };
                Ok(Some(
                    Granted::new(objects, inputs, output).with_volumes(sources, sinks),
                ))
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
            layer: Arc::default(),
            sources: Vec::new(),
            sink_volumes: Vec::new(),
            sinks: Arc::default(),
        }
    }

    /// The same, with the node's volumes: each source's layer, and each sink's volume.
    pub fn with_volumes(mut self, sources: Vec<Arc<Files>>, sinks: Vec<Volume>) -> Granted {
        self.sources = sources;
        self.sink_volumes = sinks;
        self
    }

    /// The sources' volumes' layers.
    pub fn sources(&self) -> &[Arc<Files>] {
        &self.sources
    }

    /// The sinks' volumes.
    pub fn sink_volumes(&self) -> &[Volume] {
        &self.sink_volumes
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

    /// `reactor` and its metadata with the disks in front of them: the input disks' reads, and
    /// this run's output layer on the output disk, its bounce buffers from `alloc`. Unchanged
    /// with no disk.
    #[allow(clippy::type_complexity)]
    pub fn reactor(
        &self,
        reactor: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
        alloc: Arc<dyn Allocator>,
    ) -> std::io::Result<(Arc<dyn Reactor>, Option<Arc<dyn ObjectMetadata>>)> {
        if self.inputs.is_empty()
            && self.output.is_none()
            && self.sources.is_empty()
            && self.sink_volumes.is_empty()
        {
            return Ok((reactor, metadata));
        }
        let disks = Arc::new(
            reactor::DiskReactor::new(
                reactor,
                metadata,
                self.inputs.iter().cloned(),
                self.output.clone().map(|d| (d, alloc.clone())),
            )?
            .with_volumes(
                self.sources.iter().cloned(),
                self.sink_volumes.iter().cloned(),
                alloc,
            ),
        );
        *self.layer.lock().unwrap_or_else(|e| e.into_inner()) = disks.output();
        *self.sinks.lock().unwrap_or_else(|e| e.into_inner()) = disks.sinks();
        Ok((disks.clone(), Some(disks)))
    }

    /// Ends the run's output layer: finished in place when the run `completed`, with what is
    /// left under `root` in the scratch; left with no header otherwise. `None` when the run
    /// has no output disk.
    pub fn finish(
        &self,
        completed: bool,
        root: &Path,
        alloc: Arc<dyn Allocator>,
    ) -> Option<Result<out::Written, String>> {
        let disk = self.output.as_ref()?;
        let layer = self.layer.lock().unwrap_or_else(|e| e.into_inner()).take();
        if !completed {
            return None;
        }
        // A run whose reactor was given to it made no layer: its outputs are only what is
        // under the root.
        let layer = layer.unwrap_or_else(|| {
            let tier = if alloc.is_pinned() {
                moruna_kernel::Tier::PinnedHost
            } else {
                moruna_kernel::Tier::Host
            };
            out::Output::new(disk.clone(), alloc, tier, Arc::new(|f| f()))
        });
        Some(layer.finish(root))
    }

    /// Ends the layers the run wrote onto its sinks' volumes (item 3.3): each finished in place
    /// when the run `completed`, its tables, its tree and its header after them; left with no
    /// header otherwise, which a source then refuses. By serial.
    pub fn finish_volumes(&self, completed: bool) -> Vec<(String, Result<out::Written, String>)> {
        let layers = std::mem::take(&mut *self.sinks.lock().unwrap_or_else(|e| e.into_inner()));
        if !completed {
            return Vec::new();
        }
        layers
            .into_iter()
            .map(|layer| {
                let serial = layer.target().name().to_string();
                // A volume's files are the sinks' alone: nothing is picked up from the scratch.
                (serial, layer.finish(Path::new(NO_ROOT)))
            })
            .collect()
    }

    /// The report's lines: how the arena's memory was mapped and what each disk moved.
    pub fn notes(&self) -> Vec<String> {
        let m = self.objects.mapped();
        let mut notes = vec![format!(
            "data OS: the arena made {} granted memory objects of {} bytes as the run needed \
             them, {} mapped with large pages and every page present, {} mapped plainly",
            m.regions, m.bytes, m.huge, m.plain
        )];
        for volume in self
            .sources
            .iter()
            .map(|f| f.volume())
            .chain(self.sink_volumes.iter())
        {
            let s = volume.stats();
            notes.push(format!(
                "data OS: volume {} ({} bytes, {}): {} requests, {} bytes placed by its driver, \
                 {} bytes through bounces, {} requests moved through a bounce, at most {} on their \
                 way at once",
                volume.serial(),
                volume.capacity(),
                if volume.writable() {
                    "a sink's"
                } else {
                    "a source's"
                },
                s.requests,
                s.placed_bytes,
                s.bounced_bytes,
                s.fallbacks,
                s.in_flight_max
            ));
        }
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

/// A root no sink writes under: a volume's layer takes nothing from the scratch.
const NO_ROOT: &str = "/moruna/no-root-for-a-volume";

/// The volumes the node granted the engine (`node-admission`, item 3.3), from their list
/// ([`crate::volume::GRANTS_SLOT`]): each its driver's client on its connection, a source's
/// layer opened and its index checked, a sink's volume to write.
fn granted_volumes(objects: &Arc<Objects>) -> Result<(Vec<Arc<Files>>, Vec<Volume>), String> {
    let text = crate::granted_data(crate::volume::GRANTS_SLOT)?;
    let text = String::from_utf8(text).map_err(|_| "the node's volume grants are not UTF-8")?;
    let mut sources = Vec::new();
    let mut sinks = Vec::new();
    for grant in crate::volume::parse_grants(&text)? {
        let handle = crate::start::take(&grant.slot).ok_or_else(|| {
            format!(
                "volume {}: no `{}` was granted, its driver's connection",
                grant.serial, grant.slot
            )
        })?;
        let driver = crate::volume::block::BlockDriver::open(
            moruna_svc::Channel(handle),
            Arc::clone(objects),
            grant.sink,
        )
        .map_err(|why| format!("volume {}: {why}", grant.serial))?;
        let volume = Volume::new(&grant.serial, Arc::new(driver), grant.sink)?;
        if grant.sink {
            sinks.push(volume);
        } else {
            sources.push(Arc::new(Files::open(volume)?));
        }
    }
    Ok((sources, sinks))
}
