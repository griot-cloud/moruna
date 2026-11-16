// SPDX-License-Identifier: MPL-2.0

//! A program thread's registers, as the personality sees them.
//!
//! The context is the personality entry's frame (`moruna_abi::personality`, frozen in ABI
//! 2.6): the general registers, the instruction pointer, the flags, the FS and GS bases,
//! and for an exception its vector, error code and faulting address. Running the program
//! (`UserMode::execute`) hands the frame back and takes the next one, through the entry
//! driver (`crate::user`).

use alloc::boxed::Box;

use bitflags::bitflags;
use ostd_pod::{FromZeros, IntoBytes};

use crate::{
    mm::Vaddr,
    user::{ReturnReason, UserContextApi, UserContextApiInternal, UserModeHooks},
};

/// The flags a program may change (OSTD's mask: CF PF AF ZF SF TF DF OF RF AC NT).
pub const USER_MODIFIABLE_RFLAGS: usize =
    0x1 | 0x4 | 0x10 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x1_0000 | 0x4_0000 | 0x4000;

/// The flags a program starts with: the reserved bit, IF and ID.
const USER_MODE_INITIAL_RFLAGS: usize = (1 << 1) | (1 << 9) | (1 << 21);

/// The registers of a program thread.
#[derive(Clone, Debug)]
pub struct UserContext {
    general: GeneralRegs,
    rflags: usize,
    fs_base: usize,
    gs_base: usize,
    exception: Option<CpuException>,
}

impl Default for UserContext {
    fn default() -> Self {
        Self {
            general: GeneralRegs::default(),
            rflags: USER_MODE_INITIAL_RFLAGS,
            fs_base: 0,
            gs_base: 0,
            exception: None,
        }
    }
}

/// The general registers, and the instruction pointer.
#[expect(missing_docs)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GeneralRegs {
    pub rax: usize,
    pub rbx: usize,
    pub rcx: usize,
    pub rdx: usize,
    pub rsi: usize,
    pub rdi: usize,
    pub rbp: usize,
    pub rsp: usize,
    pub r8: usize,
    pub r9: usize,
    pub r10: usize,
    pub r11: usize,
    pub r12: usize,
    pub r13: usize,
    pub r14: usize,
    pub r15: usize,
    pub rip: usize,
}

/// A thread's FS base.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsBase(usize);

impl FsBase {
    /// Makes a value.
    pub fn new(addr: usize) -> Self {
        Self(addr)
    }

    /// The address.
    pub fn addr(&self) -> usize {
        self.0
    }

    /// Reads the program's FS base: in user space it lives in the thread's block, which the
    /// entry hands to the program and takes back.
    pub fn save(&mut self) {
        self.0 = crate::backend::program_fs_base();
    }

    /// Sets the program's FS base, for its next run.
    pub fn load(&self) {
        crate::backend::set_program_fs_base(self.0);
    }
}

/// A thread's GS base. The core keeps no GS base per thread, so the program's is kept in
/// the thread's block and reported, but not loaded into the CPU.
#[derive(Clone, Copy, Debug, Default)]
pub struct GsBase(usize);

impl GsBase {
    /// Makes a value.
    pub fn new(addr: usize) -> Self {
        Self(addr)
    }

    /// The address.
    pub fn addr(&self) -> usize {
        self.0
    }

    /// Reads the program's GS base.
    pub fn save(&mut self, _guard: &crate::irq::DisabledLocalIrqGuard) {
        self.0 = crate::backend::program_gs_base();
    }

    /// Sets the program's GS base.
    pub fn load(&self, _guard: &crate::irq::DisabledLocalIrqGuard) {
        crate::backend::set_program_gs_base(self.0);
    }
}

/// A CPU exception a program raised.
#[expect(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuException {
    DivisionError,
    Debug,
    NonMaskableInterrupt,
    BreakPoint,
    Overflow,
    BoundRangeExceeded,
    InvalidOpcode,
    DeviceNotAvailable,
    DoubleFault,
    CoprocessorSegmentOverrun,
    InvalidTss(SelectorErrorCode),
    SegmentNotPresent(SelectorErrorCode),
    StackSegmentFault(SelectorErrorCode),
    GeneralProtectionFault(Option<SelectorErrorCode>),
    PageFault(RawPageFaultInfo),
    X87FloatingPointException,
    AlignmentCheck,
    MachineCheck,
    SIMDFloatingPointException,
    VirtualizationException,
    ControlProtectionException,
    HypervisorInjectionException,
    VMMCommunicationException,
    SecurityException,
    Reserved,
}

