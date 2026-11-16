// SPDX-License-Identifier: MPL-2.0

//! Scheduling: the core's. What is left is the task's CPU record.

pub mod info {
    //! The scheduling information a task keeps.

    use core::sync::atomic::{AtomicU32, Ordering};

    use crate::{cpu::CpuId, task::Task};

    /// The scheduling information of a task.
    #[derive(Debug)]
    pub struct TaskScheduleInfo {
        /// The CPU the task last ran on.
        pub cpu: AtomicCpuId,
    }

    /// An atomic `Option<CpuId>`.
    #[derive(Debug)]
    pub struct AtomicCpuId(AtomicU32);

    impl AtomicCpuId {
        const NONE: u32 = u32::MAX;

        /// Sets the CPU if none is set; returns the one set otherwise.
        pub fn set_if_is_none(&self, cpu_id: CpuId) -> Result<(), CpuId> {
            self.0
                .compare_exchange(
                    Self::NONE,
                    cpu_id.into(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .map(|_| ())
                .map_err(|prev| (prev as usize).try_into().unwrap())
        }

        /// Sets the CPU.
        pub fn set_anyway(&self, cpu_id: CpuId) {
            self.0.store(cpu_id.into(), Ordering::Relaxed);
        }

        /// Clears the CPU.
        pub fn set_to_none(&self) {
            self.0.store(Self::NONE, Ordering::Relaxed);
        }

        /// The CPU, if set.
        pub fn get(&self) -> Option<CpuId> {
            let val = self.0.load(Ordering::Relaxed);
            if val == Self::NONE {
                None
            } else {
                Some((val as usize).try_into().ok()?)
            }
        }
    }

    impl Default for AtomicCpuId {
        fn default() -> Self {
            Self(AtomicU32::new(Self::NONE))
        }
    }

    impl CommonSchedInfo for Task {
        fn cpu(&self) -> &AtomicCpuId {
            &self.schedule_info().cpu
        }
    }

    /// The scheduling information every task has.
    pub trait CommonSchedInfo {
        /// The CPU the task last ran on.
        fn cpu(&self) -> &AtomicCpuId;
    }
}
