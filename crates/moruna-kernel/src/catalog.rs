//! The buffer catalog's port (contracts d.17; placement f.18; adaptive-runtime 9.1, 9.2).
//!
//! Every placement keeps a catalog of the batches it holds, and the scheduler and its policy
//! (d.18) read and drive it through [`Residency`], which [`crate::Placement`] requires. The port
//! states the catalog's rules: no batch a running unit holds leaves memory, a batch with no
//! consumer left is freed and never spilled, and no claim is granted past the budget.

use std::sync::{Arc, Weak};

use crate::ids::{Seq, StageId};
use crate::morsel::Morsel;
use crate::placement::CatalogStats;
use crate::tier::{Tier, TierKind};

/// A batch's name in the catalog, unique for the placement's life.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BufferId(pub u64);

/// Where a batch's bytes are, as a policy reads it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// In one memory tier, with no disk copy.
    Memory(Tier),
    /// In one memory tier, with a disk copy too.
    MemoryAndDisk(Tier),
    /// Only on disk.
    Disk,
    /// A move between two tiers is in flight; the bytes are where it started until it lands.
    Moving {
        /// Where the bytes are.
        from: TierKind,
        /// Where they are going.
        to: TierKind,
    },
    /// Nowhere: a source's batch dropped to be read again.
    Dropped,
    /// Reserved (`rdma`): in another node's memory.
    Remote,
}

/// What the queues say about one batch's future (placement f.18).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Future {
    /// Takes before its first remaining consumer reaches it: the smallest position, head 0, of
    /// its entries across the queues that hold it.
    pub next_use: u64,
    /// One of its entries is inside its queue's promotion window.
    pub hot: bool,
    /// Every queue holding it has staging on.
    pub all_staging: bool,
    /// Some queue holding it has staging on.
    pub any_staging: bool,
}

/// One batch as the catalog records it (adaptive-runtime 9.1): its size, location, owner,
/// remaining consumers and reload cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchView {
    /// The batch.
    pub id: BufferId,
    /// Its size.
    pub bytes: u64,
    /// Where its bytes are.
    pub location: Location,
    /// What produced it.
    pub owner: crate::graph::Producer,
    /// The consumers still to take it, `(queue, seq)`.
    pub consumers: Vec<(StageId, Seq)>,
    /// The consumers holding it now.
    pub holders: Vec<(StageId, Seq)>,
    /// The disk bytes bringing it back moves once it has left memory.
    pub reload_cost: u64,
    /// What the queues say about its future; `None` while an entry is not yet placed.
    pub future: Option<Future>,
}

/// The catalog as a policy reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogSnapshot {
    /// Every batch, in id order.
    pub batches: Vec<BatchView>,
    /// The figures.
    pub stats: CatalogStats,
}

/// The batch at the head of one queue, what admission reads (adaptive-runtime 9.4): cheap, one
/// queue lock and the catalog's.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// The batch.
    pub id: BufferId,
    /// Its number on the queue.
    pub seq: Seq,
    /// Its size.
    pub bytes: u64,
    /// The take from this queue is its last: its bytes leave when the taker releases them.
    pub last: bool,
    /// When it arrived, in the catalog's ticks: the order batches came in.
    pub arrived: u64,
}

/// What the catalog emits, in the order things happened. `Released` and `Freed` are the data
/// plane's events of the same names (SDD 29).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogEvent {
    /// The consumer of `stage` released the share of `batch` it took as `seq`.
    Released {
        /// The batch.
        batch: BufferId,
        /// The queue the consumer took it from.
        stage: StageId,
        /// Its number there.
        seq: Seq,
    },
    /// Every consumer took and released `batch`; its `bytes` have left memory.
    Freed {
        /// The batch.
        batch: BufferId,
        /// Its size.
        bytes: u64,
    },
    /// A record of `batch` landed on disk and its resident copy was dropped.
    Spilled {
        /// The batch.
        batch: BufferId,
        /// Its size.
        bytes: u64,
    },
    /// `batch` was read back from disk into memory.
    Reloaded {
        /// The batch.
        batch: BufferId,
        /// Its size.
        bytes: u64,
    },
    /// `batch` left memory without IO.
    Dropped {
        /// The batch.
        batch: BufferId,
        /// Its size.
        bytes: u64,
    },
}