impl CpuException {
    /// The exception for `vector`, with its error code and, for a page fault, its address.
    pub fn new(vector: usize, error_code: usize, fault_addr: usize) -> Option<Self> {
        let exception = match vector {
            0 => Self::DivisionError,
            1 => Self::Debug,
            2 => Self::NonMaskableInterrupt,
            3 => Self::BreakPoint,
            4 => Self::Overflow,
            5 => Self::BoundRangeExceeded,
            6 => Self::InvalidOpcode,
            7 => Self::DeviceNotAvailable,
            8 => Self::DoubleFault,
            9 => Self::CoprocessorSegmentOverrun,
            10 => Self::InvalidTss(SelectorErrorCode(error_code)),
            11 => Self::SegmentNotPresent(SelectorErrorCode(error_code)),
            12 => Self::StackSegmentFault(SelectorErrorCode(error_code)),
            13 => Self::GeneralProtectionFault(
                (error_code != 0).then_some(SelectorErrorCode(error_code)),
            ),
            14 => Self::PageFault(RawPageFaultInfo {
                error_code: PageFaultErrorCode::from_bits_truncate(error_code),
                addr: fault_addr,
            }),
            16 => Self::X87FloatingPointException,
            17 => Self::AlignmentCheck,
            18 => Self::MachineCheck,
            19 => Self::SIMDFloatingPointException,
            20 => Self::VirtualizationException,
            21 => Self::ControlProtectionException,
            28 => Self::HypervisorInjectionException,
            29 => Self::VMMCommunicationException,
            30 => Self::SecurityException,
            15 | 22..=27 | 31 => Self::Reserved,
            _ => return None,
        };
        Some(exception)
    }
}

/// A selector error code.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SelectorErrorCode(usize);

/// A page fault's error code and address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawPageFaultInfo {
    /// The error code.
    pub error_code: PageFaultErrorCode,
    /// The faulting address.
    pub addr: Vaddr,
}

bitflags! {
    /// A page fault's error code.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PageFaultErrorCode: usize {
        /// The page was present.
        const PRESENT      = 1 << 0;
        /// A write.
        const WRITE        = 1 << 1;
        /// From user mode.
        const USER         = 1 << 2;
        /// A reserved bit was set.
        const RESERVED     = 1 << 3;
        /// An instruction fetch.
        const INSTRUCTION  = 1 << 4;
        /// A protection-key violation.
        const PROTECTION   = 1 << 5;
        /// A shadow-stack access.
        const SHADOW_STACK = 1 << 6;
        /// HLAT paging.
        const HLAT         = 1 << 7;
        /// An SGX violation.
        const SGX          = 1 << 15;
    }
}

impl UserContext {
    /// The general registers.
    pub fn general_regs(&self) -> &GeneralRegs {
        &self.general
    }

    /// The general registers, to change.
    pub fn general_regs_mut(&mut self) -> &mut GeneralRegs {
        &mut self.general
    }

    /// Takes the exception the last run raised.
    pub fn take_exception(&mut self) -> Option<CpuException> {
        self.exception.take()
    }

    /// The flags.
    pub fn rflags(&self) -> usize {
        self.rflags
    }

    /// Sets the flags the program may change.
    pub fn set_rflags(&mut self, rflags: usize) {
        self.rflags = (self.rflags & !USER_MODIFIABLE_RFLAGS) | (rflags & USER_MODIFIABLE_RFLAGS);
    }

    /// The FS base.
    pub fn fs_base(&self) -> usize {
        self.fs_base
    }

    /// Sets the FS base.
    pub fn set_fs_base(&mut self, fs_base: usize) {
        self.fs_base = fs_base;
    }

    /// The GS base.
    pub fn gs_base(&self) -> usize {
        self.gs_base
    }

    /// Sets the GS base.
    pub fn set_gs_base(&mut self, gs_base: usize) {
        self.gs_base = gs_base;
    }

    /// The context in the entry frame's layout.
    pub fn to_frame(&self, frame: &mut [u8; frame::LEN]) {
        use frame::*;
        let g = &self.general;
        let regs = [
            g.rax, g.rbx, g.rcx, g.rdx, g.rsi, g.rdi, g.rbp, g.rsp, g.r8, g.r9, g.r10, g.r11,
            g.r12, g.r13, g.r14, g.r15,
        ];
        put(frame, REASON, 0);
        for (i, r) in regs.iter().enumerate() {
            put(frame, REGISTERS + 8 * i, *r as u64);
        }
        put(frame, RIP, g.rip as u64);
        put(frame, RFLAGS, self.rflags as u64);
        put(frame, FS_BASE, self.fs_base as u64);
        put(frame, GS_BASE, self.gs_base as u64);
        put(frame, VECTOR, 0);
        put(frame, ERROR_CODE, 0);
        put(frame, FAULT_ADDRESS, 0);
    }

