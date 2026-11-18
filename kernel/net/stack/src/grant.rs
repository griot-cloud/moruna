//! A grant (SDD 31, f.6; work item 1.3b): the policy every connection through one
//! listener of the server holds, and the sockets those connections hold between them.
//!
//! A grant's `sockets` bound counts every socket of its listeners and connections until
//! the socket is freed, closing and waiting states included: each listener, each
//! listener's socket listening for the next connection, each connection half-open,
//! established and not yet accepted, accepted or connected, each one closing after its
//! client closed it or went, each UDP socket, and each connect by name still resolving.
//! The bound is the grant's, shared by every client connection through its listener, so
//! connecting again does not renew it.
//!
//! A listener's listening socket is counted because it is the next connection: when it
//! takes a SYN it becomes one, without the count changing, and a listener is given a
//! new one only while the grant has room. A SYN that finds no listening socket is
//! answered with a reset, so a connection refused for the bound is refused at accept and
//! the peer is told.
//!
//! The live connections its listeners take (half-open, established and not yet closed
//! by the client) are also counted by the peer's address, and a peer already holding the
//! policy's `per_peer` of them has its next connection reset at once. A connection being
//! closed is no longer its peer's, so a peer that connects again and again, closing each
//! connection, never meets its bound while its closed ones linger; the grant's
//! `sockets` bound, which counts them until they are freed, is what bounds memory. Each socket has the policy's buffers and its linger,
//! the server's settings where the policy leaves them.
//!
//! When a bound is hit (the grant's, or a peer's) the server says so on its console, at
//! most once a minute a grant: `netstack: grant <slot> at its bound (<n> sockets, <m>
//! peers)`. A listener minted by `moruna.net`'s `grant` is a grant of its own, named
//! after the grant it was minted under: `<slot>/granted`.

use alloc::{collections::BTreeMap, format, string::String};
use core::cell::{Cell, RefCell};

use netstack_protocol::{Addr, Policy};

/// The least time between two of a grant's lines about its bound.
pub const LINE_EVERY_NS: u64 = 60_000_000_000;

/// One grant: its name, its policy, and what is held under it.
#[derive(Debug)]
pub struct Grant {
    name: String,
    policy: Policy,
    held: Cell<u32>,
    /// The connections each remote address holds through the grant's listeners.
    peers: RefCell<BTreeMap<Addr, u32>>,
    /// A bound was hit since the last line was said.
    hit: Cell<bool>,
    /// When the last line was said.
    said: Cell<Option<u64>>,
}

impl Grant {
    /// The grant of slot `name` (a minted one's name is its minter's and `/granted`).
    pub fn new(name: impl Into<String>, policy: Policy) -> Grant {
        Grant {
            name: name.into(),
            policy,
            held: Cell::new(0),
            peers: RefCell::new(BTreeMap::new()),
            hit: Cell::new(false),
            said: Cell::new(None),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The sockets held under the grant now.
    pub fn held(&self) -> u32 {
        self.held.get()
    }

    /// Whether one more socket fits the grant's bound; when not, the bound is hit.
    pub fn room(&self) -> bool {
        let room = self.held.get() < self.policy.sockets;
        if !room {
            self.hit.set(true);
        }
        room
    }

    /// Counts one more socket, if it fits.
    pub(crate) fn take(&self) -> bool {
        let fits = self.room();
        if fits {
            self.held.set(self.held.get() + 1);
        }
        fits
    }

    /// Counts `n` sockets fewer: freed, or never made.
    pub(crate) fn give(&self, n: u32) {
        debug_assert!(n <= self.held.get(), "a grant gives back more than it held");
        self.held.set(self.held.get().saturating_sub(n));
    }

    /// The connections `peer` holds through the grant's listeners.
    pub fn peer_held(&self, peer: &Addr) -> u32 {
        self.peers.borrow().get(peer).copied().unwrap_or(0)
    }

    /// The addresses that hold connections through the grant's listeners.
    pub fn peers(&self) -> usize {
        self.peers.borrow().len()
    }

    /// Counts one more connection of `peer`'s, if it fits the policy's `per_peer`; when
    /// not, the bound is hit.
    pub(crate) fn peer_take(&self, peer: Addr) -> bool {
        let mut peers = self.peers.borrow_mut();
        let n = peers.get(&peer).copied().unwrap_or(0);
        if n >= self.policy.per_peer() {
            self.hit.set(true);
            return false;
        }
        peers.insert(peer, n + 1);
        true
    }

    /// Counts one connection of `peer`'s fewer.
    pub(crate) fn peer_give(&self, peer: &Addr) {
        let mut peers = self.peers.borrow_mut();
        let Some(n) = peers.get_mut(peer) else {
            debug_assert!(false, "a peer gives back a connection it did not hold");
            return;
        };
        *n -= 1;
        if *n == 0 {
            peers.remove(peer);
        }
    }

    /// The console's line about a bound hit since the last, at `now`, unless a line was
    /// said less than [`LINE_EVERY_NS`] ago (the hit is then not said at all).
    pub(crate) fn line(&self, now: u64) -> Option<String> {
        if !self.hit.replace(false) {
            return None;
        }
        if self
            .said
            .get()
            .is_some_and(|at| now < at.saturating_add(LINE_EVERY_NS))
        {
            return None;
        }
        self.said.set(Some(now));
        Some(format!(
            "netstack: grant {} at its bound ({} sockets, {} peers)",
            self.name,
            self.held.get(),
            self.peers()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_counts_to_its_bound_and_back() {
        let g = Grant::new(
            "listen-x",
            Policy {
                sockets: 2,
                ..Policy::default()
            },
        );
        assert_eq!(g.name(), "listen-x");
        assert_eq!(g.policy().sockets, 2);
        assert!(g.room() && g.take() && g.take());
        assert_eq!(g.held(), 2);
        assert!(!g.room() && !g.take());
        assert_eq!(g.held(), 2);
        g.give(2);
        assert_eq!(g.held(), 0);
        assert!(g.room());
    }

    #[test]
    fn a_peer_holds_at_most_its_bound_and_a_hit_is_said_once_a_minute() {
        let g = Grant::new(
            "listen-api",
            Policy {
                sockets: 81,
                per_peer: Some(2),
                ..Policy::default()
            },
        );
        let (a, b) = (Addr::v4(203, 0, 113, 1), Addr::v4(203, 0, 113, 2));
        assert_eq!(g.line(0), None, "nothing was hit");
        assert!(g.peer_take(a) && g.peer_take(a) && g.peer_take(b));
        assert_eq!((g.peer_held(&a), g.peer_held(&b), g.peers()), (2, 1, 2));
        assert!(!g.peer_take(a), "a third of a's");
        assert_eq!(g.peer_held(&a), 2);
        assert!(g.take() && g.take() && g.take());
        let line = "netstack: grant listen-api at its bound (3 sockets, 2 peers)";
        assert_eq!(g.line(5).as_deref(), Some(line));
        assert_eq!(g.line(6), None, "said, and not hit again");
        assert!(!g.peer_take(a));
        assert_eq!(g.line(5 + LINE_EVERY_NS - 1), None, "within the minute");
        assert_eq!(
            g.line(5 + LINE_EVERY_NS),
            None,
            "the hit within it is not said"
        );
        assert!(!g.peer_take(a));
        assert!(g.line(5 + LINE_EVERY_NS).is_some(), "a hit a minute on");
        g.peer_give(&a);
        g.peer_give(&b);
        assert_eq!((g.peer_held(&a), g.peer_held(&b), g.peers()), (1, 0, 1));
    }
}