/// Who hears the catalog's events, on the thread that caused them, with no lock held.
pub trait CatalogObserver: Send + Sync {
    /// One event.
    fn event(&self, event: &CatalogEvent);
}

/// What holds the claims it grants, so a claim dropped unused goes back to it.
pub trait ClaimHolder: Send + Sync {
    /// `bytes` of tier slot `slot` claimed and not used.
    fn give_back(&self, slot: usize, bytes: u64);
}

/// A claim on a tier's budget. Dropped unused, it gives the bytes back to its holder.
pub struct Reservation {
    slot: usize,
    bytes: u64,
    holder: Weak<dyn ClaimHolder>,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("slot", &self.slot)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Reservation {
    /// A claim of `bytes` in tier slot `slot`, granted by `holder`: for a placement to make.
    pub fn new(slot: usize, bytes: u64, holder: Weak<dyn ClaimHolder>) -> Reservation {
        Reservation {
            slot,
            bytes,
            holder,
        }
    }

    /// The bytes claimed.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The tier slot claimed in.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Hand the claim to the push that uses it: the placement that granted it takes the bytes
    /// over, and nothing is given back when the handle goes.
    pub fn into_claim(mut self) -> (usize, u64) {
        let claim = (self.slot, self.bytes);
        self.bytes = 0;
        claim
    }

    /// Join `other`'s bytes to this claim: a top-up granted in the same slot. Refused, giving
    /// `other` back, when the slots differ.
    pub fn join(&mut self, other: Reservation) -> crate::Result<()> {
        if other.slot != self.slot {
            return Err(crate::MorunaError::Staging(format!(
                "a claim in tier slot {} cannot join one in slot {}",
                other.slot, self.slot
            )));
        }
        let (_, bytes) = other.into_claim();
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        if let Some(holder) = self.holder.upgrade() {
            holder.give_back(self.slot, self.bytes);
        }
    }
}

/// A queue's promotion window until a knob changes it: its head and the entry behind it are
/// kept in memory for its consumer (placement b).
pub const DEFAULT_PROMOTION_WINDOW: u16 = 2;

/// The least a tier's budget must hold for a run to make progress (placement f.18 rule 13,
/// adaptive-runtime 12.5): every queue's promotion window, and one unit's input and output, at
/// the smallest batch the run can be made to use. A budget under it is refused before the run
/// starts; above it, a head on disk can always come back.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CatalogFloor {
    /// Queues.
    pub queues: u64,
    /// Their promotion windows, summed: the batches kept in memory for consumers about to take
    /// them.
    pub windows: u64,
    /// The batch the floor is counted in.
    pub batch: u64,
    /// The floor: `(windows + 2) × batch`.
    pub bytes: u64,
    /// The budget it is set against.
    pub budget: u64,
}

impl CatalogFloor {
    /// The floor of `queues` queues whose windows sum to `windows`, at `batch` bytes a batch.
    pub fn of(queues: u64, windows: u64, batch: u64, budget: u64) -> CatalogFloor {
        CatalogFloor {
            queues,
            windows,
            batch,
            bytes: windows.saturating_add(2).saturating_mul(batch),
            budget,
        }
    }

    /// True when the budget holds the floor.
    pub fn holds(&self) -> bool {
        self.budget >= self.bytes
    }

    /// What it is made of, in a sentence a refusal can carry.
    pub fn terms(&self) -> String {
        format!(
            "{} bytes: the promotion windows of its {} queues, {} batches, and one unit's input \
             and output, 2 batches, at {} bytes a batch",
            self.bytes, self.queues, self.windows, self.batch
        )
    }
}

/// What a reservation came to.
#[derive(Debug)]
pub enum Reserve {
    /// The bytes are claimed; they become resident when the batch is pushed with
    /// [`Residency::push_reserved`], and are released if the claim is dropped.
    Granted(Reservation),
    /// Not now: room is on its way (a shed found something, or demotions in flight cover it).
    Pending {
        /// The bytes that do not fit yet.
        need: u64,
    },
    /// Not while memory holds what it holds: nothing can leave the tier now. A claim larger
    /// than the tier's budget is refused for ever.
    Refused {
        /// The bytes that do not fit.
        need: u64,
        /// The tier's budget.
        budget: u64,
    },
}

