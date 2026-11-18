//! Item 1.10b's Azure check (system design 1F): what the check program decides from the
//! bytes it reads. The program itself is `main.rs`, in the cloud profile's check variant only
//! (`kernel/node/cloud-check.image`), which the PM boots on Azure with a data disk at LUN 0.
//!
//! Through the device manager's directory it finds the disks storvsc registered:
//!
//! - `boot`, the OS disk: its GPT header at LBA 1 is read and checked ([`Gpt::parse`]);
//! - `lun-0`, the data disk: [`SECTORS`] sectors from [`FIRST`] are read, written with a
//!   [`pattern`], flushed, read back, read again with `read_to` at byte [`OFFSET`] of a
//!   registration, and restored; then the block protocol's conformance suite runs against
//!   it, and restores what it writes.
//!
//! Each step says one line on the console, `storvsc-check: <step>: ok: ...` or
//! `storvsc-check: <step>: FAIL: ...`, and the last says how many passed.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{format, string::String};

/// The first sector the check writes on the data disk: 1 MiB in, where a partitioned disk's
/// first partition would start.
pub const FIRST: u64 = 2048;

/// The sectors the check writes there.
pub const SECTORS: u32 = 8;

/// The byte of a registration `read_to` reads into: not a sector's or a page's boundary.
pub const OFFSET: u64 = 1000;

/// The bytes the check writes: a pattern that differs from sector to sector.
pub fn pattern(sector: u64, at: usize) -> u8 {
    ((sector.wrapping_mul(29) as usize + at * 11 + 0x3d) & 0xff) as u8
}

/// CRC-32 (IEEE 802.3, reflected, as UEFI's headers use).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A GPT header (UEFI 2.10, 5.3.2), as much of it as the check says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpt {
    /// The header's revision, major and minor.
    pub revision: (u16, u16),
    /// The disk's GUID, as the header stores it.
    pub disk_guid: [u8; 16],
    /// The LBA of the backup header.
    pub alternate: u64,
    /// How many partition entries the array holds.
    pub entries: u32,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn quad(bytes: &[u8], at: usize) -> u64 {
    u64::from(word(bytes, at)) | u64::from(word(bytes, at + 4)) << 32
}

/// The smallest header UEFI allows, and the largest a sector of 512 bytes holds.
const HEADER_MIN: usize = 92;

impl Gpt {
    /// The header in `sector`, the disk's LBA 1: its signature, its own LBA, and its CRC
    /// over its own size checked.
    pub fn parse(sector: &[u8]) -> Result<Gpt, String> {
        if sector.len() < HEADER_MIN {
            return Err(format!("{} bytes, short of a GPT header", sector.len()));
        }
        if &sector[..8] != b"EFI PART" {
            return Err(String::from("no \"EFI PART\" signature at LBA 1"));
        }
        let size = word(sector, 12) as usize;
        if !(HEADER_MIN..=sector.len()).contains(&size) {
            return Err(format!("a header size of {size}"));
        }
        let mut header = alloc::vec::Vec::from(&sector[..size]);
        header[16..20].fill(0);
        let crc = crc32(&header);
        if crc != word(sector, 16) {
            return Err(format!(
                "a header CRC of {:#010x}, which its bytes make {crc:#010x}",
                word(sector, 16)
            ));
        }
        if quad(sector, 24) != 1 {
            return Err(format!(
                "a header that says it is at LBA {}",
                quad(sector, 24)
            ));
        }
        let mut disk_guid = [0; 16];
        disk_guid.copy_from_slice(&sector[56..72]);
        let revision = word(sector, 8);
        Ok(Gpt {
            revision: ((revision >> 16) as u16, revision as u16),
            disk_guid,
            alternate: quad(sector, 32),
            entries: word(sector, 80),
        })
    }

    /// The disk's GUID in its text form: its first three fields little-endian, as UEFI
    /// stores them.
    pub fn guid(&self) -> String {
        let g = &self.disk_guid;
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            word(g, 0),
            u16::from_le_bytes([g[4], g[5]]),
            u16::from_le_bytes([g[6], g[7]]),
            g[8],
            g[9],
            g[10],
            g[11],
            g[12],
            g[13],
            g[14],
            g[15]
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    /// A header as `sgdisk` writes one: revision 1.0, 92 bytes, at LBA 1, 128 entries.
    fn header() -> alloc::vec::Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[..8].copy_from_slice(b"EFI PART");
        sector[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        sector[12..16].copy_from_slice(&92u32.to_le_bytes());
        sector[24..32].copy_from_slice(&1u64.to_le_bytes());
        sector[32..40].copy_from_slice(&65_535u64.to_le_bytes());
        sector[56..72].copy_from_slice(&[
            0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ]);
        sector[80..84].copy_from_slice(&128u32.to_le_bytes());
        let crc = crc32(&sector[..92]);
        sector[16..20].copy_from_slice(&crc.to_le_bytes());
        sector
    }

    #[test]
    fn crc32_is_ieees() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn a_gpt_header_is_read_and_checked() {
        let gpt = Gpt::parse(&header()).unwrap();
        assert_eq!(gpt.revision, (1, 0));
        assert_eq!(gpt.alternate, 65_535);
        assert_eq!(gpt.entries, 128);
        assert_eq!(gpt.guid(), "12345678-9abc-def0-0123-456789abcdef");
    }

    #[test]
    fn what_is_no_gpt_header_says_why() {
        assert!(Gpt::parse(&[0; 91]).unwrap_err().contains("short of"));
        assert!(Gpt::parse(&[0; 512]).unwrap_err().contains("signature"));
        let mut big = header();
        big[12..16].copy_from_slice(&600u32.to_le_bytes());
        assert_eq!(Gpt::parse(&big).unwrap_err(), "a header size of 600");
        let mut changed = header();
        changed[80] = 7;
        assert!(
            Gpt::parse(&changed)
                .unwrap_err()
                .starts_with("a header CRC of")
        );
        let mut elsewhere = header();
        elsewhere[24] = 2;
        elsewhere[16..20].fill(0);
        let crc = crc32(&elsewhere[..92]);
        elsewhere[16..20].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Gpt::parse(&elsewhere).unwrap_err(),
            "a header that says it is at LBA 2"
        );
    }

    #[test]
    fn the_pattern_differs_from_sector_to_sector() {
        assert_ne!(pattern(FIRST, 0), pattern(FIRST + 1, 0));
        assert_ne!(pattern(FIRST, 0), pattern(FIRST, 1));
        assert_eq!(OFFSET % 512, 1000 % 512);
        assert_eq!(SECTORS, 8);
    }
}
