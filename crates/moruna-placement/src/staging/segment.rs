//! Segment files: the record header of e.3, the global segment counter, the segments map,
//! and the lifecycle of f.7 (release, reclaim, unlink).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use moruna_kernel::{MorunaError, PayloadKind, Seq, StageId, StagingCodec};

use crate::{PlacementEngine, kind_code, kind_of_code};

/// The magic at offset 0 of every record header page (e.3).
pub const MAGIC: [u8; 8] = *b"MORUNSEG";

/// The fixed part of a record header page (e.3); the rest of the page is zero padding.
pub const HEADER_FIXED: usize = 64;

/// One staging file.
#[derive(Clone, Debug)]
pub struct Segment {
    /// The stage whose records this segment holds (e.3: one stage per segment).
    pub stage: StageId,
    /// Where the file is.
    pub path: PathBuf,
    /// Bytes of the file the engine has laid records into, including abandoned pages of a
    /// failed write (f.10), so the file stays append-only.
    pub bytes_written: u64,
    /// Records laid into the segment.
    pub records_total: u64,
    /// Records whose entry is `Consumed` or whose lineage the watermark dropped (f.7).
    pub records_released: u64,
    /// True while this is the segment its queue appends to.
    pub active: bool,
}

impl Segment {
    /// True when every record is released and the segment is no longer appended to (f.7).
    pub fn reclaimable(&self) -> bool {
        !self.active && self.records_released >= self.records_total
    }
}

/// The segments map and the active segment per queue, behind one mutex (preamble 4.2
/// position 3).
#[derive(Default)]
pub struct Staging {
    /// Every segment the engine knows about, by its global number.
    pub segments: BTreeMap<u32, Segment>,
    /// The segment each queue appends to.
    pub active: HashMap<StageId, u32>,
}

/// One record's header, as e.3 lays it out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    /// The morsel's sequence number.
    pub seq: Seq,
    /// The stage whose output it is.
    pub stage: StageId,
    /// Table or tensor.
    pub kind: PayloadKind,
    /// How the body is encoded; `Raw` in v1.
    pub codec: StagingCodec,
    /// Bytes of the body, from `body_offset` to the end of its last piece.
    pub payload_len: u64,
    /// Absolute offset of the body in the file; one page after the header's offset.
    pub body_offset: u64,
}

impl RecordHeader {
    /// Write the header into `out`, which must be one whole page; the rest is zeroed.
    pub fn write(&self, out: &mut [u8]) -> moruna_kernel::Result<()> {
        if out.len() < HEADER_FIXED {
            return Err(MorunaError::Staging(format!(
                "a record header needs {HEADER_FIXED} bytes, got {}",
                out.len()
            )));
        }
        out.fill(0);
        out[0..8].copy_from_slice(&MAGIC);
        out[8..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..18].copy_from_slice(&self.stage.to_le_bytes());
        out[18] = kind_code(self.kind);
        out[19] = self.codec.code();
        out[24..32].copy_from_slice(&self.payload_len.to_le_bytes());
        out[32..40].copy_from_slice(&self.body_offset.to_le_bytes());
        Ok(())
    }

    /// Parse a header page. An unknown codec byte is `Unsupported("codec")` (PL-T19).
    pub fn read(buf: &[u8]) -> moruna_kernel::Result<RecordHeader> {
        if buf.len() < HEADER_FIXED {
            return Err(MorunaError::Staging(format!(
                "a record header needs {HEADER_FIXED} bytes, got {}",
                buf.len()
            )));
        }
        if buf[0..8] != MAGIC {
            return Err(MorunaError::Staging("record magic is not MORUNSEG".into()));
        }
        let Some(kind) = kind_of_code(buf[18]) else {
            return Err(MorunaError::Staging(format!(
                "unknown payload kind code {}",
                buf[18]
            )));
        };
        let Some(codec) = StagingCodec::from_code(buf[19]) else {
            return Err(MorunaError::Unsupported("codec"));
        };
        let mut eight = [0u8; 8];
        eight.copy_from_slice(&buf[8..16]);
        let seq = u64::from_le_bytes(eight);
        let stage = u16::from_le_bytes([buf[16], buf[17]]);
        eight.copy_from_slice(&buf[24..32]);
        let payload_len = u64::from_le_bytes(eight);
        eight.copy_from_slice(&buf[32..40]);
        let body_offset = u64::from_le_bytes(eight);
        Ok(RecordHeader {
            seq,
            stage,
            kind,
            codec,
            payload_len,
            body_offset,
        })
    }
}