/// The catalog's port (contracts d.17): its state and its actions, independent of any one
/// policy. Every action keeps the catalog's rules (placement PL-I19, PL-I22).
pub trait Residency: Send + Sync {
    /// Every batch and the figures.
    fn snapshot(&self) -> CatalogSnapshot;
    /// The batch at the head of queue `stage`, when there is one.
    fn head(&self, stage: StageId) -> Option<Head>;
    /// Claim `bytes` of `tier` for a batch about to be made (an admission).
    fn reserve(&self, tier: Tier, bytes: u64) -> crate::Result<Reserve>;
    /// Push a batch whose bytes were claimed: the claim becomes its residency in one step. A
    /// batch larger than its claim is pushed and the excess counted as an overrun.
    fn push_reserved(
        &self,
        stages: &[StageId],
        morsel: Morsel,
        claim: Reservation,
    ) -> crate::Result<()>;
    /// `replace` of an evicted batch under a claim, as `push_reserved`.
    fn replace_reserved(
        &self,
        stage: StageId,
        morsel: Morsel,
        claim: Reservation,
    ) -> crate::Result<()>;
    /// Push a batch that memory cannot admit straight to disk: its bytes never count as
    /// resident, and its consumers read it back under a claim (adaptive-runtime 12.5, "the run
    /// spills"). `Staging` when the run has no disk tier.
    fn push_spilled(&self, stages: &[StageId], morsel: Morsel) -> crate::Result<()>;
    /// Free at least `bytes` of `tier` by the placement's eviction order; true when anything
    /// left or is on its way out.
    fn shed(&self, tier: TierKind, bytes: u64) -> bool;
    /// Write `batch` to disk and drop its resident copy when the write lands.
    fn spill(&self, batch: BufferId) -> crate::Result<()>;
    /// Read `batch` back from disk into the host tier.
    fn reload(&self, batch: BufferId) -> crate::Result<()>;
    /// Hear every event from now on; a second observer replaces the first.
    fn observe(&self, observer: Arc<dyn CatalogObserver>);
    /// The run's floor in `tier` at `batch` bytes a batch, against the tier's budget now.
    fn floor(&self, tier: Tier, batch: u64) -> CatalogFloor;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Holder(Mutex<Vec<(usize, u64)>>);

    impl ClaimHolder for Holder {
        fn give_back(&self, slot: usize, bytes: u64) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((slot, bytes));
        }
    }

    fn claim(holder: &Arc<Holder>, slot: usize, bytes: u64) -> Reservation {
        let weak: Weak<dyn ClaimHolder> = Arc::downgrade(holder) as Weak<Holder>;
        Reservation::new(slot, bytes, weak)
    }

    /// CT-T29 a claim goes back once: dropped unused it gives its bytes back; taken over by a
    /// push, joined to another, or outliving its holder, it gives nothing back.
    #[test]
    fn ct_t29_a_claim_goes_back_once() {
        let holder = Arc::new(Holder::default());
        let given = |h: &Arc<Holder>| h.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        drop(claim(&holder, 2, 10));
        assert_eq!(given(&holder), vec![(2, 10)]);
        let used = claim(&holder, 2, 7);
        assert_eq!((used.bytes(), used.slot()), (7, 2));
        assert_eq!(used.into_claim(), (2, 7));
        assert_eq!(given(&holder), vec![(2, 10)], "taken over, nothing back");
        let mut first = claim(&holder, 2, 3);
        first.join(claim(&holder, 2, 4)).expect("one slot");
        assert_eq!(first.bytes(), 7);
        let refused = first.join(claim(&holder, 0, 5));
        assert!(refused.is_err(), "slots differ");
        assert_eq!(
            given(&holder),
            vec![(2, 10), (0, 5)],
            "the refused one went back"
        );
        assert!(format!("{first:?}").contains("bytes: 7"));
        drop(first);
        assert_eq!(given(&holder).last(), Some(&(2, 7)));
        let orphan = claim(&holder, 1, 9);
        drop(holder);
        drop(orphan);
    }
}
