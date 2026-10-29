//! The lineage index (e.2) and the commit watermarks (f.11).
//!
//! One record per morsel a source read that some sink has not yet committed, which is what the
//! manifest is written from (PL-I11). A record is keyed by its root, the source and the number
//! the source read it as, and holds the morsel's positions: every queue a morsel derived from it
//! is in, or was taken from by a consumer that has not yet produced from it (d.16). In a linear
//! chain a record has one position, which moves from queue to queue as the morsel does.
//!
//! The lineage mutex is preamble 4.2 position 2b: it is taken after a queue lock is released,
//! never inside one, and no queue lock or segments lock is ever taken while it is held.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use moruna_kernel::{Origin, PayloadKind, SegmentRef, Seq, StageId};

use crate::{NO_COMMIT, PlacementEngine};

/// Where a lineage record is kept: the source and the number it read the morsel as.
pub type Root = (u16, Seq);

/// One place a morsel derived from a root is (e.2): a queue and the number it has there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    /// The queue.
    pub stage: StageId,
    /// The morsel's number on that queue.
    pub seq: Seq,
    /// Table or tensor.
    pub kind: PayloadKind,
    /// Payload bytes.
    pub bytes: u64,
    /// The segment record holding a copy, when there is one.
    pub disk: Option<SegmentRef>,
    /// True once the morsel has been popped (it is inside a kernel or a sink).
    pub consumed: bool,
}

/// What the engine remembers about one source morsel some sink has not committed (e.2): enough
/// to re-obtain every morsel derived from it from the origin if no disk copy holds them
/// (CT-I12).
#[derive(Clone, Debug)]
pub struct Lineage {
    /// Where the rows came from.
    pub origin: Origin,
    /// Where the morsels derived from it are.
    pub positions: Vec<Position>,
    /// True once every sink it reaches has committed what it became there while checkpointing
    /// is on. The record is kept, without a disk copy, until a manifest whose watermarks cover
    /// it has been written: the scheduler reads the watermarks before the sinks' state and the
    /// lineage after it, so a morsel committed in between belongs to neither the watermarks
    /// nor, without this, the lineage, and a resume would lose it (MH 4.7, H5).
    pub committed: bool,
}

