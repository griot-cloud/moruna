//! How a mapping is filled (SDD 27, f.4): a file's pages in a range, each either a
//! hole (left zero), already in the page cache (copied from it), or read from the
//! layer's device straight into the mapping's memory, in runs of consecutive pages,
//! which the page alignment of every file's data makes possible.

use alloc::vec::Vec;

use crate::cache::{Cache, Disk};
use crate::error::{Error, Result};
use crate::volume::FileMap;

/// Consecutive layer pages read into consecutive pages of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The first page within the mapping.
    pub at: u64,
    pub layer_page: u64,
    pub pages: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Pages of the mapping copied from the cache: where, and which layer page.
    pub copies: Vec<(u64, u64)>,
    /// Pages read from the device.
    pub runs: Vec<Run>,
    /// Pages of holes, left zero.
    pub holes: u64,
}

/// The plan for `count` pages of `file` from file page `first`, no run longer than
/// `max_run` pages.
pub fn plan<D: Disk>(
    cache: &Cache<D>,
    file: &FileMap,
    first: u64,
    count: u64,
    max_run: u64,
) -> Result<Plan> {
    let end = first.checked_add(count).ok_or(Error::BadRange)?;
    if count == 0 || end > file.pages() || max_run == 0 {
        return Err(Error::BadRange);
    }
    let mut plan = Plan::default();
    for page in first..end {
        let at = page - first;
        match file.layer_page(page).ok_or(Error::BadRange)? {
            None => plan.holes += 1,
            Some(lp) if cache.cached(file.layer, lp).is_some() => plan.copies.push((at, lp)),
            Some(lp) => match plan.runs.last_mut() {
                Some(r)
                    if r.at + r.pages == at
                        && r.layer_page + r.pages == lp
                        && r.pages < max_run =>
                {
                    r.pages += 1
                }
                _ => plan.runs.push(Run {
                    at,
                    layer_page: lp,
                    pages: 1,
                }),
            },
        }
    }
    Ok(plan)
}