    /// Takes the registers from an entry frame; returns why the program entered.
    pub fn from_frame(&mut self, frame: &[u8; frame::LEN]) -> ReturnReason {
        use frame::*;
        let r = |i: usize| get(frame, REGISTERS + 8 * i) as usize;
        self.general = GeneralRegs {
            rax: r(0),
            rbx: r(1),
            rcx: r(2),
            rdx: r(3),
            rsi: r(4),
            rdi: r(5),
            rbp: r(6),
            rsp: r(7),
            r8: r(8),
            r9: r(9),
            r10: r(10),
            r11: r(11),
            r12: r(12),
            r13: r(13),
            r14: r(14),
            r15: r(15),
            rip: get(frame, RIP) as usize,
        };
        self.rflags = get(frame, RFLAGS) as usize;
        self.fs_base = get(frame, FS_BASE) as usize;
        self.gs_base = get(frame, GS_BASE) as usize;
        match get(frame, REASON) {
            reason::SYSCALL => ReturnReason::UserSyscall,
            reason::PAGE_FAULT | reason::EXCEPTION => {
                self.exception = CpuException::new(
                    get(frame, VECTOR) as usize,
                    get(frame, ERROR_CODE) as usize,
                    get(frame, FAULT_ADDRESS) as usize,
                );
                ReturnReason::UserException
            }
            _ => ReturnReason::KernelEvent,
        }
    }
}

/// The entry frame's layout (`moruna_abi::personality`, ABI 2.6).
pub mod frame {
    pub use moruna_abi::personality::{
        ERROR_CODE, FAULT_ADDRESS, FS_BASE, GS_BASE, LEN, REASON, REGISTERS, RFLAGS, RIP, VECTOR,
        reason,
    };

    pub(super) fn put(frame: &mut [u8; LEN], at: usize, v: u64) {
        frame[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    pub(super) fn get(frame: &[u8; LEN], at: usize) -> u64 {
        u64::from_le_bytes(frame[at..at + 8].try_into().unwrap())
    }
}

impl UserContextApiInternal for UserContext {
    fn execute<T: UserModeHooks>(&mut self, hooks: &T) -> ReturnReason {
        loop {
            {
                let guard = crate::irq::disable_local();
                hooks.pre_user_run(&guard);
            }
            let reason = crate::user::run_program(self);
            if reason != ReturnReason::KernelEvent || hooks.has_kernel_event() {
                return reason;
            }
        }
    }
}

impl UserContextApi for UserContext {
    fn set_instruction_pointer(&mut self, ip: usize) {
        self.set_rip(ip);
    }

    fn set_stack_pointer(&mut self, sp: usize) {
        self.set_rsp(sp)
    }

    fn stack_pointer(&self) -> usize {
        self.rsp()
    }

    fn instruction_pointer(&self) -> usize {
        self.rip()
    }
}

macro_rules! cpu_context_impl_getter_setter {
    ( $( [ $field: ident, $setter_name: ident] ),*) => {
        impl UserContext {
            $(
                #[doc = concat!("Gets the value of ", stringify!($field))]
                #[inline(always)]
                pub fn $field(&self) -> usize {
                    self.general.$field
                }

                #[doc = concat!("Sets the value of ", stringify!($field))]
                #[inline(always)]
                pub fn $setter_name(&mut self, $field: usize) {
                    self.general.$field = $field;
                }
            )*
        }
    };
}

cpu_context_impl_getter_setter!(
    [rax, set_rax],
    [rbx, set_rbx],
    [rcx, set_rcx],
    [rdx, set_rdx],
    [rsi, set_rsi],
    [rdi, set_rdi],
    [rbp, set_rbp],
    [rsp, set_rsp],
    [r8, set_r8],
    [r9, set_r9],
    [r10, set_r10],
    [r11, set_r11],
    [r12, set_r12],
    [r13, set_r13],
    [r14, set_r14],
    [r15, set_r15],
    [rip, set_rip]
);

/// A program thread's x87, SSE and AVX state, in the XSAVE layout signal frames carry.
///
/// The personality runs on the program's own thread, so the state is the thread's own:
/// `save` and `load` use the unprivileged XSAVE instructions (or FXSAVE without them).
#[derive(Debug)]
pub struct FpuContext {
    xsave_area: Box<XSaveArea>,
    area_size: usize,
}

impl FpuContext {
    /// A fresh state.
    pub fn new() -> Self {
        Self {
            xsave_area: Box::new(XSaveArea::new()),
            area_size: area_size(),
        }
    }

    /// Saves the calling thread's state.
    ///
    /// On the target the program's state is not in the registers while the personality runs:
    /// the entry saved it in the thread's block, which this copies.
    pub fn save(&mut self) {
        #[cfg(target_os = "none")]
        crate::backend::program_fpu_save(self.as_bytes_mut());
        #[cfg(not(target_os = "none"))]
        self.save_registers();
    }

    #[cfg(not(target_os = "none"))]
    fn save_registers(&mut self) {
        let mem_addr = self.as_bytes_mut().as_mut_ptr();
        #[cfg(target_arch = "x86_64")]
        // SAFETY: the area is 64-byte aligned and as large as the CPU's XSAVE area.
        unsafe {
            match xstate_max_features() {
                Some(_) => core::arch::x86_64::_xsave64(mem_addr, XFEATURE_MASK_USER_RESTORE),
                None => core::arch::x86_64::_fxsave64(mem_addr),
            }
        }
    }

    /// Loads the state into the calling thread.
    ///
    /// On the target this sets the state the entry loads when the program next runs.
    pub fn load(&self) {
        #[cfg(target_os = "none")]
        crate::backend::program_fpu_load(self.as_bytes());
        #[cfg(not(target_os = "none"))]
        self.load_registers();
    }

    #[cfg(not(target_os = "none"))]
    fn load_registers(&self) {
        let mem_addr = self.as_bytes().as_ptr();
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as for `save`; the area holds a state XSAVE or FXSAVE wrote, or the
        // initial one.
        unsafe {
            match xstate_max_features() {
                Some(max) => {
                    core::arch::x86_64::_xrstor64(mem_addr, XFEATURE_MASK_USER_RESTORE & max)
                }
                None => core::arch::x86_64::_fxrstor64(mem_addr),
            }
        }
    }

    /// The state's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.xsave_area.as_bytes()[..self.area_size]
    }

    /// The state's bytes, to change.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.xsave_area.as_mut_bytes()[..self.area_size]
    }
}

