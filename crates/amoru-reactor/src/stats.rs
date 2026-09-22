//! Reactor counters (06 j) and the once-per-run warn bookkeeping of e.3 and f.9.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which operation a record is; the key of the once-per-run warns. The six of 06 b, and the
/// two removal calls contracts d.9 added on 2026-09-22 for a sink's resume.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum OpKind {
    /// `read_file`.
    ReadFile,
    /// `read_file_opt`.
    ReadFileOpt,
    /// `write_file`.
    WriteFile,
    /// `read_object`.
    ReadObject,
    /// `write_object`.
    WriteObject,
    /// `copy`.
    Copy,
    /// `delete_object`.
    DeleteObject,
    /// `abort_multipart`.
    AbortMultipart,
}

impl OpKind {
    /// The name used in `AmoruError::Io { op }` and in the `tracing` events.
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::ReadFile => "read_file",
            OpKind::ReadFileOpt => "read_file_opt",
            OpKind::WriteFile => "write_file",
            OpKind::ReadObject => "read_object",
            OpKind::WriteObject => "write_object",
            OpKind::Copy => "copy",
            OpKind::DeleteObject => "delete_object",
            OpKind::AbortMultipart => "abort_multipart",
        }
    }
}

/// What the reactor counted during a run; read by the run report (06 j).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReactorStats {
    /// File operations issued with direct IO.
    pub direct_ops: u64,
    /// File operations issued buffered.
    pub buffered_ops: u64,
    /// File operations issued through io_uring.
    pub uring_ops: u64,
    /// Object reads completed.
    pub object_reads: u64,
    /// Object bytes requested.
    pub object_bytes: u64,
    /// Bytes copied out of a network library's buffer into an arena buffer (06 b).
    pub ingress_bytes: u64,
    /// Host to device copies.
    pub copies_h2d: u64,
    /// Device to host copies.
    pub copies_d2h: u64,
    /// Copies issued through GPUDirect Storage.
    pub copies_gds: u64,
    /// Bytes moved by `copy`.
    pub copy_bytes: u64,
    /// High water mark of operations waiting on a submission queue (f.8).
    pub queued_max: u64,
    /// Operations that resolved with an error, plus panicking `then` callbacks (g).
    pub errors: u64,
    /// Per-operation fallbacks taken (f.9).
    pub fallbacks: u64,
    /// Paths recorded as buffered for the rest of the run (f.9).
    pub sticky_fallbacks: u64,
    /// Bytes staged through the pinned bounce buffer (e.2).
    pub bounce_bytes: u64,
    /// Segments registered since start.
    pub segments_registered: u64,
    /// Segments registered and not yet unregistered.
    pub segments_open: u64,
}

/// The live counters behind [`ReactorStats`], plus the sets that make every warn of e.3 and
/// f.9 fire once per run.
#[derive(Default)]
pub(crate) struct Counters {
    direct_ops: AtomicU64,
    buffered_ops: AtomicU64,
    uring_ops: AtomicU64,
    object_reads: AtomicU64,
    object_bytes: AtomicU64,
    ingress_bytes: AtomicU64,
    copies_h2d: AtomicU64,
    copies_d2h: AtomicU64,
    copies_gds: AtomicU64,
    copy_bytes: AtomicU64,
    queued: AtomicU64,
    queued_max: AtomicU64,
    errors: AtomicU64,
    fallbacks: AtomicU64,
    sticky_fallbacks: AtomicU64,
    bounce_bytes: AtomicU64,
    segments_registered: AtomicU64,
    segments_open: AtomicU64,
    /// `(kind, io path)` pairs already warned about, for e.3 and f.9.
    warned: Mutex<HashSet<(OpKind, &'static str)>>,
    /// File paths already warned about for a sticky fallback (f.9).
    sticky_warned: Mutex<HashSet<PathBuf>>,
    /// Warns actually emitted, so a test can assert "once per run" without a subscriber.
    misaligned_warns: AtomicU64,
    fallback_warns: AtomicU64,
    sticky_warns: AtomicU64,
}

impl Counters {
    pub(crate) fn snapshot(&self) -> ReactorStats {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ReactorStats {
            direct_ops: get(&self.direct_ops),
            buffered_ops: get(&self.buffered_ops),
            uring_ops: get(&self.uring_ops),
            object_reads: get(&self.object_reads),
            object_bytes: get(&self.object_bytes),
            ingress_bytes: get(&self.ingress_bytes),
            copies_h2d: get(&self.copies_h2d),
            copies_d2h: get(&self.copies_d2h),
            copies_gds: get(&self.copies_gds),
            copy_bytes: get(&self.copy_bytes),
            queued_max: get(&self.queued_max),
            errors: get(&self.errors),
            fallbacks: get(&self.fallbacks),
            sticky_fallbacks: get(&self.sticky_fallbacks),
            bounce_bytes: get(&self.bounce_bytes),
            segments_registered: get(&self.segments_registered),
            segments_open: get(&self.segments_open),
        }
    }

