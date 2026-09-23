//! The `ArenaHandle` every `Buffer` carries (contracts d.3): the object a buffer's drop
//! returns its bytes to.
//!
//! `release` is called once per `Buffer`, so twice per allocation after a `split_at`, once
//! per half with that half's own pointer and length; the slot is returned to its class free
//! list when the last of its bytes comes back (e.3).

use std::sync::atomic::Ordering;

use moruna_kernel::{ArenaHandle, Tier};

use crate::Inner;

impl ArenaHandle for Inner {
    /// A release with no token. Every buffer this arena hands out carries one, so this can
    /// only be a caller releasing bytes by hand; the arena counts it and returns the bytes to
    /// nothing rather than guessing which allocation they belong to. Guessing was the defect:
    /// see `Space::release`.
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        self.release_token(ptr, len, tier, moruna_kernel::NO_TOKEN);
    }

    fn release_token(&self, ptr: *mut u8, len: usize, tier: Tier, token: u64) {
        match tier {
            Tier::Host | Tier::PinnedHost => {
                if self.host_tier == tier && self.host.space.contains(ptr) {
                    self.host.space.release(ptr, len as u64, token);
                } else {
                    self.foreign(tier);
                }
            }
            Tier::Device(id) => match self.device(id) {
                Some(space) if space.contains(ptr) => {
                    space.release(ptr, len as u64, token);
                }
                Some(_) | None => self.foreign(tier),
            },
            // The arena has no disk region: a `Tier::Disk` payload has no resident bytes
            // (contracts b), so no buffer it hands out can carry this tier.
            Tier::Disk(_) => self.foreign(tier),
            // Reserved for the multi-node extension; no v1 component produces it (CT-I11).
            Tier::Remote(_, _) => self.foreign(tier),
        }
    }
}

impl Inner {
    /// A release the arena cannot honour: a pointer outside every region, or a tier it
    /// never hands out. Counted in `ArenaStats::double_release` and logged (h, section j).
    fn foreign(&self, tier: Tier) {
        self.foreign.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "arena.release_foreign", tier = ?tier, "release of a pointer this arena did not hand out");
    }
}
