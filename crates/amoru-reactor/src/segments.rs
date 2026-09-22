//! The staging segment registry (06 e.4, RE-I8).
//!
//! A segment is opened once, at `register_segment`, and its descriptor lives exactly as long
//! as the registration: `unregister_segment` drops the registry's reference, and the file is
//! closed when the last operation still holding it resolves, so an unlinked segment's space
//! is returned to the filesystem at unlink rather than at process exit.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use amoru_kernel::{AmoruError, Result};

use crate::gds;

/// One registered segment.
pub(crate) struct SegmentEntry {
    /// The segment file.
    pub(crate) path: PathBuf,
    /// Its descriptor; an in-flight operation holds a clone, which is what keeps the file open
    /// past `unregister_segment` and no longer (RE-I8).
    pub(crate) fd: Arc<OwnedFd>,
    /// Whether the descriptor was opened for direct IO (f.9 may have made it buffered).
    pub(crate) direct: bool,
    /// The cuFile handle, when the GDS path is selected (e.4).
    pub(crate) gds_handle: Option<gds::Handle>,
}

/// Segment number to entry. Read on every disk operation, written by the two registry calls.
#[derive(Default)]
pub(crate) struct Segments {
    map: RwLock<HashMap<u32, Arc<SegmentEntry>>>,
}

impl Segments {
    /// Record `(segment, path, fd)`. A second registration of the same number is an error:
    /// segments are immutable (h).
    pub(crate) fn register(&self, segment: u32, entry: SegmentEntry) -> Result<()> {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        if map.contains_key(&segment) {
            return Err(AmoruError::Io {
                op: "register_segment",
                target: entry.path.display().to_string(),
                msg: "already registered".into(),
            });
        }
        map.insert(segment, Arc::new(entry));
        Ok(())
    }

    /// Forget `segment`; unregistering an unknown number is a no-op (d.1). Returns the path
    /// when something was removed, so the caller can drop the path's cached descriptors too.
    pub(crate) fn unregister(&self, segment: u32) -> Option<PathBuf> {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        map.remove(&segment).map(|e| e.path.clone())
    }

    /// The entry for a segment number; the read guard is dropped before submission (g).
    pub(crate) fn get(&self, segment: u32) -> Option<Arc<SegmentEntry>> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.get(&segment).map(Arc::clone)
    }

    /// The entry whose path is `path`, so `read_file` and `write_file` on a registered segment
    /// use its descriptor instead of the fd cache (e.4).
    pub(crate) fn by_path(&self, path: &Path) -> Option<Arc<SegmentEntry>> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        map.values().find(|e| e.path == path).map(Arc::clone)
    }

    /// Drop every registration (f.7).
    pub(crate) fn clear(&self) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        map.clear();
    }

    /// How many segments are registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The error RE-I8 names for a `copy` that reaches an unregistered segment.
pub(crate) fn not_registered(segment: u32) -> AmoruError {
    AmoruError::Io {
        op: "copy",
        target: format!("segment {segment}"),
        msg: "segment not registered".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fdcache::tests::scratch_dir;
    use crate::fdcache::{FdCache, Mode, SysOpener};
    use crate::stats::Counters;

    fn entry(path: &Path) -> SegmentEntry {
        let cache = FdCache::new(Arc::new(SysOpener));
        let (fd, direct) = cache
            .acquire(path, Mode::Write, false, false, &Counters::default())
            .expect("open");
        SegmentEntry {
            path: path.to_path_buf(),
            fd,
            direct,
            gds_handle: None,
        }
    }

    #[test]
    fn a_number_registers_once_and_unregisters_idempotently() {
        let dir = scratch_dir("segments");
        let path = dir.join("seg-0");
        let segments = Segments::default();
        segments.register(7, entry(&path)).expect("first");
        let err = segments.register(7, entry(&path)).expect_err("second");
        assert!(matches!(
            err,
            AmoruError::Io {
                op: "register_segment",
                ..
            }
        ));
        assert_eq!(segments.len(), 1);
        assert!(segments.get(7).is_some());
        assert!(segments.by_path(&path).is_some());
        assert!(segments.by_path(&dir.join("other")).is_none());
        assert_eq!(segments.unregister(7).as_deref(), Some(path.as_path()));
        assert!(segments.unregister(7).is_none());
        assert!(segments.get(7).is_none());
        segments.clear();
        assert_eq!(segments.len(), 0);
        let err = not_registered(9);
        assert!(matches!(err, AmoruError::Io { op: "copy", .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
