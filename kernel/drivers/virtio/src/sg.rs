//! A transfer's data as a device sees it: the pages of a pinned buffer, from any byte of it,
//! as the fewest ranges of device addresses, and those ranges as requests that each fit a
//! queue (item 3.3).
//!
//! A pinned buffer's pages are each a device address; pages that follow one another in the
//! device's view are one range, so a buffer on large pages is a few ranges, not one per page.
//! A request whose ranges are more than a queue may chain is cut into several, each one a
//! whole number of sectors, a range cut where a sector ends when it must be.

use alloc::vec::Vec;

use crate::queue::Buffer;

/// The bytes of a page of a pinned buffer.
pub const PAGE: u64 = 4096;

/// Why a transfer cannot be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// It reaches past the buffer's last page.
    PastEnd,
    /// It moves no bytes, or a number that is not whole sectors.
    NotWholeSectors,
    /// A request may hold no range at all.
    NoRoom,
}

/// The device ranges, `(address, bytes)`, of `bytes` bytes of a buffer whose page `i` is at
/// device address `pages[i]`, from its byte `offset`: one range for each run of pages
/// that follow one another, none longer than `u32::MAX` bytes.
pub fn ranges(pages: &[u64], offset: u64, bytes: u64) -> Result<Vec<(u64, u32)>, Layout> {
    let end = offset.checked_add(bytes).ok_or(Layout::PastEnd)?;
    if end > pages.len() as u64 * PAGE {
        return Err(Layout::PastEnd);
    }
    let mut out: Vec<(u64, u32)> = Vec::new();
    let mut at = offset;
    while at < end {
        let page = (at / PAGE) as usize;
        let within = at % PAGE;
        let n = (PAGE - within).min(end - at);
        let addr = pages[page] + within;
        match out.last_mut() {
            Some((start, len))
                if *start + u64::from(*len) == addr
                    && u64::from(*len) + n <= u64::from(u32::MAX) =>
            {
                *len += n as u32;
            }
            _ => out.push((addr, n as u32)),
        }
        at += n;
    }
    Ok(out)
}

/// A request's descriptor chain: the device reads `header`, then the data ranges, which it
/// writes when `device_writes`, then writes `status`.
pub fn chain(
    header: Buffer,
    ranges: &[(u64, u32)],
    device_writes: bool,
    status: Buffer,
) -> Vec<Buffer> {
    let mut chain = Vec::with_capacity(ranges.len() + 2);
    chain.push(header);
    chain.extend(ranges.iter().map(|&(addr, len)| Buffer {
        addr,
        len,
        device_writes,
    }));
    chain.push(status);
    chain
}

/// One request of a transfer: its first sector, relative to the transfer's, and its ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Sectors before it in the transfer.
    pub sector: u64,
    /// Its data, in order.
    pub ranges: Vec<(u64, u32)>,
}

impl Request {
    /// The bytes it moves.
    pub fn bytes(&self) -> u64 {
        self.ranges.iter().map(|(_, n)| u64::from(*n)).sum()
    }
}

