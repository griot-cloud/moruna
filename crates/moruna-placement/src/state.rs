//! The state machine of a batch's bytes (09 e.1), the payload the catalog holds for a batch, and
//! the queue entry that names a batch (e.2, f.18).

use std::sync::Arc;

use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    ManagedTensor, Morsel, MorselFeatures, NodeId, Origin, Payload, PayloadKind, PayloadSpec,
    RemoteRef, SegmentRef, Seq, StageId, Tier, TierPref,
};

use crate::catalog::BufferId;

/// The number of states an entry can be in (e.1), which is the width of the
/// `entries_by_state` histogram of section j.
pub const STATE_COUNT: usize = 8;

/// Where an entry's bytes are, and which move is in flight for it (e.1).
///
/// `OnDisk` is `Resident(Tier::Disk(_))` in the contract's terms; the engine keeps them
/// distinct because an `OnDisk` entry has no arena buffer, and keeps `ResidentOnDisk`
/// distinct from `Resident` because the second demotion of such an entry costs no IO.
#[derive(Clone, Debug)]
pub enum State {
    /// Bytes in one resident tier, no disk copy.
    Resident(Tier),
    /// Bytes resident and a valid segment record ("Resident + OnDisk" in e.1).
    ResidentOnDisk(Tier),
    /// A promotion toward the target tier is in flight.
    Promoting(Tier, Tier),
    /// A demotion one tier down is in flight.
    Demoting(Tier, Tier),
    /// No arena buffer; the bytes are a segment record.
    OnDisk(SegmentRef),
    /// No bytes anywhere; recomputable, awaiting `replace`.
    Evicted,
    /// Popped by the consumer; terminal for the queue, not for the lineage index.
    Consumed,
    /// Reserved (feature `rdma`, not v1): the counterpart of `OnDisk` for bytes in
    /// another node's registered memory. Never constructed in a v1 build; every arm
    /// that meets it returns `Unsupported("rdma")` (CT-I11).
    OnRemote(NodeId, RemoteRef),
}

impl State {
    /// Index into the `entries_by_state` histogram of section j.
    pub fn index(&self) -> usize {
        match self {
            State::Resident(_) => 0,
            State::ResidentOnDisk(_) => 1,
            State::Promoting(_, _) => 2,
            State::Demoting(_, _) => 3,
            State::OnDisk(_) => 4,
            State::Evicted => 5,
            State::Consumed => 6,
            State::OnRemote(_, _) => 7,
        }
    }

    /// The resident tier the bytes are addressable in, when there is one. A state with a
    /// move in flight reports the tier the bytes are still in (the source), because a move
    /// never invalidates its source (RE-I1).
    pub fn resident_tier(&self) -> Option<Tier> {
        match self {
            State::Resident(tier) | State::ResidentOnDisk(tier) => Some(*tier),
            State::Promoting(from, _) | State::Demoting(from, _) => {
                if from.is_resident() {
                    Some(*from)
                } else {
                    None
                }
            }
            State::OnDisk(_) | State::Evicted | State::Consumed | State::OnRemote(_, _) => None,
        }
    }

    /// The tier the entry's bytes are accounted against right now: the resident tier, or
    /// `None` for an entry with no resident bytes.
    pub fn accounted_tier(&self) -> Option<Tier> {
        self.resident_tier()
    }

    /// True while a promotion or a demotion is in flight for this entry (PL-I2).
    pub fn in_flight(&self) -> bool {
        matches!(self, State::Promoting(_, _) | State::Demoting(_, _))
    }

    /// The state a move of this entry falls back to when the move fails twice (f.10):
    /// the entry stays where its bytes were.
    pub fn source_state(&self, disk: Option<SegmentRef>) -> State {
        match self {
            State::Promoting(from, _) | State::Demoting(from, _) => {
                if from.is_resident() {
                    match disk {
                        Some(_) => State::ResidentOnDisk(*from),
                        None => State::Resident(*from),
                    }
                } else {
                    match disk {
                        Some(seg) => State::OnDisk(seg),
                        None => State::Evicted,
                    }
                }
            }
            other => other.clone(),
        }
    }
}

/// True when a payload resident in `tier` satisfies `want` (f.3). `TierPref::Host` accepts
/// either host tier, which costs nothing because a run has only one of them (contracts e.1).
pub fn satisfies(tier: Tier, want: &PayloadSpec) -> bool {
    match want.tier {
        TierPref::Any => tier.is_resident(),
        TierPref::Host => tier.is_host(),
        TierPref::Device => matches!(tier, Tier::Device(_)),
    }
}

