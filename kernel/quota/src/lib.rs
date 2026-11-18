//! The Moruna microkernel's memory accounting (SDD 20, f.18), the part of it that is
//! arithmetic and so is tested on the host.
//!
//! Every page a process causes to exist is charged to an account. An account has a
//! limit; what is charged to it, and what it holds for the accounts carved out of it
//! (their limits, whole), never pass that limit together, so the limits of an
//! account's children never sum past its own. [`Ledger`] is one account's figures.
//!
//! An address space's page tables are charged as well. OSTD makes a table only where a
//! page is mapped, and drops one only when an unmap covers the whole region the table
//! maps, so the kernel charges a table page for every region an area touches and, when
//! no area touches a region any more, unmaps the whole region so that its table goes
//! ([`Tables`]). The root table and the three tables over the top of user space, which
//! no unmap inside user space can cover, are the space's fixed charge
//! (`moruna_abi::charge::SPACE`).
//!
//! An account is charged CPU time as well (ABI 2.10, item 6.3i): what the threads of the
//! processes charged to it have used, and what every account carved out of it was charged
//! ([`CpuTime`], [`charge_cpu`]). `account_cpu` reads it ([`cpu_answer`]).

#![no_std]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use moruna_abi::charge;

/// An account's figures, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub limit: u64,
    /// Charged to it now.
    pub used: u64,
    /// The most ever charged to it at once.
    pub peak: u64,
    /// Held for the accounts carved out of it: the sum of their limits.
    pub reserved: u64,
}

impl Usage {
    /// What can still be charged to it, or carved out of it.
    pub fn room(&self) -> u64 {
        self.limit
            .saturating_sub(self.used)
            .saturating_sub(self.reserved)
    }
}

/// A charge or a reservation the account had no room for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exceeded {
    /// What the call asked for.
    pub asked: u64,
    /// The account as it was, unchanged.
    pub usage: Usage,
}

/// More given back than was charged or held: a broken invariant of the kernel's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Underflow {
    pub returned: u64,
    pub held: u64,
}

/// One account's figures: what is charged to it, what it holds for its children, and
/// the most it ever had charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ledger {
    limit: u64,
    used: u64,
    peak: u64,
    reserved: u64,
}

impl Ledger {
    /// An account with nothing charged and a ceiling of `limit` bytes.
    pub const fn new(limit: u64) -> Ledger {
        Ledger {
            limit,
            used: 0,
            peak: 0,
            reserved: 0,
        }
    }

    pub fn usage(&self) -> Usage {
        Usage {
            limit: self.limit,
            used: self.used,
            peak: self.peak,
            reserved: self.reserved,
        }
    }

    /// Whether `bytes` more fit beside what is charged and what is held.
    fn fits(&self, bytes: u64) -> bool {
        self.used
            .checked_add(self.reserved)
            .and_then(|v| v.checked_add(bytes))
            .is_some_and(|total| total <= self.limit)
    }

    /// Charges `bytes`; refused, with nothing changed, when they do not fit.
    pub fn charge(&mut self, bytes: u64) -> Result<(), Exceeded> {
        if !self.fits(bytes) {
            return Err(Exceeded {
                asked: bytes,
                usage: self.usage(),
            });
        }
        self.used += bytes;
        self.peak = self.peak.max(self.used);
        Ok(())
    }

    /// Gives back `bytes` that were charged.
    pub fn uncharge(&mut self, bytes: u64) -> Result<(), Underflow> {
        self.used = self.used.checked_sub(bytes).ok_or(Underflow {
            returned: bytes,
            held: self.used,
        })?;
        Ok(())
    }

    /// Holds `bytes` for a child account's limit; refused, with nothing changed, when
    /// they do not fit.
    pub fn reserve(&mut self, bytes: u64) -> Result<(), Exceeded> {
        if !self.fits(bytes) {
            return Err(Exceeded {
                asked: bytes,
                usage: self.usage(),
            });
        }
        self.reserved += bytes;
        Ok(())
    }

    /// Gives back a child account's limit when the child is gone.
    pub fn unreserve(&mut self, bytes: u64) -> Result<(), Underflow> {
        self.reserved = self.reserved.checked_sub(bytes).ok_or(Underflow {
            returned: bytes,
            held: self.reserved,
        })?;
        Ok(())
    }
}

