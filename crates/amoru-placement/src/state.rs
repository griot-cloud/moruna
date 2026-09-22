//! The entry state machine (09 e.1) and the payload an entry holds (e.2).

use std::sync::Arc;

use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{
    AmoruError, ManagedTensor, Morsel, MorselFeatures, NodeId, Origin, Payload, PayloadKind,
    PayloadSpec, RemoteRef, SegmentRef, Seq, StageId, Tier, TierPref,
};

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

    /// Take the payload apart into the contract's `Payload`. On failure the payload comes
    /// back untouched, so the entry that held it loses nothing.
    pub fn into_payload(self) -> Result<Payload, (EntryPayload, AmoruError)> {
        match self {
            EntryPayload::Table(batch, tier) => {
                // The tier tag is the arena's (CT-I2): the batch's buffers came from the
                // arena or from a reactor read into an arena buffer, so the safe constructor
                // infers the same tier the entry recorded. Cloning a batch is a refcount
                // bump, so the payload survives a refusal.
                match Payload::table(batch.clone()) {
                    Ok(payload) if payload.tier() == tier => Ok(payload),
                    Ok(payload) => {
                        let found = payload.tier();
                        Err((
                            EntryPayload::Table(batch, tier),
                            AmoruError::Staging(format!(
                                "entry recorded {tier:?} but its buffers are in {found:?}"
                            )),
                        ))
                    }
                    Err(error) => Err((EntryPayload::Table(batch, tier), error)),
                }
            }
            // `try_unwrap` hands the handle back when a DMA view of the tensor is still
            // alive, which is the one case in which the entry must keep its bytes and the
            // caller must try again.
            EntryPayload::Tensor(tensor, tier) => match Arc::try_unwrap(tensor) {
                Ok(tensor) => {
                    let tier = tensor.tier();
                    Ok(Payload::Tensor(tensor, tier))
                }
                Err(tensor) => Err((
                    EntryPayload::Tensor(tensor, tier),
                    AmoruError::Staging(
                        "a DMA view of this tensor is still alive; the move has not released it"
                            .into(),
                    ),
                )),
            },
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

/// One queued morsel with its placement state; identified by `(stage, seq)` (b).
pub struct Entry {
    /// The sequence number the source assigned.
    pub seq: Seq,
    /// The stage whose output this is.
    pub stage: StageId,
    /// The lineage of the morsel (CT-I12).
    pub origin: Origin,
    /// `payload.bytes()` at push; the figure every tier counter moves by.
    pub bytes: u64,
    /// The features the morsel carried, moved back into it at `pop`.
    pub features: MorselFeatures,
    /// Table or tensor.
    pub kind: PayloadKind,
    /// The bytes, while the entry has any.
    pub payload: Option<EntryPayload>,
    /// Where the bytes are and what is in flight (e.1).
    pub state: State,
    /// `stage == 0` in v1 (b, D5).
    pub recomputable: bool,
    /// The segment record holding a copy of these bytes, when there is one.
    pub disk: Option<SegmentRef>,
    /// The move in flight for this entry, when there is one (PL-I2).
    pub move_id: Option<u64>,
}

impl Entry {
    /// An entry over a freshly pushed morsel.
    pub fn new(morsel: Morsel, recomputable: bool) -> Entry {
        let Morsel {
            seq,
            stage,
            payload,
            bytes,
            origin,
            features,
        } = morsel;
        let kind = payload.kind();
        let payload = EntryPayload::from(payload);
        let tier = payload.tier();
        Entry {
            seq,
            stage,
            origin,
            bytes,
            features,
            kind,
            payload: Some(payload),
            state: State::Resident(tier),
            recomputable,
            disk: None,
            move_id: None,
        }
    }

    /// An entry a `restore` rebuilt from a manifest record: bytes on disk, no arena buffer.
    pub fn on_disk(
        seq: Seq,
        stage: StageId,
        origin: Origin,
        bytes: u64,
        kind: PayloadKind,
        seg: SegmentRef,
    ) -> Entry {
        Entry {
            seq,
            stage,
            origin,
            bytes,
            features: MorselFeatures::default(),
            kind,
            payload: None,
            state: State::OnDisk(seg),
            recomputable: stage == 0,
            disk: Some(seg),
            move_id: None,
        }
    }

    /// Rebuild the morsel this entry holds, for `pop`. Moves the payload and the features
    /// out of the entry, so the pop path allocates nothing (l). On failure the entry keeps
    /// its payload and its features, so nothing is lost and the caller may try again.
    pub fn take_morsel(&mut self) -> Result<Morsel, AmoruError> {
        let Some(payload) = self.payload.take() else {
            return Err(AmoruError::Staging(format!(
                "pop: entry {} of stage {} has no resident bytes",
                self.seq, self.stage
            )));
        };
        let payload = match payload.into_payload() {
            Ok(payload) => payload,
            Err((payload, error)) => {
                self.payload = Some(payload);
                return Err(error);
            }
        };
        let bytes = payload.bytes();
        let features = std::mem::take(&mut self.features);
        Ok(Morsel {
            seq: self.seq,
            stage: self.stage,
            payload,
            bytes,
            origin: self.origin.clone(),
            features,
        })
    }
}
