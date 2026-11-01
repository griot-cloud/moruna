//! PL-T1 head_hot (PL-I1, PL-I2): the head of every queue is resident, or has exactly one
//! promotion in flight toward the target tier and no demotion. Driven twice: over the pure
//! planner of f.2, and over the engine with delayed moves. The planner decides promotions and
//! reports pressure; which batches leave memory is the catalog's (f.18, PL-T29 to PL-T33).

mod common;

use moruna_kernel::{Locality, PayloadKind, PayloadSpec, Placement, Tier, TierKind, TierPref};
use moruna_placement::plan::{EntryView, PlanInput, plan};
use moruna_placement::state::State;
use moruna_testkit::{FakeAllocator, FakeReactor};
use std::time::Duration;

fn random_input(rng: &mut common::Rng, host: Tier, target: Tier) -> PlanInput {
    let count = rng.below(9) as usize;
    let entries = (0..count)
        .map(|_| {
            let pick = rng.below(6);
            let on_disk = pick == 3;
            let evicted = pick == 4;
            let tier = match pick {
                0 => Some(host),
                1 => Some(Tier::Device(moruna_kernel::DeviceId(0))),
                2 => Some(host),
                _ => None,
            };
            EntryView {
                tier,
                on_disk,
                in_flight: pick == 5,
                evicted,
                on_remote: false,
            }
        })
        .collect();
    let mut water = [(0u64, u64::MAX); moruna_kernel::TIER_COUNT];
    for mark in water.iter_mut() {
        let high = rng.below(40);
        *mark = (high / 2, high);
    }
    let mut bytes = [0u64; moruna_kernel::TIER_COUNT];
    for slot in bytes.iter_mut() {
        *slot = rng.below(60);
    }
    PlanInput {
        entries,
        target,
        want: PayloadSpec {
            kind: PayloadKind::Either,
            tier: if target == host {
                TierPref::Host
            } else {
                TierPref::Device
            },
        },
        host_tier: host,
        water,
        bytes,
        window: 1 + rng.below(4) as u16,
    }
}

#[test]
fn pl_t1_head_hot_planner() {
    let mut rng = common::Rng::new(0x51ACE);
    for host in [Tier::Host, Tier::PinnedHost] {
        for target in [host, Tier::Device(moruna_kernel::DeviceId(0))] {
            for _ in 0..4000 {
                let input = random_input(&mut rng, host, target);
                let out = plan(&input);
                // Only the head is promoted as the head, and only inside the window (PL-I1).
                let window = usize::from(input.window).max(1);
                for intent in &out.intents {
                    assert!(
                        intent.index < input.entries.len().min(window),
                        "a promotion names an entry outside the window: {intent:?}"
                    );
                    assert_eq!(intent.head, intent.index == 0, "{intent:?}");
                    assert_eq!(intent.to, input.target, "{intent:?}");
                    let entry = &input.entries[intent.index];
                    assert!(
                        !entry.in_flight && !entry.evicted,
                        "{intent:?} on {entry:?}"
                    );
                }
                // At most one move per entry (PL-I2).
                let mut seen = vec![0usize; input.entries.len()];
                for intent in &out.intents {
                    seen[intent.index] += 1;
                    assert!(
                        seen[intent.index] <= 1,
                        "entry {} got two moves in one plan",
                        intent.index
                    );
                }
                // Pressure is what the queue holds above a high mark, down to the low one.
                for kind in [TierKind::Device, host.kind()] {
                    let (low, high) = input.water[kind.index()];
                    let held = input.bytes[kind.index()];
                    let asked = out
                        .pressure
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .map(|(_, need)| *need);
                    if out.head_evicted {
                        assert!(out.pressure.is_empty() && out.intents.is_empty());
                    } else if held > high {
                        assert_eq!(asked, Some(held - low), "{kind:?}");
                    } else {
                        assert_eq!(asked, None, "{kind:?}");
                    }
                }
                // A head that is not where the consumer wants it gets a promotion, unless it
                // has no bytes anywhere (f.2 step 1 stops the queue for `evicted()`).
                if let Some(head) = input.entries.first() {
                    let satisfied = head.tier.is_some_and(|tier| match input.want.tier {
                        TierPref::Any => tier.is_resident(),
                        TierPref::Host => tier.is_host(),
                        TierPref::Device => matches!(tier, Tier::Device(_)),
                    });
                    let promotable = head.tier.is_some() || head.on_disk;
                    if !satisfied && !head.in_flight && !head.evicted && promotable {
                        assert!(
                            out.intents.iter().any(|i| i.index == 0 && i.head),
                            "an unsatisfied head must be promoted: {:?}",
                            head
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn pl_t1_head_hot_engine() {
    let scratch = common::Scratch::new("t1");
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(Duration::from_millis(2));
    let cfg = common::config(
        1,
        Some(scratch.path().to_path_buf()),
        common::budgets(1 << 30, 0, 1 << 30),
    );
    let engine = common::engine(cfg, &alloc, &reactor);
    engine.set_staging(0, true);
    let sample = common::table_morsel(&alloc, 0, 0, 32);
    let bytes = sample.bytes;
    drop(sample);

    let mut rng = common::Rng::new(99);
    let mut next_seq = 0u64;
    let mut popped = 0u64;
    for step in 0..400u64 {
        match rng.below(10) {
            0..=5 => {
                engine
                    .push(0, common::table_morsel(&alloc, next_seq, 0, 32))
                    .expect("push");
                next_seq += 1;
            }
            6..=7 => {
                if let Some(morsel) = engine
                    .pop(0, common::want_host(), Locality::Any)
                    .expect("pop")
                {
                    engine.release(0, morsel.seq).expect("release");
                    popped += 1;
                }
            }
            8 => engine.set_promotion_window(0, 1 + (step % 4) as u16),
            _ => engine.set_water(0, TierKind::Host, bytes * 2, bytes * (2 + step % 5)),
        }
        // The head is resident where the consumer wants it, or a promotion is in flight for
        // it, or it has no bytes at all and is waiting for `replace` (f.2 step 1).
        if let Some((seq, state)) = engine.head_state(0) {
            let ok = match &state {
                State::Resident(tier) | State::ResidentOnDisk(tier) => tier.is_host(),
                State::Promoting(_, to) => to.is_host(),
                State::OnDisk(_) | State::Evicted => true,
                // PL-I1's antecedent is "the head is not resident in a satisfying tier". A
                // demotion never invalidates its source (RE-I1), so a head whose demotion
                // was planned before pops made it the head is still resident and the
                // invariant holds; f.5 lands such a record as `Resident + OnDisk`.
                State::Demoting(from, _) => from.is_host(),
                State::Consumed | State::OnRemote(_, _) => false,
            };
            assert!(ok, "head {seq} is in {state:?} (PL-I1, PL-I2)");
        }
    }
    common::settle(&reactor);
    // Whatever is left must still drain in order.
    engine.close(0);
    let mut last = None;
    while let Some((morsel, _)) = engine
        .pop_blocking(0, common::want_host(), Locality::Any)
        .expect("pop_blocking")
    {
        if let Some(previous) = last {
            assert!(morsel.seq > previous, "FIFO order");
        }
        last = Some(morsel.seq);
        engine.release(0, morsel.seq).expect("release");
        popped += 1;
    }
    assert_eq!(popped, next_seq, "every pushed morsel came back");
    assert_eq!(moruna_placement::locks::violations(), 0, "lock order (4.2)");
}
