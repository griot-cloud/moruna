// SPDX-License-Identifier: MPL-2.0

//! Faults the personality takes on the program's memory.
//!
//! A copy of the program's memory that faults on a page the personality has not mapped yet
//! asks the layer's handler (`inject_user_page_fault_handler`, Asterinas's
//! `page_fault_handler`) to map it, then goes on, as OSTD's fault path does in the kernel.

use spin::Once;

use super::Vaddr;
use crate::arch::cpu::context::{CpuException, PageFaultErrorCode, RawPageFaultInfo};

/// Handles a fault on the program's memory: `Ok` when it was resolved.
pub type UserPageFaultHandler = fn(&CpuException) -> Result<(), ()>;

static USER_PAGE_FAULT_HANDLER: Once<UserPageFaultHandler> = Once::new();

/// Injects the handler; once.
pub fn inject_user_page_fault_handler(handler: UserPageFaultHandler) {
    USER_PAGE_FAULT_HANDLER.call_once(|| handler);
}

/// Asks the handler to resolve a fault at `addr` of the program's memory, a write when
/// `write`; true when it did.
pub(crate) fn resolve(addr: Vaddr, write: bool) -> bool {
    let Some(handler) = USER_PAGE_FAULT_HANDLER.get() else {
        return false;
    };
    let mut error_code = PageFaultErrorCode::empty();
    if write {
        error_code |= PageFaultErrorCode::WRITE;
    }
    handler(&CpuException::PageFault(RawPageFaultInfo {
        error_code,
        addr,
    }))
    .is_ok()
}