/// Round `value` up to the next multiple of `to`.
pub fn round_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

impl PlacementEngine {
    /// Take the segments lock (preamble 4.2 position 3).
    pub(crate) fn lock_staging(&self) -> crate::queue::Guarded<'_, Staging> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::SEGMENTS),
            self.staging.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// `staging_dir/moruna-<run_id>/seg-<segment:06>.seg` (e.3).
    pub(crate) fn segment_path(&self, segment: u32) -> Option<PathBuf> {
        self.run_dir
            .as_ref()
            .map(|dir| dir.join(format!("seg-{segment:06}.seg")))
    }

    /// Find room for a record of `need` bytes in the queue's active segment, rolling to a
    /// new one when it does not fit (f.5). Returns the segment, its path and the
    /// page-aligned offset the record starts at, and charges the record's span to the
    /// segment at once so a concurrent demotion cannot be given the same bytes.
    ///
    /// The engine's own roll lock is held across `register_segment`, which is a reactor
    /// call that resolves nothing and cannot call back into placement, so it is not a lock
    /// held across a completion (g).
    pub(crate) fn reserve_record(
        &self,
        stage: StageId,
        need: u64,
    ) -> moruna_kernel::Result<(u32, PathBuf, u64)> {
        if self.run_dir.is_none() {
            return Err(MorunaError::Staging(
                "demotion to disk without a staging directory".into(),
            ));
        }
        loop {
            {
                let mut staging = self.lock_staging();
                if let Some(number) = staging.active.get(&stage).copied()
                    && let Some(segment) = staging.segments.get_mut(&number)
                    && segment.bytes_written + need <= self.cfg.segment_bytes
                {
                    let start = segment.bytes_written;
                    segment.bytes_written += need;
                    let path = segment.path.clone();
                    return Ok((number, path, start));
                }
            }
            if need > self.cfg.segment_bytes {
                return Err(MorunaError::Staging(format!(
                    "a record of {need} bytes does not fit a segment of {} bytes",
                    self.cfg.segment_bytes
                )));
            }
            self.open_segment(stage, need)?;
        }
    }

    /// Open a segment for `stage` and make it the active one (f.5, PL-I7): the disk charge
    /// is the whole segment, because the file is created at its full size.
    fn open_segment(&self, stage: StageId, need: u64) -> moruna_kernel::Result<u32> {
        let _roll = self.roll_lock();
        {
            // Another thread may have opened one while this one waited for the roll lock.
            let staging = self.lock_staging();
            if let Some(number) = staging.active.get(&stage).copied()
                && let Some(segment) = staging.segments.get(&number)
                && segment.bytes_written + need <= self.cfg.segment_bytes
            {
                return Ok(number);
            }
        }
        let budget = self.disk_budget.load(Ordering::Acquire);
        let held = self.disk_bytes.load(Ordering::Acquire);
        if held.saturating_add(self.cfg.segment_bytes) > budget {
            tracing::error!(
                target: "placement.disk_bound",
                stage,
                disk_bytes = held,
                disk_budget = budget,
                segment_bytes = self.cfg.segment_bytes
            );
            return Err(MorunaError::Staging(format!(
                "staging is at its disk bound: {held} bytes held, budget {budget}, a segment is {} bytes",
                self.cfg.segment_bytes
            )));
        }
        let number = self.next_segment.fetch_add(1, Ordering::AcqRel);
        let Some(path) = self.segment_path(number) else {
            return Err(MorunaError::Staging(
                "demotion to disk without a staging directory".into(),
            ));
        };
        // The one file operation besides the manifest that does not go through the reactor,
        // because it moves no bytes (e.3). `std::fs` has no `fallocate`; `set_len` is the
        // portable way to create the file at its full size, and the engine, not the
        // filesystem, is what PL-I7 bounds.
        let file = std::fs::File::create(&path).map_err(|e| MorunaError::Io {
            op: "create",
            target: path.display().to_string(),
            msg: e.to_string(),
        })?;
        if let Err(e) = file.set_len(self.cfg.segment_bytes) {
            let _ = std::fs::remove_file(&path);
            // The filesystem could not give the space the budget said was there (h): lower
            // the effective budget to what is actually held and report the failure.
            self.disk_budget
                .store(self.disk_bytes.load(Ordering::Acquire), Ordering::Release);
            return Err(MorunaError::Io {
                op: "set_len",
                target: path.display().to_string(),
                msg: e.to_string(),
            });
        }
        drop(file);
        self.disk_bytes
            .fetch_add(self.cfg.segment_bytes, Ordering::AcqRel);
        self.reactor.register_segment(number, &path)?;
        {
            let mut staging = self.lock_staging();
            if let Some(previous) = staging.active.insert(stage, number)
                && let Some(segment) = staging.segments.get_mut(&previous)
            {
                segment.active = false;
            }
            staging.segments.insert(
                number,
                Segment {
                    stage,
                    path: path.clone(),
                    bytes_written: 0,
                    records_total: 0,
                    records_released: 0,
                    active: true,
                },
            );
        }
        tracing::info!(target: "placement.segment_roll", stage, segment = number, bytes = self.cfg.segment_bytes);
        Ok(number)
    }