/// The page tables a change of an address space's areas makes or frees.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tables {
    /// Table pages: each a region no area touched before and one touches now (a map),
    /// or one an area touched before and none touches now (an unmap).
    pub pages: u64,
    /// For an unmap, at each level of `charge::TABLE_REGIONS`, the smallest range that
    /// covers every region at that level no area touches any more: the kernel unmaps it
    /// whole, so OSTD drops the tables under it. Nothing is mapped in it.
    pub reclaim: [Option<(u64, u64)>; 3],
}

impl Tables {
    pub fn bytes(&self) -> u64 {
        self.pages * charge::PAGE
    }
}

/// The regions of `size` bytes that `start .. end` touches, by index.
fn regions(start: u64, end: u64, size: u64) -> (u64, u64) {
    (start / size, (end - 1) / size)
}

/// The table pages of the regions `pieces` touch that neither the area ending at
/// `before` (the nearest one below the pieces) nor the one starting at `after` (the
/// nearest above them) touches, leaving out the topmost region of each level (the
/// space's fixed charge), with `top` the end of user space.
///
/// For a map, `pieces` is the new area and the neighbours are the areas around it: the
/// result is what the map adds. For an unmap, `pieces` are the areas it removed, in
/// address order, and the neighbours the areas left around them: the result is what it
/// frees. Between the neighbours no other area lies, so no other area can touch the
/// regions counted (SDD 20 f.18).
pub fn tables(pieces: &[(u64, u64)], before: Option<u64>, after: Option<u64>, top: u64) -> Tables {
    let mut out = Tables::default();
    for (level, size) in charge::TABLE_REGIONS.iter().copied().enumerate() {
        let topmost = (top - 1) / size;
        let left = before.filter(|end| *end > 0).map(|end| (end - 1) / size);
        let right = after.map(|start| start / size);
        let mut last_counted: Option<u64> = None;
        let mut hull: Option<(u64, u64)> = None;
        for &(start, end) in pieces {
            if end <= start {
                continue;
            }
            let (first, last) = regions(start, end, size);
            // Regions already counted for an earlier piece are not counted again.
            let from = match last_counted {
                Some(done) if done >= first => done + 1,
                _ => first,
            };
            for region in from..=last {
                if region == topmost || Some(region) == left || Some(region) == right {
                    continue;
                }
                out.pages += 1;
                hull = Some(match hull {
                    None => (region, region),
                    Some((lo, _)) => (lo, region),
                });
            }
            last_counted = Some(last_counted.map_or(last, |done| done.max(last)));
        }
        out.reclaim[level] = hull.map(|(lo, hi)| (lo * size, (hi + 1) * size));
    }
    out
}

/// The process-account root's limit from free physical memory, after retaining
/// the kernel's reserve (one thirty-second, at least 32 MiB). Zero means boot
/// must refuse; a small machine is never assigned a fabricated minimum budget.
pub const fn machine_limit(free: u64) -> u64 {
    let fraction = free / 32;
    let reserve = if fraction > 32 << 20 {
        fraction
    } else {
        32 << 20
    };
    free.saturating_sub(reserve) & !(charge::PAGE - 1)
}

/// Whether a boundary creates an additional area record: only strictly inside
/// an existing area. Mapping boundaries and holes need no new record.
pub const fn split_needed(start: u64, end: u64, at: u64) -> bool {
    start < at && at < end
}

/// How many steps of `charge::HANDLE_SLOTS` a handle table of `slots` slots has been
/// charged for.
pub fn table_steps(slots: usize) -> u64 {
    (slots as u64).div_ceil(charge::HANDLE_SLOTS)
}

/// The CPU time charged to one account (ABI 2.10), in nanoseconds, kernel time included:
/// what the threads of the processes charged to it have used, and what was charged to
/// every account carved out of it. It only grows: a process that has ended and a carved
/// account that is gone stay counted. One atomic, so the scheduler's switch charges it
/// with interrupts off and no lock held.
#[derive(Debug, Default)]
pub struct CpuTime(AtomicU64);

impl CpuTime {
    /// No time charged.
    pub const fn new() -> CpuTime {
        CpuTime(AtomicU64::new(0))
    }

    /// Charges `ns` more.
    pub fn add(&self, ns: u64) {
        self.0.fetch_add(ns, Ordering::Relaxed);
    }