/// `ranges` (a transfer of whole `sector`-byte sectors) as requests of at most `most` ranges
/// each, every one a whole number of sectors: a range is cut where a sector ends when the
/// request it would end would otherwise hold part of a sector. One request when they fit.
pub fn requests(ranges: &[(u64, u32)], sector: u64, most: usize) -> Result<Vec<Request>, Layout> {
    let total: u64 = ranges.iter().map(|(_, n)| u64::from(*n)).sum();
    if total == 0 || sector == 0 || !total.is_multiple_of(sector) {
        return Err(Layout::NotWholeSectors);
    }
    if most == 0 {
        return Err(Layout::NoRoom);
    }
    let mut out = Vec::new();
    let mut current: Vec<(u64, u32)> = Vec::new();
    let mut held = 0u64;
    let mut moved = 0u64;
    let mut pending: Vec<(u64, u64)> = ranges.iter().map(|&(a, n)| (a, u64::from(n))).collect();
    pending.reverse();
    while let Some((addr, len)) = pending.pop() {
        if current.len() + 1 < most {
            current.push((addr, len as u32));
            held += len;
            continue;
        }
        // The last range this request may hold: it ends the request on a sector's end, and
        // what is left of it starts the next one.
        let whole = (held + len) / sector * sector;
        let take = whole.saturating_sub(held);
        if take == 0 {
            // Not even a sector's end in reach: the request ends where it is, which is on a
            // sector's end, since every request before it did and none of its ranges was cut.
            if held == 0 || !held.is_multiple_of(sector) {
                return Err(Layout::NoRoom);
            }
            pending.push((addr, len));
        } else {
            current.push((addr, take as u32));
            held += take;
            if take < len {
                pending.push((addr + take, len - take));
            }
        }
        out.push(Request {
            sector: moved / sector,
            ranges: core::mem::take(&mut current),
        });
        moved += held;
        held = 0;
    }
    if !current.is_empty() {
        if !held.is_multiple_of(sector) {
            return Err(Layout::NotWholeSectors);
        }
        out.push(Request {
            sector: moved / sector,
            ranges: current,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const MIB: u64 = 1 << 20;

    /// A region on one 2 MiB page: every page follows the last.
    fn huge(pages: usize) -> Vec<u64> {
        (0..pages as u64).map(|i| 0x4000_0000 + i * PAGE).collect()
    }

    /// A region whose pages are each somewhere else.
    fn fragmented(pages: usize) -> Vec<u64> {
        (0..pages as u64)
            .map(|i| 0x1000_0000 + i * 3 * PAGE)
            .collect()
    }

    #[test]
    fn a_region_on_a_large_page_is_one_range_from_any_byte() {
        let pages = huge(512);
        assert_eq!(ranges(&pages, 0, MIB).unwrap(), [(0x4000_0000, MIB as u32)]);
        assert_eq!(
            ranges(&pages, 4096 + 100, MIB).unwrap(),
            [(0x4000_0000 + 4196, MIB as u32)]
        );
        let reqs = requests(&ranges(&pages, 100, MIB).unwrap(), 512, 254).unwrap();
        assert_eq!(reqs.len(), 1, "a 1 MiB transfer is one request");
        assert_eq!(reqs[0].ranges.len(), 1, "and one descriptor of data");
    }

    #[test]
    fn a_fragmented_region_is_one_range_a_page_and_a_cut_first_page() {
        let pages = fragmented(4);
        assert_eq!(
            ranges(&pages, 100, 3 * PAGE).unwrap(),
            [
                (0x1000_0000 + 100, 3996),
                (0x1000_0000 + 3 * PAGE, 4096),
                (0x1000_0000 + 6 * PAGE, 4096),
                (0x1000_0000 + 9 * PAGE, 100),
            ]
        );
        // Two neighbours of four pages, then a gap.
        let mixed = [0x8000, 0x9000, 0x2_0000, 0x2_1000];
        assert_eq!(
            ranges(&mixed, 0, 4 * PAGE).unwrap(),
            [(0x8000, 8192), (0x2_0000, 8192)]
        );
        assert_eq!(ranges(&pages, 0, 0).unwrap(), []);
    }

    #[test]
    fn a_transfer_past_the_buffer_is_refused() {
        let pages = huge(2);
        assert_eq!(ranges(&pages, 1, 2 * PAGE), Err(Layout::PastEnd));
        assert_eq!(ranges(&pages, u64::MAX, 1), Err(Layout::PastEnd));
        assert!(ranges(&pages, 0, 2 * PAGE).is_ok());
    }

    #[test]
    fn a_range_is_never_longer_than_a_descriptor_takes() {
        // 5 GiB of following pages: two ranges, the first as long as a u32 allows in pages.
        let pages = huge(5 << 18);
        let r = ranges(&pages, 0, 5 << 30).unwrap();
        assert!(r.iter().all(|(_, n)| *n > 0));
        assert_eq!(r.iter().map(|(_, n)| u64::from(*n)).sum::<u64>(), 5 << 30);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn a_fragmented_mebibyte_is_cut_to_the_queue_on_sector_ends() {
        // 1 MiB from byte 100 of a fragmented region: 257 ranges, and a queue whose request
        // holds 254 of them (a 256-entry queue less the header and the status).
        let pages = fragmented(258);
        let r = ranges(&pages, 100, MIB).unwrap();
        assert_eq!(r.len(), 257);
        let reqs = requests(&r, 512, 254).unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|q| q.ranges.len() <= 254));
        assert!(reqs.iter().all(|q| q.bytes().is_multiple_of(512)));
        assert_eq!(reqs.iter().map(Request::bytes).sum::<u64>(), MIB);
        assert_eq!(reqs[0].sector, 0);
        assert_eq!(reqs[1].sector, reqs[0].bytes() / 512);
        // The data in order, cut and all.
        let joined: Vec<(u64, u64)> = reqs
            .iter()
            .flat_map(|q| q.ranges.iter().map(|&(a, n)| (a, u64::from(n))))
            .collect();
        let mut flat: Vec<(u64, u64)> = Vec::new();
        for (a, n) in joined {
            match flat.last_mut() {
                Some((s, l)) if *s + *l == a => *l += n,
                _ => flat.push((a, n)),
            }
        }
        let want: Vec<(u64, u64)> = r.iter().map(|&(a, n)| (a, u64::from(n))).collect();
        assert_eq!(flat, want);
    }

    #[test]
    fn a_request_ends_on_a_sector_whatever_its_ranges() {
        // Two ranges a request: the second range of the first request is cut where its
        // sector ends, and the rest of it starts the next.
        let r = vec![(0x1000, 700), (0x9000, 900), (0x2_0000, 448)];
        let reqs = requests(&r, 512, 2).unwrap();
        assert_eq!(
            reqs,
            [
                Request {
                    sector: 0,
                    ranges: vec![(0x1000, 700), (0x9000, 836)]
                },
                Request {
                    sector: 3,
                    ranges: vec![(0x9000 + 836, 64), (0x2_0000, 448)]
                },
            ]
        );
        // One that ends on a sector already is not cut.
        let r = vec![(0x1000, 700), (0x9000, 324), (0x2_0000, 512)];
        let reqs = requests(&r, 512, 2).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].ranges, [(0x1000, 700), (0x9000, 324)]);
        assert_eq!(reqs[1].sector, 2);
        // A sector split over two ranges cannot be one range a request; nor can a sector's
        // end be out of reach of a request's last range.
        assert_eq!(
            requests(&[(0x1000, 700), (0x9000, 836)], 512, 1),
            Err(Layout::NoRoom)
        );
        assert_eq!(
            requests(&[(0, 100), (0x2000, 100), (0x4000, 312)], 512, 2),
            Err(Layout::NoRoom)
        );
    }

    #[test]
    fn a_chain_is_the_header_the_data_and_the_status() {
        let header = Buffer {
            addr: 0x100,
            len: 16,
            device_writes: false,
        };
        let status = Buffer {
            addr: 0x110,
            len: 1,
            device_writes: true,
        };
        let pages = huge(256);
        let r = ranges(&pages, 0, MIB).unwrap();
        let c = chain(header, &r, true, status);
        assert_eq!(
            c.len(),
            3,
            "a 1 MiB read on a large page is three descriptors"
        );
        assert_eq!(c[0], header);
        assert_eq!(
            c[1],
            Buffer {
                addr: 0x4000_0000,
                len: MIB as u32,
                device_writes: true
            }
        );
        assert_eq!(c[2], status);
        // A flush has no data.
        assert_eq!(chain(header, &[], false, status), [header, status]);
        // Writes are read by the device.
        assert!(!chain(header, &r, false, status)[1].device_writes);
    }

    #[test]
    fn many_mebibyte_reads_on_large_pages_fit_one_queue_at_once() {
        // A 256-entry queue: a request from a large-page region takes three entries, so
        // eighty-five 1 MiB reads are in flight together; a fragmented one takes the queue.
        let pages = huge(512);
        let per = chain(
            Buffer {
                addr: 0,
                len: 16,
                device_writes: false,
            },
            &ranges(&pages, 4096, MIB).unwrap(),
            true,
            Buffer {
                addr: 0,
                len: 16,
                device_writes: false,
            },
        )
        .len();
        assert_eq!(256 / per, 85);
        let frag = fragmented(256);
        let reqs = requests(&ranges(&frag, 0, MIB).unwrap(), 512, 254).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].ranges.len() + 2, 256);
    }

    #[test]
    fn what_is_not_whole_sectors_or_has_no_room_is_refused() {
        assert_eq!(requests(&[], 512, 4), Err(Layout::NotWholeSectors));
        assert_eq!(requests(&[(0, 100)], 512, 4), Err(Layout::NotWholeSectors));
        assert_eq!(requests(&[(0, 512)], 0, 4), Err(Layout::NotWholeSectors));
        assert_eq!(requests(&[(0, 512)], 512, 0), Err(Layout::NoRoom));
        assert_eq!(
            requests(&[(0, 512)], 512, 4).unwrap(),
            [Request {
                sector: 0,
                ranges: vec![(0, 512)]
            }]
        );
    }
}