    /// One record landed in a segment (f.5).
    pub(crate) fn record_written(&self, segment: u32) {
        let mut staging = self.lock_staging();
        if let Some(entry) = staging.segments.get_mut(&segment) {
            entry.records_total += 1;
        }
    }

    /// A record's entry was consumed, or the watermark dropped its lineage (f.7). Unlinks
    /// the segment at once when the run keeps no manifest.
    pub(crate) fn release_record(&self, segment: u32) {
        let reclaimable = {
            let mut staging = self.lock_staging();
            match staging.segments.get_mut(&segment) {
                Some(entry) => {
                    entry.records_released += 1;
                    entry.reclaimable()
                }
                None => false,
            }
        };
        if reclaimable && !self.cfg.checkpoint_enabled {
            self.unlink_segments(&[segment]);
        }
    }

    /// The queue will take no more records: its segment stops being active, so it can be
    /// reclaimed when its last record is released (f.7).
    pub(crate) fn retire_active_segment(&self, stage: StageId) {
        let retired = {
            let mut staging = self.lock_staging();
            match staging.active.remove(&stage) {
                Some(number) => {
                    if let Some(segment) = staging.segments.get_mut(&number) {
                        segment.active = false;
                        if segment.reclaimable() {
                            Some(number)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                None => None,
            }
        };
        if let Some(number) = retired
            && !self.cfg.checkpoint_enabled
        {
            self.unlink_segments(&[number]);
        }
    }

    /// Every reclaimable segment the current manifest does not reference (f.7, PL-I12).
    pub(crate) fn reclaimable_segments(&self, referenced: &HashSet<u32>) -> Vec<u32> {
        let staging = self.lock_staging();
        staging
            .segments
            .iter()
            .filter(|(number, segment)| segment.reclaimable() && !referenced.contains(number))
            .map(|(number, _)| *number)
            .collect()
    }

    /// Unlink segments: `unregister_segment` first, so the reactor's descriptor is closed
    /// and the space is really released, then the file, then the disk charge (PL-I15, f.7).
    pub(crate) fn unlink_segments(&self, numbers: &[u32]) {
        for number in numbers {
            let path = {
                let mut staging = self.lock_staging();
                match staging.segments.get(number) {
                    Some(segment) if segment.reclaimable() => {
                        let path = segment.path.clone();
                        staging.segments.remove(number);
                        Some(path)
                    }
                    Some(_) | None => None,
                }
            };
            let Some(path) = path else { continue };
            self.reactor.unregister_segment(*number);
            let _ = std::fs::remove_file(&path);
            self.disk_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(self.cfg.segment_bytes))
                })
                .ok();
        }
    }

    /// The engine's roll lock, which serialises opening a segment so two demotions never
    /// create two files for one queue.
    fn roll_lock(&self) -> crate::queue::Guarded<'_, ()> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::ROLL),
            self.roll.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }
}