    /// Everything charged so far.
    pub fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// An account as its CPU charge sees it: its own time, and the account its limit was
/// carved out of.
pub trait Carved {
    /// The CPU time charged to it.
    fn cpu(&self) -> &CpuTime;
    /// The account it was carved out of; `None` for the machine's.
    fn parent(&self) -> Option<&Self>;
}

/// Charges `ns` of CPU time a thread used to `account`, the one its process is charged
/// to, and to every account that one was carved out of, up to the machine's: an account's
/// time is its own processes' and its children's, as its limit holds theirs.
pub fn charge_cpu<A: Carved>(account: &A, ns: u64) {
    let mut at = Some(account);
    while let Some(account) = at {
        account.cpu().add(ns);
        at = account.parent();
    }
}

/// Whether `account` is `tree` itself or was carved out of it, at any depth: whether CPU
/// time charged to `account` is charged to `tree` too. The same account, not an equal one.
pub fn within<A: Carved>(account: &A, tree: &A) -> bool {
    let mut at = Some(account);
    while let Some(account) = at {
        if core::ptr::eq(account, tree) {
            return true;
        }
        at = account.parent();
    }
    false
}

/// What `account_cpu` answers for an account charged `total` nanoseconds: with `running`,
/// the slice the calling thread is in and has not been charged yet, when its process is
/// charged within the account (`None` when it is not). Never past `i64::MAX`, which a
/// caller would read as an error.
pub fn cpu_answer(total: u64, running: Option<u64>) -> u64 {
    total
        .saturating_add(running.unwrap_or(0))
        .min(i64::MAX as u64)
}

#[cfg(test)]
extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TOP: u64 = 0x7fff_ffff_f000;

    #[test]
    fn kb5_t8_physical_root_keeps_the_reserve_and_never_overcommits() {
        for free in [0, 1, 32 * MIB, 32 * MIB + 4095] {
            assert_eq!(machine_limit(free), 0);
        }
        for free in [32 * MIB + 4096, GIB, 4 * GIB + 33, u64::MAX] {
            let limit = machine_limit(free);
            assert!(limit > 0 && limit <= free);
            assert_eq!(limit % charge::PAGE, 0);
            assert!(free - limit >= (free / 32).max(32 * MIB));
            let mut root = Ledger::new(limit);
            root.reserve(limit).unwrap();
            assert!(root.reserve(1).is_err());
            assert!(root.charge(1).is_err());
            assert_eq!(root.usage().room(), 0);
            assert_eq!(root.usage().reserved, limit);
        }
    }

    #[test]
    fn kb5_t7_whole_area_freeing_needs_no_headroom_or_artificial_peak() {
        let cost = charge::AREA + charge::ZERO_PAGE + 3 * charge::PAGE;
        let mut account = Ledger::new(cost);
        account.charge(cost).unwrap();
        let split_bytes = [PAGE_START, PAGE_START + charge::PAGE]
            .into_iter()
            .filter(|at| split_needed(PAGE_START, PAGE_START + charge::PAGE, *at))
            .count() as u64
            * charge::AREA;
        assert_eq!(split_bytes, 0);
        account.charge(split_bytes).unwrap();
        account.uncharge(cost).unwrap();
        assert_eq!(account.usage().used, 0);
        assert_eq!(account.usage().peak, cost);
        const PAGE_START: u64 = 0x1000;
    }

    #[test]
    fn kb5_t7_real_splits_require_their_records_before_the_change() {
        let mut account = Ledger::new(charge::AREA);
        account.charge(charge::AREA).unwrap();
        assert!(!split_needed(0x1000, 0x4000, 0x1000));
        assert!(!split_needed(0x1000, 0x4000, 0x4000));
        assert!(!split_needed(0x1000, 0x4000, 0x5000));
        assert!(split_needed(0x1000, 0x4000, 0x2000));
        assert!(split_needed(0x1000, 0x4000, 0x3000));
        let before = account.usage();
        assert!(account.charge(2 * charge::AREA).is_err());
        assert_eq!(account.usage(), before);
    }

