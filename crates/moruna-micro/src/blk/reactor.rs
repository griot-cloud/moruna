//! The reactor's `disk://` reads on the engine's own disks (`architecture/data-os.md` §3, §6:
//! "the reactor drives the data disk's ring").
//!
//! A source treats `disk://NAME` as an object URL, so it reads through `read_object` and learns
//! the file's size through `head_object`, as it does for any store. [`DiskReactor`] answers
//! those for the disks the engine was granted and hands every other operation to the reactor
//! it wraps, unchanged. A read is one operation on the disk's queue, its whole sectors landing
//! by DMA in the arena buffer the source was given, and its completion resolved on the disk's
//! I/O thread: no copy of the body, no call and no message per read. Readahead is the
//! scheduler's, as on every host: each source drive keeps its `read_ahead` planned splits in
//! flight (contract C2), and here each of them is requests on the device's ring.
//!
//! A sink's `write_object` under [`out::URL`] is the output layer's: [`out::Output`] puts the
//! file onto `disk.out` as it is written (contract C3).
//!
//! Volumes (item 3.3) answer `disk://<serial>/<path>`: a source's volume the files of the layer
//! it holds ([`Files`]), read straight into the source's buffer by its driver; a sink's volume
//! the layer the run writes onto it, an [`out::Output`] of its own.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use moruna_kernel::{
    Allocator, Buffer, BufferView, Completion, CopyDst, CopySrc, Dispatch, IoPaths, MorunaError,
    ObjectMeta, ObjectMetadata, Reactor, Tier,
};

use super::disk::Disk;
use super::engine::{Kind, Op};
use super::out::{self, Output};
use crate::devices;
use crate::volume::{self, io::Volume, layer::Files};

/// A volume the reactor answers for.
enum Mounted {
    /// A source's: the layer it holds, read only.
    Source(Arc<Files>),
    /// A sink's: the layer the run writes onto it.
    Sink(Arc<Output>),
}

/// The reactor with the engine's disks in front of it.
pub struct DiskReactor {
    inner: Arc<dyn Reactor>,
    metadata: Option<Arc<dyn ObjectMetadata>>,
    disks: BTreeMap<String, Disk>,
    output: Option<Arc<Output>>,
    volumes: BTreeMap<String, Mounted>,
    dispatch: Dispatch,
}

impl DiskReactor {
    /// `inner` and its metadata, with each of `disks` (by start-page name) answering the
    /// `disk://` URL that names it, and `output` (the run's `disk.out`, with the allocator its
    /// bounce buffers come from) taking the files written under [`out::URL`].
    pub fn new(
        inner: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
        disks: impl IntoIterator<Item = Disk>,
        output: Option<(Disk, Arc<dyn Allocator>)>,
    ) -> std::io::Result<DiskReactor> {
        let disks = disks
            .into_iter()
            .filter_map(|d| {
                let name = d.name().strip_prefix(devices::INPUT_PREFIX)?.to_string();
                Some((format!("{}{name}", devices::SCHEME), d))
            })
            .collect();
        let dispatch = dispatcher()?;
        let output = output.map(|(disk, alloc)| {
            let tier = if alloc.is_pinned() {
                Tier::PinnedHost
            } else {
                Tier::Host
            };
            Output::new(disk, alloc, tier, dispatch.clone())
        });
        Ok(DiskReactor {
            inner,
            metadata,
            disks,
            output,
            volumes: BTreeMap::new(),
            dispatch,
        })
    }

    /// The reactor with volumes in front of it too, each answering the `disk://<serial>/` URLs
    /// of its serial: each of `sources` the files of the layer it holds, each of `sinks` the
    /// layer the run writes onto it, its bounce buffers from `alloc`.
    pub fn with_volumes(
        mut self,
        sources: impl IntoIterator<Item = Arc<Files>>,
        sinks: impl IntoIterator<Item = Volume>,
        alloc: Arc<dyn Allocator>,
    ) -> DiskReactor {
        for files in sources {
            let serial = files.volume().serial().to_string();
            self.volumes.insert(serial, Mounted::Source(files));
        }
        let tier = if alloc.is_pinned() {
            Tier::PinnedHost
        } else {
            Tier::Host
        };
        for volume in sinks {
            let serial = volume.serial().to_string();
            let layer = Output::on_volume(volume, Arc::clone(&alloc), tier, self.dispatch.clone());
            self.volumes.insert(serial, Mounted::Sink(layer));
        }
        self
    }

    /// The layers the run writes onto its sinks' volumes, which its end finishes.
    pub fn sinks(&self) -> Vec<Arc<Output>> {
        self.volumes
            .values()
            .filter_map(|m| match m {
                Mounted::Sink(layer) => Some(Arc::clone(layer)),
                Mounted::Source(_) => None,
            })
            .collect()
    }

