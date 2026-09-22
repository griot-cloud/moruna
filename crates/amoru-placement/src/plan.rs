//! The move planner (09 f.2), written as a pure function from a queue snapshot to a list of
//! moves so that PL-T1 can drive it without an engine, an arena or a reactor. Issuing is a
//! thin layer over it (`moves.rs`) and is the only part that touches the reactor.

use amoru_kernel::{PayloadSpec, TIER_COUNT, Tier, TierKind};

use crate::state::satisfies;

/// One entry as the planner sees it.
#[derive(Clone, Debug)]
pub struct EntryView {
    /// Bytes the entry occupies in its resident tier.
    pub bytes: u64,
    /// The resident tier, when the entry has one.
    pub tier: Option<Tier>,
    /// True when a valid segment record holds a copy of these bytes.
    pub has_disk: bool,
    /// True when the entry has no resident bytes and only a segment record.
    pub on_disk: bool,
    /// True while a move is in flight for the entry (PL-I2).
    pub in_flight: bool,
    /// True when the entry has no bytes anywhere and must be replaced.
    pub evicted: bool,
    /// True when the entry's bytes can be re-obtained from its origin (CT-I12).
    pub recomputable: bool,
    /// Reserved: the entry's bytes are in another node's memory (`rdma`).
    pub on_remote: bool,
}

/// The queue state the planner reads.
#[derive(Clone, Debug)]
pub struct PlanInput {
    /// The queue in FIFO order, head at index 0.
    pub entries: Vec<EntryView>,
    /// The tier promotions aim at (b).
    pub target: Tier,
    /// What the consumer declared.
    pub want: PayloadSpec,
    /// The run's one host tier (contracts e.1).
    pub host_tier: Tier,
    /// `(low, high)` per `TierKind::index()`.
    pub water: [(u64, u64); TIER_COUNT],
    /// Resident bytes per `TierKind::index()`.
    pub bytes: [u64; TIER_COUNT],
    /// `set_staging` for this queue.
    pub staging_enabled: bool,
    /// The promotion window `k`, entries from the head inclusive.
    pub window: u16,
}

/// One move the planner decided on, by position in the queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Intent {
    /// Move the entry's bytes toward the target tier.
    Promote {
        /// Position in the queue.
        index: usize,
        /// Where the bytes are.
        from: Tier,
        /// Where they should be.
        to: Tier,
        /// True for the head, whose promotion is never skipped for a window reason (PL-I1).
        head: bool,
    },
    /// Move the entry's bytes one tier down.
    Demote {
        /// Position in the queue.
        index: usize,
        /// Where the bytes are.
        from: Tier,
        /// One tier down: the host tier, or `Disk`.
        to: TierKind,
    },
    /// Drop the resident copy of an entry that already has a valid segment record (f.7);
    /// the cheapest demotion, and no IO.
    DropResident {
        /// Position in the queue.
        index: usize,
    },
    /// Drop a recomputable entry's bytes without writing them (PL-I6).
    Evict {
        /// Position in the queue.
        index: usize,
    },
}

/// What one planning pass decided.
#[derive(Clone, Debug, Default)]
pub struct PlanOutput {
    /// The moves to issue, in the order they must be attempted.
    pub intents: Vec<Intent>,
    /// True when the target tier is above its high water mark and the pass found no entry
    /// it could demote; `is_full` reads it (f.14).
    pub blocked: bool,
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
        if !head.in_flight && !head.tier.is_some_and(|tier| satisfies(tier, &input.want)) {
            let from = head.tier.or(if head.on_disk {
                Some(disk_marker())
            } else {
                None
            });
            if let Some(from) = from {
                out.intents.push(Intent::Promote {
                    index: 0,
                    from,
                    to: input.target,
                    head: true,
                });
            }
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
        let from = entry.tier.or(if entry.on_disk {
            Some(disk_marker())
        } else {
            None
        });
        let Some(from) = from else { continue };
        out.intents.push(Intent::Promote {
            index,
            from,
            to: input.target,
            head: false,
        });
    }

    // 3. Pressure, from Device down to the host tier.
    let mut projected = input.bytes;
    let promoting: Vec<usize> = out
        .intents
        .iter()
        .filter_map(|intent| match intent {
            Intent::Promote { index, .. } => Some(*index),
            Intent::Demote { .. } | Intent::DropResident { .. } | Intent::Evict { .. } => None,
        })
        .collect();
    for kind in [TierKind::Device, input.host_tier.kind()] {
        let slot = kind.index();
        let (low, high) = input.water[slot];
        if projected[slot] <= high {
            continue;
        }
        let mut found_candidate = false;
        for index in (window..input.entries.len()).rev() {
            if projected[slot] <= low {
                break;
            }
            let entry = &input.entries[index];
            if entry.in_flight || entry.evicted || entry.on_remote || promoting.contains(&index) {
                continue;
            }
            let Some(tier) = entry.tier else { continue };
            if tier.kind() != kind {
                continue;
            }
            // An entry that already has a valid record is demoted by dropping its resident
            // copy: no IO at all (f.7).
            if entry.has_disk {
                out.intents.push(Intent::DropResident { index });
                projected[slot] = projected[slot].saturating_sub(entry.bytes);
                found_candidate = true;
                continue;
            }
            let Some(down) = one_tier_down(tier, input.host_tier) else {
                continue;
            };
            if down == TierKind::Disk && !input.staging_enabled {
                if entry.recomputable {
                    out.intents.push(Intent::Evict { index });
                    projected[slot] = projected[slot].saturating_sub(entry.bytes);
                    found_candidate = true;
                }
                // Staging off and not recomputable: no demotion is possible (PL-I6, f.2).
                continue;
            }
            out.intents.push(Intent::Demote {
                index,
                from: tier,
                to: down,
            });
            projected[slot] = projected[slot].saturating_sub(entry.bytes);
            if down != TierKind::Disk {
                projected[down.index()] = projected[down.index()].saturating_add(entry.bytes);
            }
            found_candidate = true;
        }
        if kind == input.target.kind() && !found_candidate {
            out.blocked = true;
        }
    }
    out
}

/// The `from` tier of a promotion out of a segment record. The planner works from positions
/// and tier kinds, so it does not carry the `SegmentRef`; the issuer reads the real one from
/// the entry under the queue lock.
fn disk_marker() -> Tier {
    Tier::Disk(amoru_kernel::SegmentRef {
        segment: 0,
        offset: 0,
        len: 0,
    })
}
