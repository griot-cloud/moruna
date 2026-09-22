//! File IO on the blocking pool (06 f.2), and the trait both file paths implement.
//!
//! This is the fallback of the io_uring row of e.2 and the only file path on a host without
//! io_uring. It is `pread`/`pwrite` in a loop on `tokio`'s blocking pool: a single `pread` may
//! return fewer bytes than asked for without meaning end of file, so the loop continues until
//! the length is reached or a read returns zero (f.1, f.2).
//!
//! `unsafe` is permitted here (section l): every block is a `pread`, a `pwrite` or an
//! `fcntl` over a descriptor the request holds open and a buffer the operation's `Buffer` or
//! `BufferView` keeps alive until the completion resolves (RE-I1).

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// Which way the bytes move.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Verb {
    /// `pread` into the destination buffer.
    Read,
    /// `pwrite` from the source view.
    Write,
}

/// One whole file operation, ready for either engine. The pointer is carried as an integer so
/// the request is `Send`; `fd` and `owner` keep the descriptor and the bytes alive until the
/// engine calls back (RE-I1).
pub(crate) struct FileReq {
    pub(crate) fd: Arc<OwnedFd>,
    pub(crate) verb: Verb,
    pub(crate) ptr: usize,
    pub(crate) len: usize,
    pub(crate) offset: u64,
}

/// What an engine calls when the operation is finished, with the bytes transferred.
pub(crate) type Done = Box<dyn FnOnce(io::Result<usize>) + Send + 'static>;

/// A file path (e.2): io_uring or the blocking pool. Both produce byte-identical output, which
/// is what G-I7 asks of every direct path and what RE-T10 proves.
pub(crate) trait FileEngine: Send + Sync {
    /// Run one operation to completion off the caller's thread and hand the result to `done`.
    /// Never blocks the caller (RE-I6).
    fn submit(&self, req: FileReq, done: Done);
    /// The engine's name, for the counters and the fallback warn.
    fn name(&self) -> &'static str;
}

/// `pread`/`pwrite` on `tokio`'s blocking pool (f.2).
pub(crate) struct BlockingEngine {
    handle: tokio::runtime::Handle,
}

impl BlockingEngine {
    pub(crate) fn new(handle: tokio::runtime::Handle) -> BlockingEngine {
        BlockingEngine { handle }
    }
}

impl FileEngine for BlockingEngine {
    fn submit(&self, req: FileReq, done: Done) {
        self.handle.spawn_blocking(move || {
            let result = run(&req);
            done(result);
        });
    }

    fn name(&self) -> &'static str {
        "blocking"
    }
}

/// Run one request synchronously. Called on the blocking pool, never on a reactor worker
/// thread and never on a caller's thread.
pub(crate) fn run(req: &FileReq) -> io::Result<usize> {
    match req.verb {
        Verb::Read => read_at(req.fd.as_raw_fd(), req.ptr, req.len, req.offset),
        Verb::Write => write_at(req.fd.as_raw_fd(), req.ptr, req.len, req.offset),
    }
}

