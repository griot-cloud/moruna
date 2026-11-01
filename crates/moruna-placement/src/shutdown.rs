//! `shutdown` (f.15): cancel every in-flight move, release tier accounting, wake every
//! parked caller, and leave the segments alone because a manifest may reference them.

use std::sync::atomic::Ordering;

use crate::PlacementEngine;
use crate::state::State;

impl PlacementEngine {
    /// `Placement::shutdown` (f.15). Idempotent, and it never blocks: a late completion
    /// finds no move id and returns without touching a queue.
    pub(crate) fn shutdown_engine(&self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let reservations: Vec<(usize, u64)> = {
            let mut moves = self.lock_moves();
            let held = moves
                .values()
                .filter_map(|held| held.reservation)
                .collect::<Vec<_>>();
            moves.clear();
            held
        };
        self.in_flight_bytes.store(0, Ordering::Release);
        {
            let mut catalog = self.lock_catalog();
            for (slot, bytes) in reservations {
                catalog.unreserve(slot, bytes);
            }
            // A promotion in flight stays where its bytes were; a demotion in flight stays
            // resident (f.15). Neither loses bytes, because the reactor was given a view, not the
            // buffer (RE-I1).
            let moving: Vec<_> = catalog
                .buffers()
                .into_iter()
                .filter(|buffer| buffer.state.in_flight())
                .map(|buffer| buffer.id)
                .collect();
            for id in moving {
                catalog.update(id, |buffer| {
                    buffer.state = buffer.state.source_state(buffer.disk);
                    buffer.move_id = None;
                });
            }
        }
        for index in 0..self.queues.len() {
            let waiters = {
                let mut queue = self.lock_queue(index);
                queue.closed = true;
                queue.blocked = true;
                queue.waiters.clone()
            };
            for (_, waiter) in waiters {
                waiter.unpark();
            }
        }
    }

    /// True when any move is still in the moves map; the shutdown tests read it.
    pub fn any_in_flight(&self) -> bool {
        let moves = self.lock_moves();
        !moves.is_empty()
    }
}

/// The states a shutdown leaves behind, for the test that checks them (PL-T22).
pub fn is_settled(state: &State) -> bool {
    !state.in_flight()
}