    /// The volume a `disk://<serial>/<path>` URL names, its serial and the path.
    fn mounted<'a>(&self, url: &'a str) -> Option<(&Mounted, &'a str, &'a str)> {
        let (serial, path) = volume::parse(url)?;
        self.volumes.get(serial).map(|m| (m, serial, path))
    }

    fn resolved<T: Send + 'static>(&self, r: moruna_kernel::Result<T>) -> Completion<T> {
        Completion::resolved_on(r, self.dispatch.clone())
    }

    /// A source volume's file read into `dst`.
    fn read_volume(
        &self,
        files: &Files,
        url: &str,
        path: &str,
        offset: u64,
        dst: Buffer,
    ) -> Completion<Buffer> {
        let file = match files.file(path) {
            Ok(Some(file)) => file,
            Ok(None) => return self.resolved(Err(io("read_object", url, "no such file"))),
            Err(why) => return self.resolved(Err(io("read_object", url, why))),
        };
        let Some(addr) = dst.host_ptr() else {
            return self.resolved(Err(io(
                "read_object",
                url,
                "a volume reads into host memory only",
            )));
        };
        let len = dst.len() as u64;
        let (tx, rx) = Completion::channel_on(self.dispatch.clone());
        let target = url.to_string();
        files.read(
            &file,
            offset,
            addr as u64,
            len,
            Box::new(move |r| {
                tx.resolve(
                    r.map(|()| dst)
                        .map_err(|why| io("read_object", &target, why)),
                );
            }),
        );
        rx
    }

    /// The run's output layer, when it has an output disk.
    pub fn output(&self) -> Option<Arc<Output>> {
        self.output.clone()
    }

    /// The path under the output layer a URL names, if it names one.
    fn output_path(url: &str) -> Option<&str> {
        url.strip_prefix(out::URL)
    }

    /// The disk a URL names.
    fn disk(&self, url: &str) -> Option<&Disk> {
        self.disks.get(url)
    }

    /// The URLs the disks answer.
    pub fn urls(&self) -> Vec<String> {
        self.disks.keys().cloned().collect()
    }

    fn meta(url: &str, disk: &Disk) -> ObjectMeta {
        ObjectMeta {
            url: url.to_string(),
            size: disk.length(),
            last_modified_ns: None,
            e_tag: None,
        }
    }
}

/// Late `then` callbacks run on a thread of their own, never on the thread that registers
/// them (`moruna_kernel::Dispatch`).
fn dispatcher() -> std::io::Result<Dispatch> {
    let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
    std::thread::Builder::new()
        .name("moruna-blk-dispatch".into())
        .spawn(move || {
            while let Ok(f) = rx.recv() {
                f();
            }
        })?;
    let tx = Mutex::new(tx);
    Ok(Arc::new(move |f| {
        let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(f);
    }))
}

fn io(op: &'static str, target: &str, msg: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op,
        target: target.to_string(),
        msg: msg.into(),
    }
}

impl Reactor for DiskReactor {
    fn read_file(&self, path: &Path, offset: u64, dst: Buffer) -> Completion<Buffer> {
        self.inner.read_file(path, offset, dst)
    }

    fn read_file_opt(
        &self,
        path: &Path,
        offset: u64,
        dst: Buffer,
        allow_short: bool,
    ) -> Completion<(Buffer, usize)> {
        self.inner.read_file_opt(path, offset, dst, allow_short)
    }

    fn write_file(&self, path: &Path, offset: u64, src: BufferView) -> Completion<()> {
        self.inner.write_file(path, offset, src)
    }

    fn read_object(&self, url: &str, offset: u64, dst: Buffer) -> Completion<Buffer> {
        let Some(disk) = self.disk(url) else {
            return match self.mounted(url) {
                Some((Mounted::Source(files), _, path)) => {
                    self.read_volume(files, url, path, offset, dst)
                }
                Some((Mounted::Sink(_), serial, _)) => self.resolved(Err(io(
                    "read_object",
                    url,
                    format!("volume {serial} is a sink's, which the run writes and does not read"),
                ))),
                None => self.inner.read_object(url, offset, dst),
            };
        };
        let len = dst.len() as u64;
        if offset
            .checked_add(len)
            .is_none_or(|end| end > disk.length())
        {
            return Completion::resolved_on(
                Err(io(
                    "read_object",
                    url,
                    format!(
                        "{len} bytes at {offset} are past the file's {} bytes",
                        disk.length()
                    ),
                )),
                self.dispatch.clone(),
            );
        }
        let Some(addr) = dst.host_ptr() else {
            return Completion::resolved_on(
                Err(io("read_object", url, "a disk reads into host memory only")),
                self.dispatch.clone(),
            );
        };
        let (tx, rx) = Completion::channel_on(self.dispatch.clone());
        let target = url.to_string();
        disk.submit(Op {
            kind: Kind::Read,
            offset,
            addr: addr as u64,
            len,
            done: Box::new(move |r| {
                tx.resolve(
                    r.map(|()| dst)
                        .map_err(|why| io("read_object", &target, why)),
                );
            }),
        });
        rx
    }

