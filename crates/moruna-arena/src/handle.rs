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
    fn release(&self, ptr: *mut u8, len: usize, tier: Tier) {
        match tier {
            Tier::Host | Tier::PinnedHost => {
                if self.host_tier != tier {
                    self.foreign(tier);
                    return;
                }
                let retire = {
                    let host = self.read_host();
                    match host.iter().find(|r| r.space.contains(ptr)) {
                        Some(region) => {
                            let released = region.space.release(ptr, len as u64);
                            // A draining region whose last buffer just came home is unmapped
                            //; that needs the write lock, taken once this one is gone.
                            matches!(released, crate::classes::Release::Freed)
                                && region.draining.load(Ordering::SeqCst)
                                && region.space.in_use() == 0
                        }
                        None => {
                            drop(host);
                            self.foreign(tier);
                            return;
                        }
                    }
                };
                if retire {
                    let mut host = self.write_host();
                    self.retire_drained(&mut host);
                }
            }
            Tier::Device(id) => match self.device(id) {
                Some(space) if space.contains(ptr) => {
                    space.release(ptr, len as u64);
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
