//! Per-path descriptors and the sticky-buffered set (06 f.1, f.9, l).
//!
//! Files are opened once per `(path, mode, direct flag)` and the descriptor is cached for the
//! run, because a per-operation `open` is the anti-pattern section l names. `O_DIRECT` is
//! asked for only when the direct IO path is selected and the path is not already known to
//! refuse it; a refusal on an available path is remembered for the rest of the run (f.9).

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use moruna_kernel::{MorunaError, Result};

use crate::stats::Counters;

/// What a descriptor is opened for. A read never creates the file; a write does.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub(crate) enum Mode {
    /// `O_RDONLY`.
    Read,
    /// `O_RDWR | O_CREAT`.
    Write,
}

/// How a file is opened; a seam so the tests of f.9 can make one path refuse `O_DIRECT`
/// without a filesystem that does (RE-T12 names exactly such a shim).
pub(crate) trait Opener: Send + Sync {
    /// Open `path`, asking for direct IO when `direct`. `EINVAL` means "this filesystem
    /// refuses direct IO on this path" and is what drives the sticky fallback.
    fn open(&self, path: &Path, mode: Mode, direct: bool) -> io::Result<OwnedFd>;
}

/// The real opener: `open(2)`, with `O_DIRECT` on Linux and `F_NOCACHE` on macOS.
pub(crate) struct SysOpener;

impl Opener for SysOpener {
    fn open(&self, path: &Path, mode: Mode, direct: bool) -> io::Result<OwnedFd> {
        let mut opts = std::fs::OpenOptions::new();
        match mode {
            Mode::Read => opts.read(true),
            Mode::Write => opts.read(true).write(true).create(true),
        };
        #[cfg(target_os = "linux")]
        if direct {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_DIRECT);
        }
        let file = opts.open(path)?;
        let owned = OwnedFd::from(file);
        // macOS has no `O_DIRECT`; `F_NOCACHE` is the nearest thing (preamble 6.6), so the
        // direct path here bypasses the page cache without the alignment enforcement a Linux
        // `O_DIRECT` open carries. Same selection, same fallback; the real thing runs in the
        // weekly container job.
        #[cfg(target_os = "macos")]
        if direct {
            crate::file_blocking::set_nocache(&owned)?;
        }
        let _ = direct;
        Ok(owned)
    }
}

/// Descriptors for the run, keyed by `(path, mode, effective direct flag)`.
pub(crate) struct FdCache {
    opener: Arc<dyn Opener>,
    map: Mutex<HashMap<(PathBuf, Mode, bool), Arc<OwnedFd>>>,
    sticky: RwLock<HashSet<PathBuf>>,
}

impl FdCache {
    pub(crate) fn new(opener: Arc<dyn Opener>) -> FdCache {
        FdCache {
            opener,
            map: Mutex::new(HashMap::new()),
            sticky: RwLock::new(HashSet::new()),
        }
    }

    /// True when `path` has already refused `O_DIRECT` this run (f.9).
    pub(crate) fn is_sticky(&self, path: &Path) -> bool {
        self.sticky
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(path)
    }

    /// A descriptor for `path`, direct when `want_direct` and the path has not refused it.
    /// Returns the descriptor and whether it is a direct one.
    ///
    /// `guaranteed` is the `Present` half of RE-I3: on a guaranteed path an `EINVAL` from an
    /// `O_DIRECT` open is a platform bug and becomes `Config`, never a silent fallback.
    pub(crate) fn acquire(
        &self,
        path: &Path,
        mode: Mode,
        want_direct: bool,
        guaranteed: bool,
        counters: &Counters,
    ) -> Result<(Arc<OwnedFd>, bool)> {
        let direct = want_direct && !self.is_sticky(path);
        if let Some(fd) = self.lookup(path, mode, direct) {
            return Ok((fd, direct));
        }
        match self.opener.open(path, mode, direct) {
            Ok(fd) => Ok((self.insert(path, mode, direct, fd), direct)),
            Err(e) if direct && e.raw_os_error() == Some(libc::EINVAL) => {
                if guaranteed {
                    return Err(MorunaError::Config {
                        name: "host_profile",
                        msg: format!(
                            "direct_io_staging is Present but O_DIRECT is EINVAL on {}",
                            path.display()
                        ),
                    });
                }
                self.mark_sticky(path, counters);
                let fd = self.open_buffered(path, mode)?;
                Ok((fd, false))
            }
            Err(e) => Err(io_error("open", path, &e)),
        }
    }

    /// A buffered descriptor whatever the direct path says; used for the operations e.3 sends
    /// buffered (a piece of a staging record that is not a page multiple).
    pub(crate) fn acquire_buffered(&self, path: &Path, mode: Mode) -> Result<Arc<OwnedFd>> {
        if let Some(fd) = self.lookup(path, mode, false) {
            return Ok(fd);
        }
        self.open_buffered(path, mode)
    }

