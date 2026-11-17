// SPDX-License-Identifier: MPL-2.0

//! moruna: the hypervisor at boot (Moruna's system design, section 1F, "The core on
//! Hyper-V"; work item 1.8).
//!
//! The boot processor finds the hypervisor, says which on the console (Hyper-V, KVM or
//! another; nothing on bare metal), and on Hyper-V reports the guest OS identity and
//! places the hypercall page, mapped executable in kernel mode only, for the core's VMBus
//! mechanism (item 1.9c), which makes the calls. No hypercall is made here. What is
//! decided, rather than done, is in `cpu/hypervisor.rs`, which the host tests check.

use spin::Once;

use crate::{
    arch::cpu::{
        cpuid,
        hypervisor::{self, Cpu, Hypervisor},
    },
    mm::{
        FrameAllocOptions, HasPaddr, PAGE_SIZE, Vaddr,
        kspace::kvirt_area::KVirtArea,
        page_prop::{CachePolicy, PageFlags, PageProperty, PrivilegedPageFlags},
    },
};

/// The CPU this runs on, as `cpu/hypervisor.rs` reads it.
pub(in crate::arch) struct ThisCpu;

impl Cpu for ThisCpu {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Option<[u32; 4]> {
        cpuid::cpuid(leaf, subleaf).map(|r| [r.eax, r.ebx, r.ecx, r.edx])
    }

    fn rdmsr(&self, msr: u32) -> u64 {
        // SAFETY: `cpu/hypervisor.rs` reads an MSR only once CPUID has said that it is
        // implemented and that this partition may read it, so the read cannot fault, and
        // reading it changes nothing.
        unsafe { x86::msr::rdmsr(msr) }
    }
}

/// The hypercall page's mapping: kernel mode, read and execute, never written. It is
/// kept for as long as the machine runs, and so is the frame under it.
static HYPERCALL_PAGE: Once<KVirtArea> = Once::new();

/// Finds the hypervisor and, on Hyper-V, sets up the hypercall page.
///
/// It runs on the boot processor before the other processors start, so no processor
/// holds a stale translation of the page's new mapping.
pub(in crate::arch) fn init_on_bsp() {
    let found = hypervisor::hypervisor(&ThisCpu);
    if let Some(found) = found {
        crate::early_println!("{}", found.line());
    }
    if found == Some(Hypervisor::HyperV) {
        set_up_hypercall_page();
    }
}

/// Reports the guest OS identity and places the hypercall page (TLFS, "Establishing the
/// Hypercall Interface (x86/x64)").
///
/// The page is a frame of the kernel's own, so nothing else uses the memory the overlay
/// covers (step 6). Anything that stops it is logged, and the boot goes on with no
/// hypercall page.
///
/// Where the TLFS says to write the guest OS ID only if it is zero (step 3) and to keep a
/// hypercall page already enabled (step 5), this does as Linux does for a guest
/// (`arch/x86/hyperv/hv_init.c`): it always writes Moruna's ID, and always moves the page
/// to its own frame. A page firmware left enabled lies in memory the OS reclaims, which
/// the frame allocator may hand out for anything; keeping it there would leave an
/// overlay that faults on every write over memory the kernel believes is its own.
fn set_up_hypercall_page() {
    use hvdef::{HV_X64_MSR_GUEST_OS_ID, HV_X64_MSR_HYPERCALL};
    use x86::msr::{rdmsr, wrmsr};

    if !hypervisor::hypercall_msrs(&ThisCpu) {
        crate::warn!("Hyper-V: this partition may not use the hypercall MSRs");
        return;
    }
    let frame = match FrameAllocOptions::new().zeroed(true).alloc_frame() {
        Ok(frame) => frame,
        Err(err) => {
            crate::warn!("Hyper-V: no frame for the hypercall page: {:?}", err);
            return;
        }
    };
    let page = frame.paddr() as u64;

    // SAFETY: Microsoft's hypervisor interface (`Hv#1`) implements the guest OS ID and
    // hypercall MSRs, and this partition may access them (`hypercall_msrs`), so neither
    // access faults.
    let current = unsafe { rdmsr(HV_X64_MSR_HYPERCALL) };
    let Some(enabled) = hypervisor::hypercall_msr(current, page) else {
        crate::warn!(
            "Hyper-V: the hypercall MSR is locked ({:#x}); no hypercall page",
            current
        );
        return;
    };

    // Step 8, before step 7 as Linux does: the page is mapped where only the kernel may
    // reach it, to be read and executed, never written.
    let area = KVirtArea::map_frames(
        PAGE_SIZE,
        0,
        core::iter::once(frame),
        PageProperty {
            flags: PageFlags::RX,
            cache: CachePolicy::Writeback,
            priv_flags: PrivilegedPageFlags::empty(),
        },
    );

    // SAFETY: as above. Writing the guest OS ID changes only what the hypervisor knows
    // of the guest (step 3), and is done whatever firmware wrote there (see above). The
    // hypercall page overlays a frame the kernel holds for good and never writes, so
    // nothing of the kernel is hidden by it, and the page firmware may have left enabled
    // elsewhere is moved off memory the frame allocator hands out (step 7).
    unsafe {
        wrmsr(HV_X64_MSR_GUEST_OS_ID, hypervisor::GUEST_OS_ID);
        wrmsr(HV_X64_MSR_HYPERCALL, enabled);
    }
    // SAFETY: as above.
    let now = unsafe { rdmsr(HV_X64_MSR_HYPERCALL) };
    if !hvdef::hypercall::MsrHypercallContents::from_bits(now).enable() {
        crate::warn!("Hyper-V: the hypercall page did not enable ({:#x})", now);
        return;
    }
    crate::info!(
        "Hyper-V: guest OS ID {:#x}, hypercall page at {:#x}",
        hypervisor::GUEST_OS_ID,
        page
    );
    HYPERCALL_PAGE.call_once(|| area);
}

/// The kernel address of the hypercall page, once it is set up: a call to it makes a
/// hypercall, at CPL 0 (TLFS, "Legal Hypercall Environments").
#[expect(dead_code, reason = "item 1.9c makes the hypercalls")]
pub(in crate::arch) fn hypercall_page() -> Option<Vaddr> {
    HYPERCALL_PAGE.get().map(KVirtArea::start)
}