    #[test]
    fn kb5_t3_a_ledger_charges_within_its_limit() {
        let mut l = Ledger::new(16 * 4096);
        l.charge(4096).unwrap();
        l.charge(3 * 4096).unwrap();
        assert_eq!(
            l.usage(),
            Usage {
                limit: 16 * 4096,
                used: 4 * 4096,
                peak: 4 * 4096,
                reserved: 0
            }
        );
        // Past the limit: refused, nothing changed.
        let before = l.usage();
        let e = l.charge(13 * 4096).unwrap_err();
        assert_eq!(e.asked, 13 * 4096);
        assert_eq!(e.usage, before);
        assert_eq!(l.usage(), before);
        // Exactly to the limit.
        l.charge(12 * 4096).unwrap();
        assert_eq!(l.usage().room(), 0);
        assert!(l.charge(1).is_err());
        // Freeing returns the charge; the peak stays.
        l.uncharge(10 * 4096).unwrap();
        assert_eq!(l.usage().used, 6 * 4096);
        assert_eq!(l.usage().peak, 16 * 4096);
        l.uncharge(6 * 4096).unwrap();
        assert_eq!(l.usage().used, 0);
        assert_eq!(
            l.uncharge(1),
            Err(Underflow {
                returned: 1,
                held: 0
            })
        );
        // Arithmetic that would overflow is a refusal, never a wrap.
        let mut big = Ledger::new(u64::MAX);
        big.charge(u64::MAX - 1).unwrap();
        assert!(big.charge(2).is_err());
        assert!(big.reserve(u64::MAX).is_err());
    }

    #[test]
    fn kb5_t3_children_never_sum_past_their_parent() {
        let mut parent = Ledger::new(64 * MIB);
        parent.charge(8 * MIB).unwrap();
        parent.reserve(32 * MIB).unwrap();
        parent.reserve(24 * MIB).unwrap();
        assert_eq!(parent.usage().room(), 0);
        // One page more, for a child or a charge, is refused.
        assert!(parent.reserve(4096).is_err());
        assert!(parent.charge(4096).is_err());
        // A child gone gives its limit back.
        parent.unreserve(24 * MIB).unwrap();
        assert_eq!(parent.usage().room(), 24 * MIB);
        parent.reserve(16 * MIB).unwrap();
        parent.charge(8 * MIB).unwrap();
        assert_eq!(parent.usage().room(), 0);
        // What is held for children is not use: the peak counts charges alone.
        assert_eq!(parent.usage().peak, 16 * MIB);
        assert_eq!(parent.usage().reserved, 48 * MIB);
        assert!(parent.unreserve(49 * MIB).is_err());
        // A child larger than its parent can never be carved.
        assert!(Ledger::new(MIB).reserve(MIB + 4096).is_err());
    }

    #[test]
    fn kb5_t3_the_peak_is_the_most_at_once() {
        let mut l = Ledger::new(GIB);
        for n in [3u64, 9, 2, 7] {
            l.charge(n * 4096).unwrap();
            l.uncharge(n * 4096).unwrap();
        }
        assert_eq!(l.usage().peak, 9 * 4096);
        assert_eq!(l.usage().used, 0);
    }

    #[test]
    fn kb5_t4_a_map_charges_each_region_it_alone_touches() {
        // One page: one table at each level.
        let t = tables(&[(0x1000_0000_0000, 0x1000_0000_1000)], None, None, TOP);
        assert_eq!(t.pages, 3);
        assert_eq!(t.bytes(), 3 * 4096);
        // 64 MiB from a 2 MiB boundary: 32 last-level tables, one above each of them.
        let base = 0x1000_0000_0000;
        let t = tables(&[(base, base + 64 * MIB)], None, None, TOP);
        assert_eq!(t.pages, 32 + 1 + 1);
        // Off a boundary it touches one region more.
        let t = tables(&[(base + 4096, base + 4096 + 64 * MIB)], None, None, TOP);
        assert_eq!(t.pages, 33 + 1 + 1);
        // A neighbour below that touches the first region, and one above that touches
        // the last, leave those regions charged already.
        let t = tables(
            &[(base + 4096, base + 4096 + 64 * MIB)],
            Some(base + 4096),
            Some(base + 64 * MIB + 8192),
            TOP,
        );
        assert_eq!(t.pages, 31);
        // One that ends where the region begins touches the region below it.
        let t = tables(
            &[(base + 4096, base + 4096 + 64 * MIB)],
            Some(base),
            Some(base + 64 * MIB + 8192),
            TOP,
        );
        assert_eq!(t.pages, 32);
        // A neighbour in another region changes nothing.
        let t = tables(
            &[(base + 4 * MIB, base + 6 * MIB)],
            Some(base + MIB),
            Some(base + 10 * MIB),
            TOP,
        );
        assert_eq!(t.pages, 1);
        // Whole gibibytes: one table above each, one above them.
        let t = tables(&[(4 * GIB, 6 * GIB)], None, None, TOP);
        assert_eq!(t.pages, 1024 + 2 + 1);
    }

