//! Signals (SDD 20, e.3): the bits an object raises, and the threads waiting on them.
//!
//! A signaller is updated from task context and from interrupt context (a timer, an
//! interrupt line), so its lock disables interrupts. A waiter registers its waker,
//! reads the bits, and sleeps; any change of the bits wakes every registered waker,
//! which then reads the bits again.

use alloc::{sync::Arc, vec::Vec};

use moruna_abi::Signals;
use ostd::sync::{LocalIrqDisabled, SpinLock, Waker};

struct State {
    bits: Signals,
    observers: Vec<Arc<Waker>>,
}

pub struct Signaller {
    state: SpinLock<State, LocalIrqDisabled>,
}

impl Signaller {
    pub fn new(initial: Signals) -> Signaller {
        Signaller {
            state: SpinLock::new(State {
                bits: initial,
                observers: Vec::new(),
            }),
        }
    }

    pub fn get(&self) -> Signals {
        self.state.lock().bits
    }

    /// Clears `clear`, then sets `set`, and wakes every observer if the bits changed.
    pub fn update(&self, clear: Signals, set: Signals) {
        let mut state = self.state.lock();
        let bits = state.bits.difference(clear).union(set);
        if bits != state.bits {
            state.bits = bits;
            for waker in &state.observers {
                waker.wake_up();
            }
        }
    }

    /// Registers `waker`, and returns the bits as they are now.
    pub fn observe(&self, waker: &Arc<Waker>) -> Signals {
        let mut state = self.state.lock();
        state.observers.push(waker.clone());
        state.bits
    }

    pub fn unobserve(&self, waker: &Arc<Waker>) {
        self.state
            .lock()
            .observers
            .retain(|w| !Arc::ptr_eq(w, waker));
    }
}
