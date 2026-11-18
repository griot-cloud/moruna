// SPDX-License-Identifier: MPL-2.0

//! moruna: the hypervisor at boot (Moruna's system design, section 1F, "The core on
//! Hyper-V"; work item 1.8), and Hyper-V's mechanism for VMBus ("VMBus, split by the
//! microkernel rule"; work item 1.9c).
//!
//! The boot processor finds the hypervisor, says which on the console (Hyper-V, KVM or
//! another; nothing on bare metal), and on Hyper-V reports the guest OS identity and
//! places the hypercall page, mapped executable in kernel mode only. It then enables CPU
//! 0's SynIC with SINT 2, and only that, at a vector of an IRQ line of its own: [`synic`]
//! is what the core's `hyperv_*` calls reach, the two hypercalls of VMBus
//! (`HvCallPostMessage`, `HvCallSignalEvent`), SINT 2's message slot, its event flags and
//! its interrupt. What is decided, rather than done, is in `cpu/hypervisor.rs`, which the
//! host tests check.

use core::{
    arch::asm,
    sync::atomic::{Ordering, fence},
};

use hvdef::HV_MESSAGE_SIZE;
use spin::Once;

use crate::{
    arch::cpu::{
        cpuid,
        hypervisor::{self, Cpu, Hypervisor},
    },
    cpu::{CpuId, CpuSet},
    irq::IrqLine,
    mm::{
        Frame, FrameAllocOptions, HasPaddr, PAGE_SIZE, UFrame, Vaddr, VmIo, VmIoOnce,
        kspace::kvirt_area::KVirtArea,
        page_prop::{CachePolicy, PageFlags, PageProperty, PrivilegedPageFlags},
    },
    smp::inter_processor_call,
    sync::SpinLock,
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
    if found == Some(Hypervisor::HyperV) && set_up_hypercall_page() {
        set_up_synic();
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
///
/// Returns whether the page is in place.
fn set_up_hypercall_page() -> bool {
    use hvdef::{HV_X64_MSR_GUEST_OS_ID, HV_X64_MSR_HYPERCALL};
    use x86::msr::{rdmsr, wrmsr};

    if !hypervisor::hypercall_msrs(&ThisCpu) {
        crate::warn!("Hyper-V: this partition may not use the hypercall MSRs");
        return false;
    }
    let frame = match FrameAllocOptions::new().zeroed(true).alloc_frame() {
        Ok(frame) => frame,
        Err(err) => {
            crate::warn!("Hyper-V: no frame for the hypercall page: {:?}", err);
            return false;
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
        return false;
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
        return false;
    }
    crate::info!(
        "Hyper-V: guest OS ID {:#x}, hypercall page at {:#x}",
        hypervisor::GUEST_OS_ID,
        page
    );
    HYPERCALL_PAGE.call_once(|| area);
    true
}

/// The kernel address of the hypercall page, once it is set up: a call to it makes a
/// hypercall, at CPL 0 (TLFS, "Legal Hypercall Environments").
pub(in crate::arch) fn hypercall_page() -> Option<Vaddr> {
    HYPERCALL_PAGE.get().map(KVirtArea::start)
}

/// Makes a hypercall through the hypercall page, with `control` in RCX, `input` in RDX
/// and `output` in R8, and returns its status (TLFS, "Hypercall Calling Conventions").
///
/// It is called only once the SynIC is enabled, which needs the hypercall page.
fn hypercall(control: u64, input: u64, output: u64) -> u16 {
    let Some(page) = hypercall_page() else {
        // There is no hypercall without the page.
        return hvdef::HvStatus::from(Err(hvdef::HvError::InvalidHypercallCode)).0;
    };
    let result: u64;
    // SAFETY: the hypercall page holds the code Hyper-V wrote for the call, mapped
    // executable in kernel mode, and the call keeps its convention: the control value in
    // RCX, the input in RDX and R8 (a slow call's input, a page of the kernel's own that
    // the hypervisor reads; a fast call's, values), the result in RAX, and RCX, RDX and R8
    // to R11 changed. None of the calls made here has output for the hypervisor to write.
    unsafe {
        asm!(
            "call {page}",
            page = in(reg) page,
            inout("rcx") control => _,
            inout("rdx") input => _,
            inout("r8") output => _,
            out("rax") result,
            out("r9") _,
            out("r10") _,
            out("r11") _,
        );
    }
    hypervisor::hypercall_status(result)
}

/// CPU 0's SynIC, enabled with SINT 2 alone (system design 1F, "The SynIC is enabled on
/// CPU 0 only, and every channel targets it"), and the two hypercalls VMBus makes.
///
/// Its pages are frames the kernel holds for as long as the machine runs: the
/// hypervisor's message page and event-flags page overlay them, and the input of
/// `HvCallPostMessage` is written to the third.
pub struct Synic {
    /// The SynIC message page (SIMP): a 256-byte slot a SINT.
    messages: Frame<()>,
    /// The SynIC event-flags page (SIEFP), which the core maps into the `vmbus` process.
    events: UFrame,
    /// The IRQ line at whose vector SINT 2 is delivered.
    line: IrqLine,
    /// The page `HvCallPostMessage` reads its input from, one post at a time.
    input: SpinLock<Frame<()>>,
    /// Held while a message is taken, so two takers never empty the slot twice.
    taking: SpinLock<()>,
    /// CPU 0's VP index, read when the SynIC was enabled.
    vp_index: u32,
}

static SYNIC: Once<Synic> = Once::new();

/// CPU 0's SynIC, once it is enabled: `None` off Hyper-V, or on a Hyper-V partition that
/// may not use the SynIC or post and signal (`cpu/hypervisor.rs`, `synic_msrs`).
pub fn synic() -> Option<&'static Synic> {
    SYNIC.get()
}

/// Enables CPU 0's SynIC: its message and event-flags pages, SINT 2 at an IRQ line's
/// vector, then the SynIC itself, in the order the TLFS gives ("SynIC Enable/Disable")
/// and Linux keeps (`drivers/hv/hv.c`, `hv_synic_enable_regs`). It runs on the boot
/// processor, CPU 0, before the other processors start. Anything that stops it is
/// logged, and the boot goes on without VMBus.
fn set_up_synic() {
    use hvdef::{HV_X64_MSR_SCONTROL, HV_X64_MSR_SIEFP, HV_X64_MSR_SIMP};
    use x86::msr::{rdmsr, wrmsr};

    if !hypervisor::synic_msrs(&ThisCpu) {
        crate::warn!("Hyper-V: this partition may not use the SynIC, post or signal");
        return;
    }
    // The VP VMBus is to target is CPU 0's, whose number Hyper-V gives, not CPU 0's own.
    let Some(vp_index) = hypervisor::vp_index(&ThisCpu) else {
        crate::warn!("Hyper-V: this partition may not read CPU 0's VP index");
        return;
    };
    let frame = || FrameAllocOptions::new().zeroed(true).alloc_frame();
    let (Ok(messages), Ok(events), Ok(input)) = (frame(), frame(), frame()) else {
        crate::warn!("Hyper-V: no frames for the SynIC");
        return;
    };
    let Ok(line) = IrqLine::alloc() else {
        crate::warn!("Hyper-V: no IRQ line for SINT 2");
        return;
    };
    // SAFETY: Microsoft's hypervisor interface implements the SynIC MSRs and this
    // partition may access them (`synic_msrs`), so the reads cannot fault.
    let (simp, siefp) = unsafe { (rdmsr(HV_X64_MSR_SIMP), rdmsr(HV_X64_MSR_SIEFP)) };
    let (Some(simp), Some(siefp)) = (
        hypervisor::synic_page_msr(simp, messages.paddr() as u64),
        hypervisor::synic_page_msr(siefp, events.paddr() as u64),
    ) else {
        crate::warn!("Hyper-V: the SynIC pages cannot be placed");
        return;
    };
    // SAFETY: as above. The message and event-flags pages overlay two frames the kernel
    // holds for good and reaches only through `Synic`, so the overlays hide nothing of
    // the kernel; pages firmware may have left enabled are moved off memory the frame
    // allocator hands out, as the hypercall page is. SINT 2 is delivered at the vector of
    // an IRQ line the kernel holds for good, acknowledged at the local APIC as every IRQ
    // line is; no other SINT is written, as Linux writes none. This runs on CPU 0, so it
    // is CPU 0's SynIC.
    unsafe {
        wrmsr(HV_X64_MSR_SIMP, simp);
        wrmsr(HV_X64_MSR_SIEFP, siefp);
        let sint = rdmsr(hypervisor::VMBUS_SINT_MSR);
        wrmsr(
            hypervisor::VMBUS_SINT_MSR,
            hypervisor::sint_msr(sint, line.num()),
        );
        let scontrol = rdmsr(HV_X64_MSR_SCONTROL);
        wrmsr(HV_X64_MSR_SCONTROL, hypervisor::scontrol_msr(scontrol));
    }
    crate::info!(
        "Hyper-V: SynIC on CPU 0 (VP {}), SINT {} at vector {}",
        vp_index,
        hypervisor::VMBUS_SINT,
        line.num()
    );
    SYNIC.call_once(|| Synic {
        messages,
        events: events.into(),
        line,
        input: SpinLock::new(input),
        taking: SpinLock::new(()),
        vp_index,
    });
}

/// Writes `HV_X64_MSR_EOM`: the hypervisor then delivers the next message waiting for
/// SINT 2's empty slot.
fn write_end_of_message() {
    // SAFETY: the SynIC MSRs are implemented and this partition may write them
    // (`synic_msrs`); `end_of_message` has this run on CPU 0, whose SynIC is enabled.
    // Writing EOM asks the hypervisor for the next message and changes nothing of the
    // kernel.
    unsafe { x86::msr::wrmsr(hvdef::HV_X64_MSR_EOM, 0) };
}

/// Writes end-of-message on CPU 0, whose SynIC the messages arrive through: at once on
/// CPU 0, and in an inter-processor call from any other, which this does not wait for.
fn end_of_message() {
    let mut cpu_0 = CpuSet::new_empty();
    cpu_0.add(CpuId::bsp());
    let _ = inter_processor_call(&cpu_0, write_end_of_message);
}

impl Synic {
    /// `HvCallPostMessage`: posts `payload` as a message of `message_type` to
    /// `connection_id`. Returns Hyper-V's status (`HV_STATUS_INSUFFICIENT_BUFFERS` when it
    /// has no room for the message now), or `None` for a payload over 240 bytes, which is
    /// not posted.
    pub fn post_message(
        &self,
        connection_id: u32,
        message_type: u32,
        payload: &[u8],
    ) -> Option<u16> {
        let input = hypervisor::post_message_input(connection_id, message_type, payload)?;
        let page = self.input.lock();
        page.write_bytes(0, &input).ok()?;
        Some(hypercall(hypervisor::POST_MESSAGE, page.paddr() as u64, 0))
    }

    /// `HvCallSignalEvent`: sets event flag `flag` of `connection_id`'s port and
    /// interrupts its partition. Returns Hyper-V's status.
    pub fn signal_event(&self, connection_id: u32, flag: u16) -> u16 {
        hypercall(
            hypervisor::SIGNAL_EVENT,
            hypervisor::signal_event_input(connection_id, flag),
            0,
        )
    }

    /// Copies the message waiting in SINT 2's slot to `out`, as Hyper-V wrote it, and
    /// empties the slot, writing end-of-message when another message waits behind it.
    /// Returns false, and copies nothing, when the slot is empty. It never waits.
    pub fn take_message(&self, out: &mut [u8; HV_MESSAGE_SIZE]) -> bool {
        let _taking = self.taking.lock();
        let slot = hypervisor::MESSAGE_SLOT;
        let typ: u32 = self
            .messages
            .read_once(slot + hypervisor::MESSAGE_TYPE)
            .unwrap_or_default();
        if !hypervisor::message_present(typ) || self.messages.read_bytes(slot, out).is_err() {
            return false;
        }
        // The slot is emptied, then its flags read again: a message that came to wait
        // behind it meanwhile set them, and is delivered once EOM is written (TLFS,
        // "Message Buffers"; Linux's `vmbus_signal_eom`).
        let _ = self
            .messages
            .write_once(slot + hypervisor::MESSAGE_TYPE, &0u32);
        fence(Ordering::SeqCst);
        let flags: u8 = self
            .messages
            .read_once(slot + hypervisor::MESSAGE_FLAGS)
            .unwrap_or_default();
        if hypervisor::message_pending(flags) {
            end_of_message();
        }
        true
    }

    /// CPU 0's event-flags page, whose bytes from `EVENT_FLAGS_OFFSET` are SINT 2's flags.
    pub fn event_flags(&self) -> UFrame {
        self.events.clone()
    }

    /// Where SINT 2's event flags lie in [`event_flags`](Self::event_flags)'s page.
    pub const EVENT_FLAGS_OFFSET: usize = hypervisor::EVENT_FLAGS;

    /// CPU 0's VP index (`HV_X64_MSR_VP_INDEX`, read when the SynIC was enabled): the VP
    /// that SINT 2's messages and interrupts arrive on, and so the target VMBus names for
    /// its contact and for every channel.
    pub fn vp_index(&self) -> u32 {
        self.vp_index
    }

    /// The IRQ line SINT 2 is delivered at: a callback registered on it runs on CPU 0
    /// for each message or event flag Hyper-V gives the SINT.
    pub fn interrupt(&self) -> IrqLine {
        self.line.clone()
    }
}