    #[test]
    fn kb5_t4_the_top_of_user_space_is_the_space_s_own() {
        // The last page of user space: its three tables are the fixed charge.
        let t = tables(&[(TOP - 4096, TOP)], None, None, TOP);
        assert_eq!(t.pages, 0);
        assert_eq!(t.reclaim, [None, None, None]);
        // The 2 MiB below the topmost are another last-level region, under the same
        // upper two.
        let end = TOP + 4096;
        let t = tables(&[(end - 4 * MIB, end - 2 * MIB)], None, None, TOP);
        assert_eq!(t.pages, 1);
    }

    #[test]
    fn kb5_t4_an_unmap_frees_what_nothing_touches_and_names_where() {
        let base = 0x1000_0000_0000;
        // Two areas removed with a hole between them, nothing left around: every region
        // either touched is freed once, and the hull covers them.
        let pieces = [
            (base, base + 3 * MIB),
            (base + 2 * MIB + 8192, base + 5 * MIB),
        ];
        let t = tables(&pieces, None, None, TOP);
        // Regions 0, 1, 2 of the last level (the second piece shares region 1), and
        // the one above.
        assert_eq!(t.pages, 3 + 1 + 1);
        assert_eq!(t.reclaim[0], Some((base, base + 6 * MIB)));
        assert_eq!(t.reclaim[1], Some((base, base + GIB)));
        assert_eq!(t.reclaim[2], Some((base, base + 512 * GIB)));
        // An area left below in the first region keeps it, and the regions above.
        let t = tables(&pieces, Some(base - 4096 + 8192), None, TOP);
        assert_eq!(t.pages, 2);
        assert_eq!(t.reclaim[0], Some((base + 2 * MIB, base + 6 * MIB)));
        assert_eq!(t.reclaim[1], None);
        assert_eq!(t.reclaim[2], None);
        // One above in the last region keeps that one.
        let t = tables(&pieces, Some(base - 4096 + 8192), Some(base + 5 * MIB), TOP);
        assert_eq!(t.pages, 1);
        assert_eq!(t.reclaim[0], Some((base + 2 * MIB, base + 4 * MIB)));
        // An empty piece and no pieces are nothing.
        assert_eq!(tables(&[(base, base)], None, None, TOP), Tables::default());
        assert_eq!(tables(&[], None, None, TOP), Tables::default());
    }

    /// Every region at every level any of `areas` touches, but the topmost: what the
    /// incremental counts must always sum to.
    fn brute(areas: &[(u64, u64)]) -> u64 {
        let mut n = 0;
        for size in charge::TABLE_REGIONS {
            let topmost = (TOP - 1) / size;
            let mut seen: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
            for &(s, e) in areas {
                for r in s / size..=(e - 1) / size {
                    if r != topmost && !seen.contains(&r) {
                        seen.push(r);
                    }
                }
            }
            n += seen.len() as u64;
        }
        n
    }

    /// The neighbours of `start .. end` among `areas` (sorted, disjoint, none inside it).
    fn around(areas: &[(u64, u64)], start: u64, end: u64) -> (Option<u64>, Option<u64>) {
        let before = areas.iter().filter(|a| a.1 <= start).map(|a| a.1).max();
        let after = areas.iter().filter(|a| a.0 >= end).map(|a| a.0).min();
        (before, after)
    }

