//! The move planner (09 f.2), written as a pure function from a queue snapshot to the promotions
//! it decides and the pressure it finds, so that PL-T1 can drive it without an engine, an arena
//! or a reactor. Which batches leave memory under that pressure is not the queue's to decide: the
//! buffer catalog chooses them across every queue by the known future (f.18). Issuing is a thin
//! layer over both (`moves.rs`) and is the only part that touches the reactor.

use moruna_kernel::{PayloadSpec, TIER_COUNT, Tier, TierKind};

use crate::state::satisfies;

/// One entry as the planner sees it: where its batch's bytes are (f.18).
#[derive(Clone, Debug)]
pub struct EntryView {
    /// The resident tier, when the batch has one.
    pub tier: Option<Tier>,
    /// True when the batch has no resident bytes and only a segment record.
    pub on_disk: bool,
    /// True while a move is in flight for the batch (PL-I2).
    pub in_flight: bool,
    /// True when the batch has no bytes anywhere and must be replaced.
    pub evicted: bool,
    /// Reserved: the batch's bytes are in another node's memory (`rdma`).
    pub on_remote: bool,
}

/// The queue state the planner reads.
#[derive(Clone, Debug)]
pub struct PlanInput {
    /// The queue in FIFO order, head at index 0; the engine gives the head and the promotion
    /// window, which is all the planner reads.
    pub entries: Vec<EntryView>,
    /// The tier promotions aim at (b).
    pub target: Tier,
    /// What the consumer declared.
    pub want: PayloadSpec,
    /// The run's one host tier (contracts e.1).
    pub host_tier: Tier,
    /// `(low, high)` per `TierKind::index()`.
    pub water: [(u64, u64); TIER_COUNT],
    /// The bytes per `TierKind::index()` of the batches this queue holds, from the catalog, less
    /// those a demotion in flight is already taking out of the tier.
    pub bytes: [u64; TIER_COUNT],
    /// The promotion window `k`, entries from the head inclusive.
    pub window: u16,
}

/// One promotion the planner decided on, by position in the queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    /// Position in the queue.
    pub index: usize,
    /// Where the bytes are.
    pub from: Tier,
    /// Where they should be.
    pub to: Tier,
    /// True for the head, whose promotion is never skipped for a window reason (PL-I1).
    pub head: bool,
}

/// What one planning pass decided.
#[derive(Clone, Debug, Default)]
pub struct PlanOutput {
    /// The promotions to issue, head first, then the window in position order.
    pub intents: Vec<Intent>,
    /// Per tier above its high water mark, the bytes the queue must shed to reach its low water
    /// mark: what the catalog is asked to free from the batches this queue holds (f.2 step 3).
    pub pressure: Vec<(TierKind, u64)>,
    /// True when the head has no bytes anywhere and must be replaced before anything else
    /// can be planned for this queue (f.2 step 1).
    pub head_evicted: bool,
}

/// One tier down from `from`: `Device` to the host tier, the host tier to `Disk` (e.4).
/// `None` for a tier that has nothing below it in the v1 ladder.
pub fn one_tier_down(from: Tier, host_tier: Tier) -> Option<TierKind> {
    match from {
        Tier::Device(_) => Some(host_tier.kind()),
        Tier::PinnedHost | Tier::Host => Some(TierKind::Disk),
        Tier::Disk(_) => None,
        Tier::Remote(_, _) => None,
    }
}

/// Plan one queue (f.2). Pure: it reads the snapshot and writes nothing.
pub fn plan(input: &PlanInput) -> PlanOutput {
    let mut out = PlanOutput::default();
    let window = usize::from(input.window).max(1).min(input.entries.len());

    // 1. Head first (PL-I1).
    if let Some(head) = input.entries.first() {
        if head.evicted {
            out.head_evicted = true;
            return out;
        }
        if head.on_remote {
            // Reserved (rdma): the head's bytes are on another node. No v1 path produces
            // this, and the issuer refuses it with `Unsupported("rdma")`.
            return out;
        }
        if !head.in_flight
            && !head.tier.is_some_and(|tier| satisfies(tier, &input.want))
            && let Some(from) = head.tier.or(head.on_disk.then(disk_marker))
        {
            out.intents.push(Intent {
                index: 0,
                from,
                to: input.target,
                head: true,
            });
        }
    }

    // 2. Window: entries 2..=k from the head, in position order.
    for index in 1..window {
        let entry = &input.entries[index];
        if entry.in_flight || entry.evicted || entry.on_remote {
            continue;
        }
        if entry.tier.is_some_and(|tier| tier == input.target) {
            continue;
        }
        let Some(from) = entry.tier.or(entry.on_disk.then(disk_marker)) else {
            continue;
        };
        out.intents.push(Intent {
            index,
            from,
            to: input.target,
            head: false,
        });
    }

    // 3. Pressure, from Device down to the host tier: what the queue holds above a high water
    // mark is shed down to the low one, by the catalog (f.18).
    for kind in [TierKind::Device, input.host_tier.kind()] {
        let slot = kind.index();
        let (low, high) = input.water[slot];
        if input.bytes[slot] > high {
            out.pressure.push((kind, input.bytes[slot] - low));
        }
    }
    out
}

/// The `from` tier of a promotion out of a segment record. The planner works from positions
/// and tier kinds, so it does not carry the `SegmentRef`; the issuer reads the real one from
/// the entry under the queue lock.
fn disk_marker() -> Tier {
    Tier::Disk(moruna_kernel::SegmentRef {
        segment: 0,
        offset: 0,
        len: 0,
    })
}
