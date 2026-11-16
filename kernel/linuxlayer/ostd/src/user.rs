// SPDX-License-Identifier: MPL-2.0

//! Running a program thread.
//!
//! In OSTD this enters user mode on the kernel's CPU. Here the thread is already the
//! program's own: `execute` hands its registers back through the personality entry and
//! returns with the next entry's (`kernel-is-the-machine.md` §4.1 a). The entry is
//! stream B1's mechanism, so it is reached through a driver installed at start:
//! the core's entry on the target, a scripted program in host tests.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::{arch::cpu::context::UserContext, irq::DisabledLocalIrqGuard};

pub(crate) trait UserContextApiInternal {
    /// Runs the program until it enters the personality.
    fn execute<T: UserModeHooks>(&mut self, hooks: &T) -> ReturnReason;
}

/// Reading and writing a program context's instruction and stack pointers.
pub trait UserContextApi {
    /// Sets the instruction pointer.
    fn set_instruction_pointer(&mut self, ip: usize);

    /// The instruction pointer.
    fn instruction_pointer(&self) -> usize;

    /// Sets the stack pointer.
    fn set_stack_pointer(&mut self, sp: usize);

    /// The stack pointer.
    fn stack_pointer(&self) -> usize;
}

/// A program thread's run: its context, handed to the program and back.
pub struct UserMode {
    context: UserContext,
}

impl !Send for UserMode {}

impl UserMode {
    /// Makes a run with `context`.
    pub fn new(context: UserContext) -> Self {
        Self { context }
    }

    /// Runs the program until it makes a call, raises an exception, or an event for the
    /// personality is pending.
    #[track_caller]
    pub fn execute<T: UserModeHooks>(&mut self, hooks: &T) -> ReturnReason {
        UserContextApiInternal::execute(&mut self.context, hooks)
    }

    /// The context.
    pub fn context(&self) -> &UserContext {
        &self.context
    }

    /// The context, to change.
    pub fn context_mut(&mut self) -> &mut UserContext {
        &mut self.context
    }
}

/// Why `execute` returned.
#[derive(Debug, Eq, PartialEq)]
pub enum ReturnReason {
    /// The program made a system call.
    UserSyscall,
    /// The program raised an exception.
    UserException,
    /// An event for the personality, such as an interruption.
    KernelEvent,
}

/// Hooks around running the program.
pub trait UserModeHooks {
    /// Whether an event for the personality is pending.
    fn has_kernel_event(&self) -> bool {
        false
    }

    /// Called before the program runs.
    fn pre_user_run(&self, _guard: &DisabledLocalIrqGuard) {}
}

/// No hooks.
pub struct DummyUserHooks;

impl UserModeHooks for DummyUserHooks {}

/// How a thread runs its program: it gives the context to the program and takes back the
/// next entry's, and says why the program entered.
pub type ProgramDriver = fn(&mut UserContext) -> ReturnReason;

static DRIVER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Installs the driver every thread runs its program with.
pub fn set_program_driver(driver: ProgramDriver) {
    DRIVER.store(driver as *mut (), Ordering::Release);
}

pub(crate) fn run_program(context: &mut UserContext) -> ReturnReason {
    let ptr = DRIVER.load(Ordering::Acquire);
    assert!(
        !ptr.is_null(),
        "no program driver: the personality entry is not set"
    );
    // SAFETY: only `set_program_driver` stores here, and it stores a `ProgramDriver`.
    let driver: ProgramDriver = unsafe { core::mem::transmute(ptr) };
    driver(context)
}