    #[test]
    fn kb5_t4_incremental_counts_match_a_full_count() {
        // A deterministic walk of maps and unmaps over a few gibibytes near a 512 GiB
        // boundary and near the top of user space: after every step, the sum of what
        // each map added less what each unmap freed is the full count.
        let bases = [512 * GIB - 3 * GIB, TOP - 6 * GIB];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for base in bases {
            let mut areas: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
            let mut charged = 0u64;
            for _ in 0..400 {
                let start = base + (next() % (6 * GIB / 4096)) * 4096;
                let len = (1 + next() % 1500) * 4096 * if next() % 4 == 0 { 512 } else { 1 };
                let end = (start + len).min(base + 6 * GIB).min(TOP);
                if end <= start {
                    continue;
                }
                if next() % 3 != 0 {
                    // A map where nothing is.
                    if areas.iter().any(|a| a.0 < end && start < a.1) {
                        continue;
                    }
                    let (before, after) = around(&areas, start, end);
                    charged += tables(&[(start, end)], before, after, TOP).pages;
                    areas.push((start, end));
                    areas.sort();
                } else {
                    // An unmap of whatever lies in the range, splitting areas at its ends.
                    let mut kept = alloc::vec::Vec::new();
                    let mut removed = alloc::vec::Vec::new();
                    for &(s, e) in &areas {
                        if e <= start || s >= end {
                            kept.push((s, e));
                            continue;
                        }
                        if s < start {
                            kept.push((s, start));
                        }
                        if e > end {
                            kept.push((end, e));
                        }
                        removed.push((s.max(start), e.min(end)));
                    }
                    kept.sort();
                    removed.sort();
                    let (before, after) = around(&kept, start, end);
                    let freed = tables(&removed, before, after, TOP);
                    charged -= freed.pages;
                    // What the hull names holds no area any more.
                    for (lo, hi) in freed.reclaim.iter().flatten() {
                        assert!(!kept.iter().any(|a| a.0 < *hi && *lo < a.1));
                    }
                    areas = kept;
                }
                assert_eq!(charged, brute(&areas));
            }
        }
    }

    #[test]
    fn kb5_t4_handle_table_steps() {
        assert_eq!(table_steps(0), 0);
        assert_eq!(table_steps(1), 1);
        assert_eq!(table_steps(64), 1);
        assert_eq!(table_steps(65), 2);
        assert_eq!(table_steps(4096), 64);
    }

    use alloc::sync::Arc;

    /// An account as the core keeps one, for its CPU time: carved out of a parent it
    /// holds, as `kernel/core/src/account.rs` holds its own.
    #[derive(Debug, Default)]
    struct Node {
        cpu: CpuTime,
        parent: Option<Arc<Node>>,
    }

    impl Carved for Node {
        fn cpu(&self) -> &CpuTime {
            &self.cpu
        }
        fn parent(&self) -> Option<&Node> {
            self.parent.as_deref()
        }
    }

    fn carve(parent: &Arc<Node>) -> Arc<Node> {
        Arc::new(Node {
            cpu: CpuTime::new(),
            parent: Some(parent.clone()),
        })
    }

    /// A process: its threads' slices are charged to the account it was made in.
    struct Process(Arc<Node>);

    impl Process {
        fn ran(&self, ns: u64) {
            charge_cpu(&*self.0, ns);
        }
    }

    /// 6.3i: a slice is charged along the parent chain, so a job's account holds the CPU of
    /// its engine and of every function instance carved from it, and nothing of its
    /// neighbour's.
    #[test]
    fn i63_a_slice_is_charged_to_the_account_and_to_every_account_above_it() {
        let machine = Arc::new(Node::default());
        let admission = carve(&machine);
        let job = carve(&admission);
        let runtime = carve(&job);
        let task = carve(&runtime);
        let other_job = carve(&admission);

        Process(task.clone()).ran(5);
        for account in [&task, &runtime, &job, &admission, &machine] {
            assert_eq!(account.cpu.total(), 5);
        }
        // The engine's own threads: its account and those above, not the instance's.
        Process(runtime.clone()).ran(7);
        assert_eq!(task.cpu.total(), 5);
        for account in [&runtime, &job, &admission, &machine] {
            assert_eq!(account.cpu.total(), 12);
        }
        // A neighbour is charged its own, and the accounts above hold both.
        assert_eq!(other_job.cpu.total(), 0);
        Process(other_job.clone()).ran(100);
        assert_eq!(other_job.cpu.total(), 100);
        assert_eq!(job.cpu.total(), 12);
        assert_eq!(admission.cpu.total(), 112);
        assert_eq!(machine.cpu.total(), 112);
        // No time is no charge, and the machine's account has nothing above it.
        charge_cpu(&*task, 0);
        charge_cpu(&*machine, 1);
        assert_eq!(task.cpu.total(), 5);
        assert_eq!(machine.cpu.total(), 113);
        assert_eq!(admission.cpu.total(), 112);
    }

