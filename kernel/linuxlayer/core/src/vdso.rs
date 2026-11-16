// SPDX-License-Identifier: MPL-2.0

//! The vDSO, in user space: none is mapped (data-os.md §4). A program finds no
//! `AT_SYSINFO_EHDR` and makes the calls instead, which enter the personality. The names
//! the loader and `/proc/[pid]/maps` use stay, so their files carry unchanged.

use crate::{prelude::*, vm::page_cache::Vmo};

/// Where the vDSO's parts would lie in its object.
pub(crate) struct VdsoVmoLayout {
    pub data_segment_offset: usize,
    pub data_segment_size: usize,
    pub text_segment_offset: usize,
    pub text_segment_size: usize,
    pub size: usize,
}

/// Linux's x86-64 layout, kept for the names.
pub(crate) const VDSO_VMO_LAYOUT: VdsoVmoLayout = VdsoVmoLayout {
    data_segment_offset: 0,
    data_segment_size: PAGE_SIZE,
    text_segment_offset: 4 * PAGE_SIZE,
    text_segment_size: PAGE_SIZE,
    size: 5 * PAGE_SIZE,
};

/// The vDSO's object: none.
pub(crate) fn vdso_vmo() -> Option<Arc<Vmo>> {
    None
}