impl Default for FpuContext {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for FpuContext {
    fn clone(&self) -> Self {
        let mut xsave_area = Box::new(XSaveArea::new());
        xsave_area.as_mut_bytes()[..self.area_size].copy_from_slice(self.as_bytes());
        Self {
            xsave_area,
            area_size: self.area_size,
        }
    }
}

#[repr(C)]
#[repr(align(64))]
#[derive(Clone, Copy, Debug, Pod)]
struct XSaveArea {
    fxsave_area: FxSaveArea,
    features: u64,
    compaction: u64,
    reserved: [u64; 6],
    extended_state_area: [u8; MAX_XSAVE_AREA_SIZE - size_of::<FxSaveArea>() - 64],
}

impl XSaveArea {
    fn new() -> Self {
        let mut area = Self::new_zeroed();
        area.fxsave_area.control = 0x037F;
        area.fxsave_area.tag = 0;
        area.fxsave_area.mxcsr = 0x1F80;
        area.features = xstate_max_features().map_or(0, |max| xcr0() & max);
        area
    }
}

#[repr(C)]
#[repr(align(16))]
#[derive(Clone, Copy, Debug, Pod)]
struct FxSaveArea {
    control: u16,
    status: u16,
    tag: u8,
    reserved1: u8,
    op: u16,
    ip: u32,
    cs: u32,
    dp: u32,
    ds: u32,
    mxcsr: u32,
    mxcsr_mask: u32,
    st_space: [u32; 32],
    xmm_space: [u32; 64],
    reserved2: [u32; 12],
    reserved3: [u32; 12],
}

/// The XSAVE components a program's state holds: x87, SSE, AVX and AVX-512.
pub(crate) const XFEATURE_MASK_USER_RESTORE: u64 = 0b1110_0111;
const MAX_XSAVE_AREA_SIZE: usize = 4096;

fn xstate_max_features() -> Option<u64> {
    super::cpuid::query_xstate_max_features()
}

fn area_size() -> usize {
    let fx = size_of::<FxSaveArea>();
    super::cpuid::query_xsave_area_size()
        .map_or(fx, |n| (n as usize).clamp(fx, MAX_XSAVE_AREA_SIZE))
}

fn xcr0() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: XGETBV with ECX 0 reads XCR0, which is readable in user mode when the
    // CPU has XSAVE, which the caller checked.
    unsafe {
        core::arch::x86_64::_xgetbv(0)
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}
