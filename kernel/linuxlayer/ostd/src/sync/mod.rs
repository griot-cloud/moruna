// SPDX-License-Identifier: MPL-2.0

//! Useful synchronization primitives.
//!
//! User space: every lock and wait queue sleeps on the core's futex (`raw.rs`), and RCU's
//! grace periods are reader epochs (`rcu/monitor.rs`).

mod guard;
mod mutex;
pub(crate) mod raw;
mod rcu;
mod rwarc;
mod rwlock;
mod rwmutex;
mod spin;
mod wait;

pub(crate) use self::rcu::{reader_enter, reader_exit};
pub use self::{
    guard::{GuardTransfer, LocalIrqDisabled, PreemptDisabled, SpinGuardian, WriteIrqDisabled},
    mutex::{Mutex, MutexGuard},
    rcu::{Rcu, RcuDrop, RcuOption, RcuOptionReadGuard, RcuReadGuard, non_null, reclaim},
    rwarc::{RoArc, RwArc},
    rwlock::{RwLock, RwLockReadGuard, RwLockUpgradeableGuard, RwLockWriteGuard},
    rwmutex::{RwMutex, RwMutexReadGuard, RwMutexUpgradeableGuard, RwMutexWriteGuard},
    spin::{SpinLock, SpinLockGuard},
    wait::{WaitQueue, Waiter, Waker},
};