    fn write_object(&self, url: &str, src: BufferView) -> Completion<()> {
        if self.disk(url).is_some() {
            return Completion::resolved_on(
                Err(io("write_object", url, "an input disk is read only")),
                self.dispatch.clone(),
            );
        }
        if let Some(path) = DiskReactor::output_path(url)
            && let Some(output) = &self.output
        {
            return output.put(path, src);
        }
        match self.mounted(url) {
            Some((Mounted::Sink(layer), _, path)) => layer.put(path, src),
            Some((Mounted::Source(_), serial, _)) => self.resolved(Err(io(
                "write_object",
                url,
                format!("volume {serial} is a source's, which is read only"),
            ))),
            None if DiskReactor::output_path(url).is_some() => {
                self.resolved(Err(io("write_object", url, "the run has no output disk")))
            }
            None => self.inner.write_object(url, src),
        }
    }

    fn copy(&self, src: CopySrc, dst: CopyDst) -> Completion<Option<Buffer>> {
        self.inner.copy(src, dst)
    }

    fn register_segment(&self, segment: u32, path: &Path) -> moruna_kernel::Result<()> {
        self.inner.register_segment(segment, path)
    }

    fn unregister_segment(&self, segment: u32) {
        self.inner.unregister_segment(segment)
    }

    fn delete_object(&self, url: &str) -> Completion<()> {
        if self.disk(url).is_some() {
            return Completion::resolved_on(
                Err(io("delete_object", url, "an input disk is read only")),
                self.dispatch.clone(),
            );
        }
        if let Some((mounted, serial, path)) = self.mounted(url)
            && (self.output.is_none() || DiskReactor::output_path(url).is_none())
        {
            let result = match mounted {
                Mounted::Sink(layer) if layer.remove(path) => Ok(()),
                Mounted::Sink(_) => Err(io("delete_object", url, "no such file in the layer")),
                Mounted::Source(_) => Err(io(
                    "delete_object",
                    url,
                    format!("volume {serial} is a source's, which is read only"),
                )),
            };
            return self.resolved(result);
        }
        if let Some(path) = DiskReactor::output_path(url) {
            // A file taken out of the layer; its pages stay on the disk, unnamed.
            let removed = self.output.as_ref().is_some_and(|o| o.remove(path));
            let result = if removed {
                Ok(())
            } else {
                Err(io("delete_object", url, "no such file in the output layer"))
            };
            return Completion::resolved_on(result, self.dispatch.clone());
        }
        self.inner.delete_object(url)
    }

    fn abort_multipart(&self, url: &str, upload_id: &str) -> Completion<()> {
        self.inner.abort_multipart(url, upload_id)
    }

    fn paths(&self) -> IoPaths {
        self.inner.paths()
    }

    /// The wrapped reactor's shutdown. The disks are the process's, not the run's, and stay
    /// open for the next run.
    fn shutdown(&self) {
        self.inner.shutdown();
    }
}

impl ObjectMetadata for DiskReactor {
    fn head_object(&self, url: &str) -> Completion<ObjectMeta> {
        if let Some(disk) = self.disk(url) {
            return Completion::resolved_on(
                Ok(DiskReactor::meta(url, disk)),
                self.dispatch.clone(),
            );
        }
        if let Some((mounted, serial, path)) = self.mounted(url) {
            let found = match mounted {
                Mounted::Source(files) => files.file(path),
                Mounted::Sink(_) => Err(format!(
                    "volume {serial} is a sink's, which the run writes and does not read"
                )),
            };
            return self.resolved(match found {
                Ok(Some(file)) => Ok(ObjectMeta {
                    url: url.to_string(),
                    size: file.size,
                    last_modified_ns: None,
                    e_tag: None,
                }),
                Ok(None) => Err(io("head_object", url, "no such file")),
                Err(why) => Err(io("head_object", url, why)),
            });
        }
        match &self.metadata {
            Some(m) => m.head_object(url),
            None => Completion::resolved_on(
                Err(io("head_object", url, "no object store is configured")),
                self.dispatch.clone(),
            ),
        }
    }

    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>> {
        if let Some((mounted, serial, path)) = self.mounted(url) {
            let listed = match mounted {
                Mounted::Source(files) => files.list(path),
                Mounted::Sink(_) => Err(format!(
                    "volume {serial} is a sink's, which the run writes and does not read"
                )),
            };
            return self.resolved(
                listed
                    .map(|files| {
                        files
                            .into_iter()
                            .map(|(path, size)| ObjectMeta {
                                url: format!("{}{serial}/{path}", volume::SCHEME),
                                size,
                                last_modified_ns: None,
                                e_tag: None,
                            })
                            .collect()
                    })
                    .map_err(|why| io("list_prefix", url, why)),
            );
        }
        if url.starts_with(devices::SCHEME) {
            // A disk holds one file; a prefix of the scheme lists the disks it names.
            let found = self
                .disks
                .iter()
                .filter(|(u, _)| u.starts_with(url))
                .map(|(u, d)| DiskReactor::meta(u, d))
                .collect();
            return Completion::resolved_on(Ok(found), self.dispatch.clone());
        }
        match &self.metadata {
            Some(m) => m.list_prefix(url),
            None => Completion::resolved_on(
                Err(io("list_prefix", url, "no object store is configured")),
                self.dispatch.clone(),
            ),
        }
    }
}