    pub(crate) fn note_direct(&self, direct: bool) {
        let slot = if direct {
            &self.direct_ops
        } else {
            &self.buffered_ops
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_uring(&self) {
        self.uring_ops.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_object_read(&self, bytes: u64) {
        self.object_reads.fetch_add(1, Ordering::Relaxed);
        self.object_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn note_ingress(&self, bytes: u64) {
        self.ingress_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn note_copy(&self, kind: CopyDirection, bytes: u64) {
        match kind {
            CopyDirection::HostToDevice => &self.copies_h2d,
            CopyDirection::DeviceToHost => &self.copies_d2h,
            CopyDirection::Gds => &self.copies_gds,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.copy_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn note_bounce(&self, bytes: u64) {
        self.bounce_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn note_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_fallback(&self) {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_sticky_fallback(&self) {
        self.sticky_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn segment_registered(&self) {
        self.segments_registered.fetch_add(1, Ordering::Relaxed);
        self.segments_open.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn segment_unregistered(&self) {
        let _ = self
            .segments_open
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// One operation joined a submission queue (f.8); updates `queued_max`.
    pub(crate) fn enqueued(&self) {
        let now = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self
            .queued_max
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |max| {
                (now > max).then_some(now)
            });
    }

    /// One operation left a submission queue for a reactor thread.
    pub(crate) fn dequeued(&self) {
        let _ = self
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// True the first time this `(kind, path)` pair is seen; the caller warns only then.
    fn first_time(&self, kind: OpKind, path: &'static str) -> bool {
        let mut seen = self.warned.lock().unwrap_or_else(|e| e.into_inner());
        seen.insert((kind, path))
    }

    /// `reactor.misaligned` (e.3): a caller issued an operation direct IO could have taken.
    pub(crate) fn warn_misaligned(&self, kind: OpKind, path: &'static str) {
        if self.first_time(kind, path) {
            self.misaligned_warns.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "reactor.misaligned",
                op = kind.as_str(),
                io_path = path,
                "operation is not page aligned; issued buffered"
            );
        }
    }

    /// `reactor.fallback` (f.9): one operation took the fallback of an available path.
    pub(crate) fn warn_fallback(&self, kind: OpKind, path: &'static str, msg: &str) {
        if self.first_time(kind, path) {
            self.fallback_warns.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "reactor.fallback",
                op = kind.as_str(),
                io_path = path,
                error = msg,
                "operation fell back once"
            );
        }
    }

    /// `reactor.sticky_fallback` (f.9): a file path is buffered for the rest of the run.
    pub(crate) fn warn_sticky(&self, path: &Path) {
        let first = {
            let mut seen = self.sticky_warned.lock().unwrap_or_else(|e| e.into_inner());
            seen.insert(path.to_path_buf())
        };
        if first {
            self.sticky_warns.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "reactor.sticky_fallback",
                path = %path.display(),
                "O_DIRECT refused for this path; every later operation on it is buffered"
            );
        }
    }

    /// Observable for the tests of e.3 and f.9: warns actually emitted.
    #[cfg(test)]
    pub(crate) fn warn_counts(&self) -> WarnCounts {
        WarnCounts {
            misaligned: self.misaligned_warns.load(Ordering::Relaxed),
            fallback: self.fallback_warns.load(Ordering::Relaxed),
            sticky: self.sticky_warns.load(Ordering::Relaxed),
        }
    }
}

/// Which row of the copy table (f.5) a copy took.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum CopyDirection {
    /// Host tier to `Device`.
    HostToDevice,
    /// `Device` to host tier.
    DeviceToHost,
    /// `Disk` to `Device` through cuFile.
    Gds,
}

/// How many times each once-per-run warn actually fired.
#[cfg(test)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WarnCounts {
    pub(crate) misaligned: u64,
    pub(crate) fallback: u64,
    pub(crate) sticky: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_fire_once_per_kind_and_path() {
        let c = Counters::default();
        c.warn_misaligned(OpKind::ReadFile, "direct");
        c.warn_misaligned(OpKind::ReadFile, "direct");
        c.warn_misaligned(OpKind::WriteFile, "direct");
        assert_eq!(c.warn_counts().misaligned, 2);
        c.warn_fallback(OpKind::ReadObject, "object", "boom");
        c.warn_fallback(OpKind::ReadObject, "object", "boom");
        assert_eq!(c.warn_counts().fallback, 1);
        c.warn_sticky(Path::new("/a"));
        c.warn_sticky(Path::new("/a"));
        c.warn_sticky(Path::new("/b"));
        assert_eq!(c.warn_counts().sticky, 2);
    }

    #[test]
    fn queue_depth_tracks_its_high_water_mark() {
        let c = Counters::default();
        c.enqueued();
        c.enqueued();
        c.dequeued();
        c.enqueued();
        assert_eq!(c.snapshot().queued_max, 2);
        c.note_copy(CopyDirection::Gds, 7);
        c.note_direct(true);
        c.note_direct(false);
        c.note_uring();
        c.note_object_read(3);
        c.note_ingress(3);
        c.note_bounce(1);
        c.note_error();
        c.note_fallback();
        c.note_sticky_fallback();
        c.segment_registered();
        c.segment_unregistered();
        c.segment_unregistered();
        let s = c.snapshot();
        assert_eq!((s.copies_gds, s.copy_bytes), (1, 7));
        assert_eq!((s.direct_ops, s.buffered_ops, s.uring_ops), (1, 1, 1));
        assert_eq!((s.object_reads, s.object_bytes, s.ingress_bytes), (1, 3, 3));
        assert_eq!((s.bounce_bytes, s.errors, s.fallbacks), (1, 1, 1));
        assert_eq!((s.sticky_fallbacks, s.segments_registered), (1, 1));
        assert_eq!(s.segments_open, 0);
        assert_eq!(OpKind::Copy.as_str(), "copy");
        assert_eq!(OpKind::ReadFileOpt.as_str(), "read_file_opt");
        assert_eq!(OpKind::WriteObject.as_str(), "write_object");
        assert_eq!(OpKind::DeleteObject.as_str(), "delete_object");
        assert_eq!(OpKind::AbortMultipart.as_str(), "abort_multipart");
    }

    #[test]
    fn copy_directions_land_in_their_own_counters() {
        let c = Counters::default();
        c.note_copy(CopyDirection::HostToDevice, 2);
        c.note_copy(CopyDirection::DeviceToHost, 3);
        let s = c.snapshot();
        assert_eq!((s.copies_h2d, s.copies_d2h, s.copy_bytes), (1, 1, 5));
    }
}