impl PlacementEngine {
    /// Take the lineage lock (preamble 4.2 position 2b).
    pub(crate) fn lock_lineage(&self) -> crate::queue::Guarded<'_, BTreeMap<Root, Lineage>> {
        crate::queue::Guarded::new(
            crate::locks::Held::enter(crate::locks::LINEAGE),
            self.lineage.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// The root of the morsel `seq` on queue `stage`.
    pub(crate) fn root_of(&self, stage: StageId, seq: Seq) -> Root {
        self.cfg.topology.root(stage, seq).unwrap_or((0, seq))
    }

    /// A morsel arrived at the queues `stages` (one for a push, every queue of a fan-out for a
    /// `push_to`): in one step, the position its producer took it from is gone and a position
    /// at each queue is recorded, so a manifest sees the morsel before or after the move and
    /// never half way (e.2, d.16).
    pub(crate) fn lineage_push(
        &self,
        stages: &[StageId],
        seq: Seq,
        kind: PayloadKind,
        bytes: u64,
        origin: Origin,
    ) {
        let Some(&first) = stages.first() else {
            return;
        };
        let root = self.root_of(first, seq);
        let parent = self.cfg.topology.parent(first, seq);
        let mut lineage = self.lock_lineage();
        let record = lineage.entry(root).or_insert_with(|| Lineage {
            origin,
            positions: Vec::new(),
            committed: false,
        });
        if let Some((stage, at)) = parent {
            record
                .positions
                .retain(|p| !(p.stage == stage && p.seq == at));
        }
        for &stage in stages {
            match record
                .positions
                .iter_mut()
                .find(|p| p.stage == stage && p.seq == seq)
            {
                Some(position) => {
                    position.kind = kind;
                    position.bytes = bytes;
                    position.disk = None;
                    position.consumed = false;
                }
                None => record.positions.push(Position {
                    stage,
                    seq,
                    kind,
                    bytes,
                    disk: None,
                    consumed: false,
                }),
            }
        }
    }

    /// The position of `seq` on `stage`, to change it.
    fn with_position(&self, stage: StageId, seq: Seq, change: impl FnOnce(&mut Position)) {
        let root = self.root_of(stage, seq);
        let mut lineage = self.lock_lineage();
        if let Some(position) = lineage.get_mut(&root).and_then(|record| {
            record
                .positions
                .iter_mut()
                .find(|p| p.stage == stage && p.seq == seq)
        }) {
            change(position);
        }
    }

    /// A demotion wrote a record, or a promotion kept one (f.7): the disk copy stays valid
    /// until the entry is consumed.
    pub(crate) fn lineage_set_disk(&self, stage: StageId, seq: Seq, disk: Option<SegmentRef>) {
        self.with_position(stage, seq, |position| position.disk = disk);
    }

    /// A consumer popped the morsel; its position stays until what the consumer makes of it is
    /// pushed, or every sink has committed it (PL-I11).
    pub(crate) fn lineage_consumed(&self, stage: StageId, seq: Seq) {
        self.with_position(stage, seq, |position| {
            position.consumed = true;
            position.disk = None;
        });
    }

    /// The watermark of sink `sink`, or `None` when it has committed nothing (e.2).
    pub(crate) fn sink_committed(&self, sink: u16) -> Option<Seq> {
        match self
            .sink_watermarks
            .get(usize::from(sink))
            .map(|w| w.load(Ordering::Acquire))
        {
            None | Some(NO_COMMIT) => None,
            Some(seq) => Some(seq),
        }
    }

    /// Every sink's watermark.
    pub(crate) fn sink_watermarks(&self) -> Vec<Option<Seq>> {
        (0..self.cfg.topology.sinks())
            .map(|sink| self.sink_committed(sink))
            .collect()
    }

    /// The highest number source `source` read such that every sink it reaches has committed
    /// what every morsel at or below it became there, given the sinks' watermarks `sinks`;
    /// `None` when there is no such number (f.11, d.16). For a chain it is the sink's watermark.
    pub(crate) fn source_watermark(&self, source: u16, sinks: &[Option<Seq>]) -> Option<Seq> {
        let mut covered: Option<Seq> = None;
        let mut reached = false;
        for sink in 0..self.cfg.topology.sinks() {
            for (from, map) in self.cfg.topology.paths(sink) {
                if *from != source {
                    continue;
                }
                reached = true;
                let below = sinks
                    .get(usize::from(sink))
                    .copied()
                    .flatten()
                    .and_then(|watermark| map.floor_inverse(watermark))?;
                covered = Some(covered.map_or(below, |c| c.min(below)));
            }
        }
        if reached { covered } else { None }
    }

    /// `set_committed` (f.11): sink `sink` has committed every morsel at or below `seq`. Each
    /// source's records every sink has committed are released. The lineage lock is held only to
    /// collect; the segments lock is taken after it has been dropped (preamble 4.2).
    pub(crate) fn commit_to(&self, sink: u16, seq: Seq) {
        let Some(cell) = self.sink_watermarks.get(usize::from(sink)) else {
            return;
        };
        let previous = cell.load(Ordering::Acquire);
        if previous != NO_COMMIT && seq < previous {
            debug_assert!(
                false,
                "set_committed({sink}, {seq}) is below the watermark {previous}; it is monotonic"
            );
            return;
        }
        cell.store(seq, Ordering::Release);
        let sinks = self.sink_watermarks();
        let mut released: Vec<SegmentRef> = Vec::new();
        for source in 0..self.cfg.topology.sources() {
            let Some(upto) = self.source_watermark(source, &sinks) else {
                continue;
            };
            if let Some(cell) = self.source_watermarks.get(usize::from(source)) {
                cell.store(upto, Ordering::Release);
            }
            let range = (source, 0)..=(source, upto);
            let mut lineage = self.lock_lineage();
            if self.cfg.checkpoint_enabled {
                // Kept, marked, until the next manifest prunes it (`lineage_snapshot`).
                for record in lineage.range_mut(range).map(|(_, r)| r) {
                    if record.committed {
                        continue;
                    }
                    record.committed = true;
                    for position in &mut record.positions {
                        position.consumed = true;
                        released.extend(position.disk.take());
                    }
                }
            } else {
                let keys: Vec<Root> = lineage.range(range).map(|(k, _)| *k).collect();
                for key in keys {
                    if let Some(record) = lineage.remove(&key) {
                        released.extend(record.positions.into_iter().filter_map(|p| p.disk));
                    }
                }
            }
        }
        for segment in released {
            self.release_record(segment.segment);
        }
    }

    /// Each source's watermark as the last commit computed it.
    pub(crate) fn source_watermarks(&self) -> Vec<Option<Seq>> {
        self.source_watermarks
            .iter()
            .map(|w| match w.load(Ordering::Acquire) {
                NO_COMMIT => None,
                seq => Some(seq),
            })
            .collect()
    }

    /// The lineage index as the manifest writes it: ascending root, every record above its
    /// source's watermark in `above` whether or not the sinks have committed it since (e.5, MH
    /// 4.7). Committed records a watermark covers are dropped here, under the same lock, so a
    /// record leaves the index only once a manifest no longer needs it.
    pub(crate) fn lineage_snapshot(&self, above: &[Option<Seq>]) -> Vec<(Root, Lineage)> {
        let mut lineage = self.lock_lineage();
        let covered = |root: &Root| {
            above
                .get(usize::from(root.0))
                .copied()
                .flatten()
                .is_some_and(|watermark| root.1 <= watermark)
        };
        lineage.retain(|root, record| !(record.committed && covered(root)));
        lineage
            .iter()
            .filter(|(root, _)| !covered(root))
            .map(|(root, record)| (*root, record.clone()))
            .collect()
    }

    /// How many source morsels some sink has not committed (j).
    pub(crate) fn lineage_len(&self) -> u64 {
        self.lock_lineage()
            .values()
            .filter(|record| !record.committed)
            .count() as u64
    }
}