    /// 6.3i: the time is the account's, not the process's: a process that has ended stays
    /// counted, and another made in the account adds to it.
    #[test]
    fn i63_an_ended_process_stays_counted() {
        let machine = Arc::new(Node::default());
        let job = carve(&machine);
        let first = Process(job.clone());
        first.ran(40);
        first.ran(2);
        drop(first);
        assert_eq!(job.cpu.total(), 42);
        assert_eq!(machine.cpu.total(), 42);
        let second = Process(job.clone());
        second.ran(8);
        drop(second);
        assert_eq!(job.cpu.total(), 50);
        assert_eq!(machine.cpu.total(), 50);
    }

    /// 6.3i: an account carved from another is charged its own processes' time alone, its
    /// parent that and its own; a carved account that is gone leaves its time in its
    /// parent, and one carved after it starts at nothing.
    #[test]
    fn i63_a_carved_account_s_time_is_its_parent_s_too_and_outlives_it() {
        let engine = Arc::new(Node::default());
        Process(engine.clone()).ran(3);
        let instance = carve(&engine);
        Process(instance.clone()).ran(30);
        assert_eq!(instance.cpu.total(), 30);
        assert_eq!(engine.cpu.total(), 33);
        // The instance ends and its account goes: the last holder drops it.
        assert_eq!(Arc::strong_count(&instance), 1);
        drop(instance);
        assert_eq!(engine.cpu.total(), 33);
        let next = carve(&engine);
        assert_eq!(next.cpu.total(), 0);
        Process(next.clone()).ran(4);
        assert_eq!(next.cpu.total(), 4);
        assert_eq!(engine.cpu.total(), 37);
    }

    /// 6.3i: slices charged at once from several CPUs all land.
    #[test]
    fn i63_slices_charged_from_several_cpus_at_once_all_land() {
        let job = Arc::new(Node::default());
        let instances: alloc::vec::Vec<_> = (0..4).map(|_| carve(&job)).collect();
        let cpus: alloc::vec::Vec<_> = instances
            .iter()
            .cloned()
            .map(|instance| {
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        charge_cpu(&*instance, 3);
                    }
                })
            })
            .collect();
        for cpu in cpus {
            cpu.join().unwrap();
        }
        for instance in &instances {
            assert_eq!(instance.cpu.total(), 30_000);
        }
        assert_eq!(job.cpu.total(), 120_000);
    }

    /// 6.3i: whose running slice `account_cpu` adds: the caller's, when its process is
    /// charged to the account read or to one carved from it.
    #[test]
    fn i63_an_account_is_within_itself_and_within_what_it_was_carved_from() {
        let machine = Arc::new(Node::default());
        let engine = carve(&machine);
        let instance = carve(&engine);
        let neighbour = carve(&machine);
        assert!(within(&*engine, &*engine));
        assert!(within(&*instance, &*engine));
        assert!(within(&*instance, &*machine));
        assert!(!within(&*engine, &*instance));
        assert!(!within(&*neighbour, &*engine));
        assert!(!within(&*machine, &*engine));
        // The same account, not one that reads the same.
        assert_eq!(engine.cpu.total(), neighbour.cpu.total());
        assert!(!within(&*engine, &*neighbour));
    }

    #[test]
    fn i63_the_answer_adds_the_caller_s_running_slice_and_is_never_an_error() {
        assert_eq!(cpu_answer(0, None), 0);
        assert_eq!(cpu_answer(1_000, None), 1_000);
        assert_eq!(cpu_answer(1_000, Some(250)), 1_250);
        assert_eq!(cpu_answer(1_000, Some(0)), 1_000);
        // 292 years of CPU time is the most it says: above it a caller reads an error.
        let most = i64::MAX as u64;
        assert_eq!(cpu_answer(most, None), most);
        assert_eq!(cpu_answer(most, Some(1)), most);
        assert_eq!(cpu_answer(u64::MAX, Some(u64::MAX)), most);
        assert!(moruna_abi::Error::from_raw(cpu_answer(u64::MAX, None)).is_ok());
    }
}
