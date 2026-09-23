//! The lineage index (e.2) and the committed watermark (f.11).
//!
//! One record per morsel the engine has seen that the sink has not yet committed, which is
//! what the manifest is written from (PL-I11). The lineage mutex is preamble 4.2 position
//! 2b: it is taken after a queue lock is released, never inside one, and no queue lock or
//! segments lock is ever taken while it is held.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use moruna_kernel::{Origin, PayloadKind, SegmentRef, Seq, StageId};

use crate::{NO_COMMIT, PlacementEngine};

/// What the engine remembers about one uncommitted morsel (e.2): 64 bytes of bookkeeping,
/// and enough to re-obtain the bytes from the origin if the disk copy is gone (CT-I12).
#[derive(Clone, Debug)]
pub struct Lineage {
    /// Where the rows came from.
    pub origin: Origin,
    /// The stage whose output the morsel currently is.
    pub stage: StageId,
    /// Table or tensor.
    pub kind: PayloadKind,
    /// Payload bytes.
    pub bytes: u64,
    /// The segment record holding a copy, when there is one.
    pub disk: Option<SegmentRef>,
    /// True once the morsel has been popped (it is inside a kernel or the sink).
    pub consumed: bool,
}

impl PlacementEngine {
    /// Take the lineage lock (preamble 4.2 position 2b).
    pub(crate) fn lock_lineage(&self) -> crate::queue::Guarded<'_, BTreeMap<Seq, Lineage>> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::LINEAGE),
            self.lineage.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// A morsel arrived at a queue: create the record on first sight, else advance its
    /// stage and clear the disk copy and the consumed flag (e.2).
    pub(crate) fn lineage_push(
        &self,
        seq: Seq,
        stage: StageId,
        kind: PayloadKind,
        bytes: u64,
        origin: Origin,
    ) {
        let mut lineage = self.lock_lineage();
        match lineage.get_mut(&seq) {
            Some(record) => {
                record.stage = stage;
                record.kind = kind;
                record.bytes = bytes;
                record.disk = None;
                record.consumed = false;
            }
            None => {
                lineage.insert(
                    seq,
                    Lineage {
                        origin,
                        stage,
                        kind,
                        bytes,
                        disk: None,
                        consumed: false,
                    },
                );
            }
        }
    }

    /// A demotion wrote a record, or a promotion kept one (f.7): the disk copy stays valid
    /// until the entry is consumed.
    pub(crate) fn lineage_set_disk(&self, seq: Seq, disk: Option<SegmentRef>) {
        let mut lineage = self.lock_lineage();
        if let Some(record) = lineage.get_mut(&seq) {
            record.disk = disk;
        }
    }

    /// A consumer popped the morsel; its lineage stays until the watermark passes it
    /// (PL-I11).
    pub(crate) fn lineage_consumed(&self, seq: Seq) {
        let mut lineage = self.lock_lineage();
        if let Some(record) = lineage.get_mut(&seq) {
            record.consumed = true;
            record.disk = None;
        }
    }

    /// `set_committed` (f.11). The lineage lock is held only to collect; the segments lock
    /// is taken after it has been dropped (preamble 4.2).
    pub(crate) fn commit_to(&self, seq: Seq) {
        let previous = self.committed.load(Ordering::Acquire);
        if previous != NO_COMMIT && seq < previous {
            debug_assert!(
                false,
                "set_committed({seq}) is below the watermark {previous}; it is monotonic"
            );
            return;
        }
        self.committed.store(seq, Ordering::Release);
        let released: Vec<SegmentRef> = {
            let mut lineage = self.lock_lineage();
            let above = lineage.split_off(&seq.saturating_add(1));
            let below = std::mem::replace(&mut *lineage, above);
            below
                .into_values()
                .filter_map(|record| record.disk)
                .collect()
        };
        for segment in released {
            self.release_record(segment.segment);
        }
    }

    /// The lineage index as the manifest writes it: ascending `seq` (e.5).
    pub(crate) fn lineage_snapshot(&self) -> Vec<(Seq, Lineage)> {
        let lineage = self.lock_lineage();
        lineage
            .iter()
            .map(|(seq, record)| (*seq, record.clone()))
            .collect()
    }

    /// How many uncommitted morsels the engine is tracking (j).
    pub(crate) fn lineage_len(&self) -> u64 {
        self.lock_lineage().len() as u64
    }
}
