//! Physical Region Page entries (NVMe Base 2.0, 4.1.1): how a command names the
//! memory a transfer moves. A Moruna buffer is whole pages, each pinned with its own
//! device address, so every entry is a page's address with no offset: one page is
//! PRP1 alone, two are PRP1 and PRP2, more are PRP1 and a list of the rest in a page
//! of the driver's, which PRP2 points at.

use alloc::format;

use moruna_mmio::Region;

use crate::{MAX_TRANSFER_PAGES, NvmeError, PAGE};

/// The PRP1 and PRP2 of a transfer over `pages`, writing the list for three or more
/// into `list` (one page, at device address `list_addr`).
pub fn entries<R: Region + ?Sized>(
    pages: &[u64],
    list: &R,
    list_addr: u64,
) -> Result<(u64, u64), NvmeError> {
    if pages.len() > MAX_TRANSFER_PAGES as usize {
        return Err(NvmeError::Unsupported(format!(
            "a transfer of {} pages",
            pages.len()
        )));
    }
    if let Some(p) = pages.iter().find(|p| !p.is_multiple_of(PAGE as u64)) {
        return Err(NvmeError::Unsupported(format!(
            "a buffer page at {p:#x}, not on a page boundary"
        )));
    }
    match pages {
        [] => Err(NvmeError::Unsupported(alloc::string::String::from(
            "a transfer of no pages",
        ))),
        [one] => Ok((*one, 0)),
        [first, second] => Ok((*first, *second)),
        [first, rest @ ..] => {
            if list.len() < rest.len() * 8 || !list_addr.is_multiple_of(PAGE as u64) {
                return Err(NvmeError::Unsupported(format!(
                    "a PRP list of {} entries in {} bytes at {list_addr:#x}",
                    rest.len(),
                    list.len()
                )));
            }
            for (i, page) in rest.iter().enumerate() {
                list.write64(i * 8, *page)?;
            }
            Ok((*first, list_addr))
        }
    }
}
