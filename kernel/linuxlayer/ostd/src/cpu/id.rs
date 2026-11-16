// SPDX-License-Identifier: MPL-2.0

//! CPU identification numbers.

pub use current::PinCurrentCpu;
pub use set::{AtomicCpuSet, CpuSet};

use crate::util::id_set::Id;

/// The ID of a CPU in the system.
///
/// If converting from/to an integer, the integer must start from 0 and be less
/// than the number of CPUs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuId(u32);

impl CpuId {
    /// Creates a new instance.
    ///
    /// # Panics
    ///
    /// The given number must be smaller than the total number of CPUs
    /// (`ostd::cpu::num_cpus()`).
    pub fn new(raw_id: u32) -> Self {
        assert!(raw_id < num_cpus() as u32);
        // SAFETY: The raw ID is smaller than `num_cpus()`.
        unsafe { Self::new_unchecked(raw_id) }
    }

    /// Returns the CPU ID of the bootstrap processor (BSP).
    ///
    /// The number for the BSP is always zero.
    pub const fn bsp() -> Self {
        // BSP's `CURRENT_CPU` is assigned a value of 0.
        let bsp_raw_cpu_id = 0;
        // SAFETY: There is at least one CPU.
        Self(bsp_raw_cpu_id)
    }
}

impl From<CpuId> for u32 {
    fn from(cpu_id: CpuId) -> Self {
        cpu_id.0
    }
}

/// The error type returned when converting an out-of-range integer to [`CpuId`].
#[derive(Clone, Copy, Debug)]
pub struct CpuIdFromIntError;

impl TryFrom<usize> for CpuId {
    type Error = CpuIdFromIntError;

    fn try_from(raw_id: usize) -> Result<Self, Self::Error> {
        if raw_id < num_cpus() {
            // SAFETY: The raw ID is smaller than `num_cpus()`.
            let new_self = unsafe { CpuId::new_unchecked(raw_id as u32) };
            Ok(new_self)
        } else {
            Err(CpuIdFromIntError)
        }
    }
}

/// Returns the number of CPUs.
pub fn num_cpus() -> usize {
    // User space: the CPUs the core reported at start, at most `MAX_CPUS`.
    let n = NUM_CPUS.load(core::sync::atomic::Ordering::Relaxed);
    if n != 0 {
        return n as usize;
    }
    let n = crate::backend::num_cpus().clamp(1, super::local::MAX_CPUS) as u32;
    NUM_CPUS.store(n, core::sync::atomic::Ordering::Relaxed);
    n as usize
}

static NUM_CPUS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Returns an iterator over all CPUs.
pub fn all_cpus() -> impl Iterator<Item = CpuId> {
    (0..num_cpus()).map(|raw_id| {
        // SAFETY: The raw ID is smaller than `num_cpus()`.
        unsafe { CpuId::new_unchecked(raw_id as u32) }
    })
}

mod set {
    use super::{CpuId, num_cpus};
    use crate::util::id_set::{AtomicIdSet, Id, IdSet};

    /// A set of CPU IDs.
    pub type CpuSet = IdSet<CpuId>;

    /// A set of CPU IDs, with support for concurrent access.
    pub type AtomicCpuSet = AtomicIdSet<CpuId>;

    // SAFETY: `CpuId`s and the integers within 0 to `num_cpus` (exclusive)
    // have 1:1 mapping.
    unsafe impl Id for CpuId {
        unsafe fn new_unchecked(raw_id: u32) -> Self {
            Self(raw_id)
        }

        fn cardinality() -> u32 {
            num_cpus() as u32
        }
    }
}

mod current {
    //! The current CPU ID.

    use super::CpuId;
    use crate::{task::atomic_mode::InAtomicMode, util::id_set::Id};

    /// A marker trait for guard types that can "pin" the current task to the
    /// current CPU.
    ///
    /// Such guard types include [`DisabledLocalIrqGuard`] and
    /// [`DisabledPreemptGuard`]. When such guards exist, the CPU executing the
    /// current task is pinned. So getting the current CPU ID or CPU-local
    /// variables are safe.
    ///
    /// # Safety
    ///
    /// The implementor must ensure that the current task is pinned to the current
    /// CPU while any one of the instances of the implemented structure exists.
    ///
    /// [`DisabledLocalIrqGuard`]: crate::irq::DisabledLocalIrqGuard
    /// [`DisabledPreemptGuard`]: crate::task::DisabledPreemptGuard
    pub unsafe trait PinCurrentCpu {
        /// Returns the ID of the current CPU.
        fn current_cpu(&self) -> CpuId {
            CpuId::current_racy()
        }
    }

    // SAFETY: A guard that enforces the atomic mode requires disabling any
    // context switching. So naturally, the current task is pinned on the CPU.
    unsafe impl<T: InAtomicMode> PinCurrentCpu for T {}
    unsafe impl PinCurrentCpu for dyn InAtomicMode + '_ {}

    impl CpuId {
        /// Returns the ID of the current CPU.
        ///
        /// This function is safe to call, but is vulnerable to races. The returned CPU
        /// ID may be outdated if the task migrates to another CPU.
        ///
        /// To ensure that the CPU ID is up-to-date, do it under any guards that
        /// implement the [`PinCurrentCpu`] trait.
        pub fn current_racy() -> Self {
            // User space: a thread does not know the CPU it runs on, so every thread
            // is on CPU 0 for per-CPU state.
            // SAFETY: there is always a CPU 0.
            unsafe { Self::new_unchecked(0) }
        }
    }
}
