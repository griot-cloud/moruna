//! One exclusive namespace writer, bound to the actual SDK connection and never-reused
//! generation. Drivers consume stamps only at original IO/flush completion, not disconnect.
use crate::block;
use moruna_svc::ConnectionId;
/// Namespace write admission. This is device ownership, not object credential verification.
#[derive(Default)]
pub struct Lease {
    owner: Option<Owner>,
    generation: u64,
    poisoned: bool,
}
struct Owner {
    connection: ConnectionId,
    generation: u64,
    inflight: u64,
    closing: bool,
    flushing: bool,
    dirty: bool,
}
/// Non-cloneable original owner IO ownership, consumed only by its actual completion.
pub struct Io {
    connection: ConnectionId,
    generation: u64,
}
/// Non-cloneable final flush ownership, consumed after the actual durable barrier.
pub struct Release {
    connection: ConnectionId,
    generation: u64,
    dirty: bool,
}
impl Release {
    /// Actual owner connection, used to correlate any deferred explicit release reply.
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }
    /// Whether this lease admitted writes that require the driver's durability barrier.
    pub fn needs_flush(&self) -> bool {
        self.dirty
    }
}
impl Lease {
    /// Claim only a vacant, healthy namespace. Caller separately checks actual read-only
    /// device geometry; no token ever wraps or reaches a different connection by reuse.
    pub fn claim(&mut self, connection: ConnectionId) -> Result<u64, block::Error> {
        if self.poisoned {
            return Err(block::Error::LeaseFailed);
        }
        if self.owner.is_some() {
            return Err(block::Error::LeaseInUse);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(block::Error::LeaseFailed)?;
        self.generation = generation;
        self.owner = Some(Owner {
            connection,
            generation,
            inflight: 0,
            closing: false,
            flushing: false,
            dirty: false,
        });
        Ok(generation)
    }
    fn owner(
        &mut self,
        connection: ConnectionId,
        generation: u64,
    ) -> Result<&mut Owner, block::Error> {
        if self.poisoned {
            return Err(block::Error::LeaseFailed);
        }
        self.owner
            .as_mut()
            .filter(|o| o.connection == connection && o.generation == generation)
            .ok_or(block::Error::NotWriter)
    }
    /// Validate before any mutating operation is admitted or pinned for submission.
    pub fn authorize(
        &mut self,
        connection: ConnectionId,
        generation: u64,
    ) -> Result<(), block::Error> {
        let owner = self.owner(connection, generation)?;
        if owner.closing {
            return Err(block::Error::LeaseClosing);
        }
        Ok(())
    }
    /// Read permission needs no writer grant. Reads on the actual lease owner's channel
    /// are nevertheless counted, so disconnect/handoff proves its original DMA is over.
    pub fn can_read(&self, connection: ConnectionId) -> Result<(), block::Error> {
        if self.poisoned {
            return Err(block::Error::LeaseFailed);
        }
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.connection == connection && owner.closing)
        {
            return Err(block::Error::LeaseClosing);
        }
        Ok(())
    }
    /// Track original reads only when this actual connection owns the writer. Other
    /// readonly clients remain unprivileged and do not gain/need a namespace lease.
    pub fn read(&mut self, connection: ConnectionId) -> Result<Option<Io>, block::Error> {
        self.can_read(connection)?;
        let generation = self
            .owner
            .as_ref()
            .filter(|owner| owner.connection == connection)
            .map(|owner| owner.generation);
        match generation {
            Some(generation) => self.begin(connection, generation, false).map(Some),
            None => Ok(None),
        }
    }
    /// Track one actual write after parameter/buffer validation, before device submission.
    pub fn write(&mut self, connection: ConnectionId, generation: u64) -> Result<Io, block::Error> {
        self.begin(connection, generation, true)
    }
    /// Track one explicit flush until its original completion, retaining the same owner.
    pub fn flush(&mut self, connection: ConnectionId, generation: u64) -> Result<Io, block::Error> {
        self.begin(connection, generation, false)
    }
    fn begin(
        &mut self,
        connection: ConnectionId,
        generation: u64,
        dirty: bool,
    ) -> Result<Io, block::Error> {
        self.authorize(connection, generation)?;
        let owner = self.owner(connection, generation)?;
        owner.inflight = owner
            .inflight
            .checked_add(1)
            .ok_or(block::Error::LeaseFailed)?;
        owner.dirty |= dirty;
        Ok(Io {
            connection,
            generation,
        })
    }
    /// Original CQE/used-ring result consumed, including failures whose DMA is now over.
    pub fn finish(&mut self, io: Io) -> Result<(), block::Error> {
        let Some(owner) = self.owner.as_mut().filter(|o| {
            o.connection == io.connection && o.generation == io.generation && o.inflight > 0
        }) else {
            self.poisoned = true;
            return Err(block::Error::LeaseFailed);
        };
        owner.inflight -= 1;
        Ok(())
    }
    /// Explicit release closes admission; a successful reply must wait for final barrier.
    pub fn close(&mut self, connection: ConnectionId, generation: u64) -> Result<(), block::Error> {
        self.authorize(connection, generation)?;
        self.owner(connection, generation)?.closing = true;
        Ok(())
    }
    /// Peer departure closes admission without dropping any original IO ownership.
    pub fn disconnect(&mut self, connection: ConnectionId) {
        if let Some(owner) = self.owner.as_mut().filter(|o| o.connection == connection) {
            owner.closing = true;
        }
    }
    /// Mint one final barrier only after all admitted original write/flush results arrived.
    pub fn releasing(&mut self) -> Option<Release> {
        let owner = self.owner.as_mut()?;
        if !owner.closing || owner.inflight != 0 || owner.flushing {
            return None;
        }
        owner.flushing = true;
        Some(Release {
            connection: owner.connection,
            generation: owner.generation,
            dirty: owner.dirty,
        })
    }
    /// Transfer ownership only after the driver's actual required flush. Any mismatch or
    /// failed barrier permanently refuses further claims until device-level recovery.
    pub fn released(&mut self, release: Release, success: bool) -> Result<(), block::Error> {
        let valid = self.owner.as_ref().is_some_and(|o| {
            o.connection == release.connection
                && o.generation == release.generation
                && o.closing
                && o.inflight == 0
                && o.flushing
        });
        if !valid || !success {
            self.poisoned = true;
            return Err(block::Error::LeaseFailed);
        }
        self.owner = None;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnect_waits_for_every_original_write_and_final_flush() {
        let mut state = Lease::default();
        let token = state.claim(1).unwrap();
        let first = state.write(1, token).unwrap();
        let second = state.write(1, token).unwrap();
        assert!(matches!(
            state.write(2, token),
            Err(block::Error::NotWriter)
        ));
        state.disconnect(1);
        assert!(state.releasing().is_none());
        assert!(matches!(state.claim(2), Err(block::Error::LeaseInUse)));
        state.finish(first).unwrap();
        assert!(state.releasing().is_none());
        state.finish(second).unwrap();
        let flush = state.releasing().unwrap();
        assert!(flush.needs_flush());
        assert!(matches!(state.claim(2), Err(block::Error::LeaseInUse)));
        state.released(flush, true).unwrap();
        let next = state.claim(2).unwrap();
        assert_ne!(next, token);
        assert!(matches!(
            state.write(2, token),
            Err(block::Error::NotWriter)
        ));
    }
    #[test]
    fn owner_read_blocks_handoff_but_readonly_peer_needs_no_lease() {
        let mut state = Lease::default();
        assert!(state.read(9).unwrap().is_none());
        let token = state.claim(7).unwrap();
        let read = state.read(7).unwrap().unwrap();
        assert!(state.read(9).unwrap().is_none());
        state.disconnect(7);
        assert!(matches!(state.read(7), Err(block::Error::LeaseClosing)));
        assert!(state.read(9).unwrap().is_none());
        assert!(state.releasing().is_none());
        assert!(matches!(state.claim(9), Err(block::Error::LeaseInUse)));
        state.finish(read).unwrap();
        let release = state.releasing().unwrap();
        assert!(!release.needs_flush());
        state.released(release, true).unwrap();
        assert_ne!(state.claim(9).unwrap(), token);
    }
    #[test]
    fn release_retains_explicit_flush_and_refuses_failed_durability() {
        let mut state = Lease::default();
        let token = state.claim(7).unwrap();
        let flush = state.flush(7, token).unwrap();
        state.close(7, token).unwrap();
        assert!(state.releasing().is_none());
        state.finish(flush).unwrap();
        let release = state.releasing().unwrap();
        assert!(!release.needs_flush());
        assert!(matches!(
            state.released(release, false),
            Err(block::Error::LeaseFailed)
        ));
        assert!(matches!(state.claim(8), Err(block::Error::LeaseFailed)));
    }
}
