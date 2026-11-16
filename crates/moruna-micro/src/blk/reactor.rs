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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use moruna_kernel::{
    Buffer, BufferView, Completion, CopyDst, CopySrc, Dispatch, IoPaths, MorunaError, ObjectMeta,
    ObjectMetadata, Reactor,
};

use super::disk::Disk;
use super::engine::{Kind, Op};
use crate::devices;

/// The reactor with the engine's disks in front of it.
pub struct DiskReactor {
    inner: Arc<dyn Reactor>,
    metadata: Option<Arc<dyn ObjectMetadata>>,
    disks: BTreeMap<String, Disk>,
    dispatch: Dispatch,
}

impl DiskReactor {
    /// `inner` and its metadata, with each of `disks` (by start-page name) answering the
    /// `disk://` URL that names it.
    pub fn new(
        inner: Arc<dyn Reactor>,
        metadata: Option<Arc<dyn ObjectMetadata>>,
        disks: impl IntoIterator<Item = Disk>,
    ) -> std::io::Result<DiskReactor> {
        let disks = disks
            .into_iter()
            .filter_map(|d| {
                let name = d.name().strip_prefix(devices::INPUT_PREFIX)?.to_string();
                Some((format!("{}{name}", devices::SCHEME), d))
            })
            .collect();
        Ok(DiskReactor {
            inner,
            metadata,
            disks,
            dispatch: dispatcher()?,
        })
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
            return self.inner.read_object(url, offset, dst);
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
        self.inner.write_object(url, src)
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
        self.inner.delete_object(url)
    }

    fn abort_multipart(&self, url: &str, upload_id: &str) -> Completion<()> {
        self.inner.abort_multipart(url, upload_id)
    }

    fn paths(&self) -> IoPaths {
        self.inner.paths()
    }

    fn shutdown(&self) {
        self.inner.shutdown();
        for disk in self.disks.values() {
            disk.close();
        }
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
        match &self.metadata {
            Some(m) => m.head_object(url),
            None => Completion::resolved_on(
                Err(io("head_object", url, "no object store is configured")),
                self.dispatch.clone(),
            ),
        }
    }

    fn list_prefix(&self, url: &str) -> Completion<Vec<ObjectMeta>> {
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