/// True when a payload of `kind` satisfies `want`'s payload kind.
pub fn kind_satisfies(kind: PayloadKind, want: &PayloadSpec) -> bool {
    match want.kind {
        PayloadKind::Either => true,
        PayloadKind::Table => kind == PayloadKind::Table,
        PayloadKind::Tensor => kind == PayloadKind::Tensor,
    }
}

/// An entry's payload, held apart from the morsel header so that a DMA source can be built
/// from it without consuming the morsel: a table's Arrow buffers and a tensor both need a
/// shared handle, and `BufferView::of_tensor` takes an `Arc<ManagedTensor>` (contracts d.3).
pub enum EntryPayload {
    /// A record batch and the tier its buffers are in.
    Table(RecordBatch, Tier),
    /// A tensor and the tier its bytes are in.
    Tensor(Arc<ManagedTensor>, Tier),
}

impl EntryPayload {
    /// The tier the bytes are in.
    pub fn tier(&self) -> Tier {
        match self {
            EntryPayload::Table(_, tier) | EntryPayload::Tensor(_, tier) => *tier,
        }
    }

    /// `Table` or `Tensor`.
    pub fn kind(&self) -> PayloadKind {
        match self {
            EntryPayload::Table(_, _) => PayloadKind::Table,
            EntryPayload::Tensor(_, _) => PayloadKind::Tensor,
        }
    }

    /// A shareable reference to the payload for building DMA sources outside the queue lock
    /// (g: the clone is a refcount bump, never a byte copy).
    pub fn reference(&self) -> PayloadRef {
        match self {
            EntryPayload::Table(batch, _) => PayloadRef::Table(batch.clone()),
            EntryPayload::Tensor(tensor, _) => PayloadRef::Tensor(Arc::clone(tensor)),
        }
    }

    /// A consumer's share of the bytes as the contract's `Payload` (f.3, CT-I15): the batch's
    /// buffers or the tensor's bytes, shared and never copied, so a take never waits for a DMA
    /// view a completion still holds to go.
    pub fn share(&self) -> Payload {
        match self {
            EntryPayload::Table(batch, tier) => Payload::Table(batch.clone(), *tier),
            EntryPayload::Tensor(tensor, tier) => Payload::Tensor(tensor.share(), *tier),
        }
    }
}

impl From<Payload> for EntryPayload {
    fn from(payload: Payload) -> EntryPayload {
        match payload {
            Payload::Table(batch, tier) => EntryPayload::Table(batch, tier),
            Payload::Tensor(tensor, tier) => EntryPayload::Tensor(Arc::new(tensor), tier),
        }
    }
}

/// A shareable handle on an entry's payload, taken under the queue lock and used to build
/// `BufferView`s after the lock is released (preamble 4.2).
#[derive(Clone)]
pub enum PayloadRef {
    /// The batch; cloning it bumps the refcount of its Arrow buffers.
    Table(RecordBatch),
    /// The tensor.
    Tensor(Arc<ManagedTensor>),
}

impl PayloadRef {
    /// `Table` or `Tensor`.
    pub fn kind(&self) -> PayloadKind {
        match self {
            PayloadRef::Table(_) => PayloadKind::Table,
            PayloadRef::Tensor(_) => PayloadKind::Tensor,
        }
    }
}

/// One queued morsel: its place in its queue and the batch it names (b, f.18). Where the batch's
/// bytes are, its disk copy and its move in flight are the catalog's, kept once for every queue
/// the batch waits in (PL-I18); the entry keeps only what is the morsel's own on this queue.
pub struct Entry {
    /// The sequence number on this queue.
    pub seq: Seq,
    /// The stage whose output this is.
    pub stage: StageId,
    /// The lineage of the morsel (CT-I12).
    pub origin: Origin,
    /// The features the morsel carried, moved back into it at `pop`.
    pub features: MorselFeatures,
    /// The batch in the catalog.
    pub buffer: BufferId,
}

impl Entry {
    /// Rebuild the morsel this entry names from a consumer's share of its batch, for `pop`.
    /// Moves the features out of the entry, so the pop path allocates nothing (l).
    pub fn into_morsel(mut self, payload: Payload) -> Morsel {
        let bytes = payload.bytes();
        Morsel {
            seq: self.seq,
            stage: self.stage,
            payload,
            bytes,
            origin: self.origin,
            features: std::mem::take(&mut self.features),
        }
    }
}