    /// Close every descriptor for `path` (at `unregister_segment`, so an unlinked file's space
    /// is returned at unlink, RE-I8).
    pub(crate) fn forget(&self, path: &Path) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|(p, _, _), _| p != path);
    }

    /// Close every cached descriptor (f.7).
    pub(crate) fn clear(&self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }

    /// How many descriptors are cached; for the shutdown test (RE-T7).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn lookup(&self, path: &Path, mode: Mode, direct: bool) -> Option<Arc<OwnedFd>> {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&(path.to_path_buf(), mode, direct)).map(Arc::clone)
    }

    fn insert(&self, path: &Path, mode: Mode, direct: bool, fd: OwnedFd) -> Arc<OwnedFd> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            map.entry((path.to_path_buf(), mode, direct))
                .or_insert_with(|| Arc::new(fd)),
        )
    }

    fn open_buffered(&self, path: &Path, mode: Mode) -> Result<Arc<OwnedFd>> {
        match self.opener.open(path, mode, false) {
            Ok(fd) => Ok(self.insert(path, mode, false, fd)),
            Err(e) => Err(io_error("open", path, &e)),
        }
    }

    fn mark_sticky(&self, path: &Path, counters: &Counters) {
        let first = {
            let mut set = self.sticky.write().unwrap_or_else(|e| e.into_inner());
            set.insert(path.to_path_buf())
        };
        if first {
            counters.note_sticky_fallback();
            counters.warn_sticky(path);
        }
    }
}

/// One `io::Error` as the contract's `Io` (contracts d.14).
pub(crate) fn io_error(op: &'static str, target: &Path, e: &io::Error) -> MorunaError {
    MorunaError::Io {
        op,
        target: target.display().to_string(),
        msg: e.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The shim RE-T12 names: `open` returns `EINVAL` for one path when direct IO is asked
    /// for, and counts every direct attempt so a test can prove no later attempt is made.
    pub(crate) struct DirectRefusingOpener {
        pub(crate) refuse: PathBuf,
        pub(crate) direct_attempts: AtomicU64,
    }

    impl DirectRefusingOpener {
        pub(crate) fn new(refuse: impl Into<PathBuf>) -> Arc<DirectRefusingOpener> {
            Arc::new(DirectRefusingOpener {
                refuse: refuse.into(),
                direct_attempts: AtomicU64::new(0),
            })
        }

        pub(crate) fn attempts(&self) -> u64 {
            self.direct_attempts.load(Ordering::SeqCst)
        }
    }

    impl Opener for DirectRefusingOpener {
        fn open(&self, path: &Path, mode: Mode, direct: bool) -> io::Result<OwnedFd> {
            if direct {
                self.direct_attempts.fetch_add(1, Ordering::SeqCst);
                if path == self.refuse {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
            }
            SysOpener.open(path, mode, direct)
        }
    }

    pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
        // Preamble 6.7: a scratch directory unique to this process, never a fixed path.
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("moruna-reactor-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_path_that_refuses_direct_is_remembered_for_the_run() {
        let dir = scratch_dir("sticky");
        let bad = dir.join("bad");
        let good = dir.join("good");
        std::fs::write(&bad, b"x").expect("write");
        std::fs::write(&good, b"x").expect("write");
        let opener = DirectRefusingOpener::new(bad.clone());
        let cache = FdCache::new(opener.clone());
        let counters = Counters::default();
        let (_, direct) = cache
            .acquire(&bad, Mode::Read, true, false, &counters)
            .expect("open");
        assert!(!direct);
        assert_eq!(counters.snapshot().sticky_fallbacks, 1);
        assert_eq!(counters.warn_counts().sticky, 1);
        let attempts = opener.attempts();
        for _ in 0..100 {
            let (_, direct) = cache
                .acquire(&bad, Mode::Read, true, false, &counters)
                .expect("open");
            assert!(!direct);
        }
        assert_eq!(opener.attempts(), attempts, "no later O_DIRECT attempt");
        assert_eq!(counters.snapshot().sticky_fallbacks, 1);
        let (_, direct) = cache
            .acquire(&good, Mode::Read, true, false, &counters)
            .expect("open");
        assert!(direct, "another path stays direct");
        cache.clear();
        assert_eq!(cache.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_guaranteed_direct_path_that_refuses_is_a_config_error() {
        let dir = scratch_dir("guaranteed");
        let bad = dir.join("bad");
        std::fs::write(&bad, b"x").expect("write");
        let cache = FdCache::new(DirectRefusingOpener::new(bad.clone()));
        let counters = Counters::default();
        let err = cache
            .acquire(&bad, Mode::Read, true, true, &counters)
            .expect_err("Present must not fall back");
        assert!(matches!(
            err,
            MorunaError::Config {
                name: "host_profile",
                ..
            }
        ));
        assert_eq!(counters.snapshot().sticky_fallbacks, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn descriptors_are_cached_and_forgettable() {
        let dir = scratch_dir("cache");
        let file = dir.join("f");
        std::fs::write(&file, b"x").expect("write");
        let cache = FdCache::new(Arc::new(SysOpener));
        let counters = Counters::default();
        let (a, _) = cache
            .acquire(&file, Mode::Read, false, false, &counters)
            .expect("open");
        let (b, _) = cache
            .acquire(&file, Mode::Read, false, false, &counters)
            .expect("open");
        assert!(Arc::ptr_eq(&a, &b), "one descriptor per (path, mode, flag)");
        let c = cache.acquire_buffered(&file, Mode::Read).expect("open");
        assert!(Arc::ptr_eq(&a, &c));
        assert_eq!(cache.len(), 1);
        cache.forget(&file);
        assert_eq!(cache.len(), 0);
        let missing = dir.join("absent");
        let err = cache
            .acquire(&missing, Mode::Read, false, false, &counters)
            .expect_err("a read never creates the file");
        assert!(matches!(err, MorunaError::Io { op: "open", .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
