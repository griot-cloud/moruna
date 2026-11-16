// SPDX-License-Identifier: MPL-2.0

//! CPU-related definitions.

mod id;
pub mod local;

pub use id::{AtomicCpuSet, CpuId, CpuIdFromIntError, CpuSet, PinCurrentCpu, all_cpus, num_cpus};

/// The CPU privilege level: user mode or kernel mode.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PrivilegeLevel {
    /// User mode.
    User = 0,
    /// Kernel mode.
    Kernel = 1,
}

/// Initializes the CPU module (the BSP part).
///
/// # Safety
///
/// The caller must ensure that
/// 1. We're in the boot context of the BSP and APs have not yet booted.
/// 2. The number of CPUs is available.
/// 3. CPU-local storage has NOT been used.
pub(crate) unsafe fn init_on_bsp() {
    let num_cpus = crate::arch::boot::smp::count_processors().unwrap_or(1);

    // SAFETY:
    // 1. We're in the boot context of the BSP and APs have not yet booted.
    // 2. The number of CPUs is correct.
    // 3. CPU-local storage has NOT been used.
    unsafe { local::copy_bsp_for_ap(num_cpus as usize) };

    // For this point on, CPU-local storage on all CPUs are safe to use.

    // SAFETY:
    // 1. We're in the boot context of the BSP.
    // 2. The number of CPUs is correct.
    unsafe { id::init_on_bsp(num_cpus) };

    publish_cpu_id(0);
}

/// Initializes the CPU module (the AP part).
///
/// # Safety
///
/// The caller must ensure that:
/// 1. We're in the boot context of an AP.
/// 2. The CPU ID of the AP is correct.
pub(crate) unsafe fn init_on_ap(cpu_id: u32) {
    // SAFETY:
    // 1. We're in the boot context of an AP.
    // 2. The CPU ID of the AP is correct.
    unsafe { id::init_on_ap(cpu_id) };

    publish_cpu_id(cpu_id);
}

/// Writes this CPU's number to its `IA32_TSC_AUX`, which user code reads with `rdtscp` or
/// `rdpid`: a thread learns the CPU it runs on with no system call. A CPU without
/// `rdtscp` is left as it is.
#[cfg(target_arch = "x86_64")]
fn publish_cpu_id(cpu_id: u32) {
    use core::arch::x86_64::__cpuid;

    // CPUID.80000001H:EDX[27] reports RDTSCP and IA32_TSC_AUX.
    if __cpuid(0x8000_0000).eax < 0x8000_0001 || __cpuid(0x8000_0001).edx & (1 << 27) == 0 {
        return;
    }
    // SAFETY: The CPU has IA32_TSC_AUX (checked above), which only `rdtscp` and `rdpid`
    // read; no memory safety depends on it.
    unsafe { x86::msr::wrmsr(x86::msr::IA32_TSC_AUX, u64::from(cpu_id)) };
}

#[cfg(not(target_arch = "x86_64"))]
fn publish_cpu_id(_cpu_id: u32) {}