/// Read until `len` bytes or end of file; returns the bytes read (f.1, f.2).
pub(crate) fn read_at(fd: RawFd, ptr: usize, len: usize, offset: u64) -> io::Result<usize> {
    let mut done = 0usize;
    while done < len {
        // SAFETY: `fd` is open for the call (the request holds it), and `ptr + done` is inside
        // a destination buffer of at least `len` bytes that the caller's `Buffer` keeps alive
        // until this operation resolves (RE-I1).
        let n = unsafe {
            libc::pread(
                fd,
                (ptr + done) as *mut libc::c_void,
                len - done,
                (offset + done as u64) as libc::off_t,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break;
        }
        done += n as usize;
    }
    Ok(done)
}

/// Write `len` bytes; a short write is retried from the written offset up to three times
/// before it becomes an error (f.1).
pub(crate) fn write_at(fd: RawFd, ptr: usize, len: usize, offset: u64) -> io::Result<usize> {
    let mut done = 0usize;
    let mut short_writes = 0u8;
    while done < len {
        // SAFETY: `fd` is open for the call, and `ptr + done` is inside a source region of at
        // least `len` bytes that the caller's `BufferView` keeps alive until this operation
        // resolves (RE-I1); the region is only read.
        let n = unsafe {
            libc::pwrite(
                fd,
                (ptr + done) as *const libc::c_void,
                len - done,
                (offset + done as u64) as libc::off_t,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        done += n as usize;
        if done < len {
            short_writes += 1;
            if short_writes > 3 {
                return Err(io::Error::other(format!(
                    "short write: {done} of {len} bytes after 3 retries"
                )));
            }
        }
    }
    Ok(done)
}

/// macOS has no `O_DIRECT`; `F_NOCACHE` is the nearest thing and is what the direct IO path
/// uses there (preamble 6.6). Linux sets `O_DIRECT` at open instead and never calls this.
#[cfg(target_os = "macos")]
pub(crate) fn set_nocache(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor this process owns; `fcntl` only sets a descriptor flag.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_NOCACHE, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fdcache::tests::scratch_dir;
    use std::fs;

    #[test]
    fn a_read_loops_to_the_length_and_stops_at_end_of_file() {
        let dir = scratch_dir("blocking");
        let path = dir.join("f");
        fs::write(&path, vec![7u8; 100]).expect("write");
        let file = fs::File::open(&path).expect("open");
        let mut buf = vec![0u8; 100];
        let n = read_at(file.as_raw_fd(), buf.as_mut_ptr() as usize, 100, 0).expect("read");
        assert_eq!(n, 100);
        assert!(buf.iter().all(|b| *b == 7));
        let n = read_at(file.as_raw_fd(), buf.as_mut_ptr() as usize, 100, 60).expect("read");
        assert_eq!(n, 40, "a read that reaches end of file is short");
        let n = read_at(file.as_raw_fd(), buf.as_mut_ptr() as usize, 100, 1000).expect("read");
        assert_eq!(n, 0, "a read past the end reads nothing");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_write_places_bytes_at_the_offset() {
        let dir = scratch_dir("blocking-write");
        let path = dir.join("f");
        fs::write(&path, vec![0u8; 16]).expect("create");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");
        let src = [3u8; 8];
        let n = write_at(file.as_raw_fd(), src.as_ptr() as usize, 8, 8).expect("write");
        assert_eq!(n, 8);
        let back = fs::read(&path).expect("read back");
        assert_eq!(&back[..8], &[0u8; 8]);
        assert_eq!(&back[8..], &[3u8; 8]);
        let err = read_at(-1, src.as_ptr() as usize, 8, 0).expect_err("a bad descriptor errors");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        let err = write_at(-1, src.as_ptr() as usize, 8, 0).expect_err("a bad descriptor errors");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_engine_runs_off_the_calling_thread() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let dir = scratch_dir("blocking-engine");
        let path = dir.join("f");
        fs::write(&path, vec![9u8; 32]).expect("write");
        let fd = Arc::new(OwnedFd::from(fs::File::open(&path).expect("open")));
        let engine = BlockingEngine::new(rt.handle().clone());
        assert_eq!(engine.name(), "blocking");
        let mut buf = vec![0u8; 32];
        let (tx, rx) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        engine.submit(
            FileReq {
                fd,
                verb: Verb::Read,
                ptr: buf.as_mut_ptr() as usize,
                len: 32,
                offset: 0,
            },
            Box::new(move |r| {
                tx.send((r.map_err(|e| e.to_string()), std::thread::current().id()))
                    .expect("send");
            }),
        );
        let (result, thread) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("completion");
        assert_eq!(result, Ok(32));
        assert_ne!(thread, caller, "the work does not run on the caller");
        assert!(buf.iter().all(|b| *b == 9));
        fs::remove_dir_all(&dir).ok();
    }
}
