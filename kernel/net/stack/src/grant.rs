//! A grant (SDD 31, f.6; work item 1.3b): the policy every connection through one
//! listener of the server holds, the buffers its TCP sockets get, and the sockets those
//! connections hold between them.
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
//! A listener minted by `moruna.net`'s `grant` is a grant of its own, with its own bound
//! and the server's buffers: the protocol's `Policy` carries no buffers.

use core::cell::Cell;

use netstack_protocol::Policy;

/// The receive and send buffers a grant gives each of its TCP sockets, in bytes (the
/// configuration's `rx=` and `tx=`); `None` is the server's setting
/// (`tcp-receive-buffer`, `tcp-send-buffer`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buffers {
    pub rx: Option<usize>,
    pub tx: Option<usize>,
}

/// One grant: its policy, its buffers, and the sockets held under it.
#[derive(Debug)]
pub struct Grant {
    policy: Policy,
    buffers: Buffers,
    held: Cell<u32>,
}

impl Grant {
    pub fn new(policy: Policy, buffers: Buffers) -> Grant {
        Grant {
            policy,
            buffers,
            held: Cell::new(0),
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn buffers(&self) -> Buffers {
        self.buffers
    }

    /// The sockets held under the grant now.
    pub fn held(&self) -> u32 {
        self.held.get()
    }

    /// Whether one more socket fits the grant's bound.
    pub fn room(&self) -> bool {
        self.held.get() < self.policy.sockets
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_counts_to_its_bound_and_back() {
        let g = Grant::new(
            Policy {
                sockets: 2,
                ..Policy::default()
            },
            Buffers {
                rx: Some(4096),
                tx: None,
            },
        );
        assert_eq!(g.policy().sockets, 2);
        assert_eq!(g.buffers().rx, Some(4096));
        assert!(g.room() && g.take() && g.take());
        assert_eq!(g.held(), 2);
        assert!(!g.room() && !g.take());
        assert_eq!(g.held(), 2);
        g.give(2);
        assert_eq!(g.held(), 0);
        assert!(g.room());
    }
}
