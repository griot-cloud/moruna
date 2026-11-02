//! Strict raw-device capability. Minted only from actual Present/no-fallback paths and
//! bound to a verified O_DIRECT descriptor. It never reopens a path through the fd cache.
#![forbid(unsafe_code)]

use crate::Reactor;
use crate::runtime::{ReadFileOp, WriteFileOp};
use moruna_kernel::{Buffer, BufferView, Completion, MorunaError, Result};
use moruna_storage::raw::{AlignedDevice, Direct};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

struct Strict {
    reactor: Arc<Reactor>,
}
struct Bound {
    reactor: Arc<Reactor>,
    fd: Arc<OwnedFd>,
}
impl Reactor {
    /// Mint a mandatory direct/io_uring storage capability from the actual selected
    /// guaranteed paths. Probed availability and IoPaths reporting flags are insufficient.
    pub fn strict_storage(self: &Arc<Self>) -> Result<Arc<dyn Direct>> {
        let paths = self.inner.paths;
        if !self.open()
            || !paths.direct.on
            || !paths.direct.guaranteed
            || !paths.uring.on
            || !paths.uring.guaranteed
            || self.inner.engine.name() != "uring"
        {
            return Err(MorunaError::Config { name: "raw_storage", msg: "raw storage needs Present direct IO and io_uring, with the actual uring engine and no fallback".into() });
        }
        Ok(Arc::new(Strict {
            reactor: Arc::clone(self),
        }))
    }
}
impl Direct for Strict {
    fn bind(
        &self,
        fd: Arc<OwnedFd>,
    ) -> std::result::Result<Arc<dyn AlignedDevice>, Box<MorunaError>> {
        if !crate::file_blocking::direct_flag(&fd).map_err(|e| MorunaError::Io {
            op: "bind_raw",
            target: "owned descriptor".into(),
            msg: e.to_string(),
        })? {
            return Err(MorunaError::Config {
                name: "raw_storage",
                msg: "raw descriptor is not O_DIRECT".into(),
            }
            .into());
        }
        Ok(Arc::new(Bound {
            reactor: Arc::clone(&self.reactor),
            fd,
        }))
    }
}
impl AlignedDevice for Bound {
    fn read(&self, offset: u64, dst: Buffer) -> Completion<Buffer> {
        let (tx, completion) = Completion::channel_on(self.reactor.dispatch.clone());
        self.reactor.send(
            |q| &q.read_file,
            ReadFileOp {
                strict_fd: Some(Arc::clone(&self.fd)),
                path: PathBuf::from("[owned raw device]"),
                offset,
                dst,
                tx,
            },
        );
        completion
    }
    fn write(&self, offset: u64, src: BufferView) -> Completion<()> {
        let (tx, completion) = Completion::channel_on(self.reactor.dispatch.clone());
        self.reactor.send(
            |q| &q.write_file,
            WriteFileOp {
                strict_fd: Some(Arc::clone(&self.fd)),
                path: PathBuf::from("[owned raw device]"),
                offset,
                src,
                tx,
            },
        );
        completion
    }
}
