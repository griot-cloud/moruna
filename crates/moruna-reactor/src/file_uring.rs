//! Linux io_uring adapter for the single-owner lifecycle of SDD06 l. The owner alone
//! accesses SQ/CQ; partial retries never push from a separate reaper thread.
#![cfg(all(target_os = "linux", feature = "uring"))]

use crate::file_blocking::{Done, FileEngine, FileReq, Verb};
use crate::file_lifecycle::{self, CHUNK, Port, Shared};
use io_uring::{IoUring, opcode, types};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

struct Ring(IoUring);
impl Port for Ring {
    fn push(&mut self, id: u64, req: &FileReq, transferred: usize) -> bool {
        let fd = types::Fd(req.fd.as_raw_fd());
        let ptr = (req.ptr + transferred) as *mut u8;
        let len = (req.len - transferred).min(CHUNK) as u32;
        let offset = req.offset + transferred as u64;
        let entry = match req.verb {
            Verb::Read => opcode::Read::new(fd, ptr, len).offset(offset).build(),
            Verb::Write => opcode::Write::new(fd, ptr.cast_const(), len)
                .offset(offset)
                .build(),
        }
        .user_data(id);
        // SAFETY: only this owner accesses SQ. The lifecycle retains FileReq's descriptor
        // and Done's charged buffer/view until the consumed original CQE, or closes this
        // non-SQPOLL ring before releasing an entry proven never consumed by the kernel.
        unsafe { self.0.submission().push(&entry).is_ok() }
    }
    fn submit(&mut self) -> io::Result<usize> {
        self.0.submit()
    }
    fn unconsumed(&mut self) -> usize {
        self.0.submission().len()
    }
    fn complete(&mut self) -> Option<(u64, i32)> {
        self.0
            .completion()
            .next()
            .map(|c| (c.user_data(), c.result()))
    }
}
pub(crate) struct UringEngine {
    shared: Arc<Shared>,
    owner: Option<std::thread::JoinHandle<()>>,
}
impl UringEngine {
    pub(crate) fn new(depth: u32) -> io::Result<Self> {
        // This constructor does not enable SQPOLL: unconsumed-entry shutdown relies on
        // the owner being the only code that may enter and submit this ring.
        let ring = Ring(IoUring::new(depth.max(4))?);
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let owner = std::thread::Builder::new()
            .name("moruna-uring-owner".into())
            .spawn(move || file_lifecycle::run(ring, worker))?;
        Ok(Self {
            shared,
            owner: Some(owner),
        })
    }
}
impl FileEngine for UringEngine {
    fn submit(&self, req: FileReq, done: Done) {
        self.shared.submit(req, done);
    }
    fn name(&self) -> &'static str {
        "uring"
    }
    fn terminal_failure(&self) -> Option<String> {
        self.shared.terminal_failure()
    }
}
impl Drop for UringEngine {
    fn drop(&mut self) {
        self.shared.stop();
        if let Some(owner) = self.owner.take() {
            if let Err(cause) = owner.join() {
                tracing::error!(target: "reactor", cause = ?cause, "io_uring owner panicked during drain");
            }
        }
    }
}
