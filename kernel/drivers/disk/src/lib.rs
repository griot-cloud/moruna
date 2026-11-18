//! What a block driver tells of its disk (`architecture/moruna-os-system.md` 3C, item 3.3):
//! the serial a job names it by, and whether it is the disk the node booted from, which is
//! never a volume.
//!
//! - [`serial`]: the serial a device states (virtio-blk's `GET_ID`, at most 20 bytes padded
//!   with zeros; storvsc's VPD page 0x80), when it states one a directory can hold.
//! - [`Guid`] and [`disk_guid`]: the node's image carries its own GPT disk GUID, which
//!   `package.py` sets from the image's content and writes into the boot payload as
//!   `disk.guid`. A disk whose GPT header, checked whole, names that GUID is the boot disk;
//!   another Moruna image's disk attached as a volume has another.
//! - [`entries`], [`find`] and [`Slice`]: the boot disk's `moruna-system` partition (system
//!   design 1A), which holds the node's engine layer: its driver serves it as a disk of its own,
//!   `system`, read only, a window of the boot disk that no request reaches past (item 3.3).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::fmt;

/// The bytes of virtio-blk's serial (virtio 1.2, 5.2.6).
pub const VIRTIO_ID_BYTES: usize = 20;
/// The longest serial a directory holds a disk under.
pub const SERIAL_MAX: usize = 64;

/// The serial `bytes` state: the bytes before the first zero, when they are 1 to
/// [`SERIAL_MAX`] visible ASCII characters. `None`: the device states none a job could name.
pub fn serial(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = &bytes[..end];
    if text.is_empty() || text.len() > SERIAL_MAX || !text.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    core::str::from_utf8(text).ok().map(String::from)
}

/// A GPT GUID, its 16 bytes in the order a disk holds them: the first three fields little
/// endian, the last two as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid(pub [u8; 16]);

/// The bytes of the text form, `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
pub const GUID_TEXT: usize = 36;

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Guid {
    /// The GUID `text` writes, in either case.
    pub fn parse(text: &str) -> Option<Guid> {
        let t = text.as_bytes();
        if t.len() != GUID_TEXT || [8, 13, 18, 23].iter().any(|&i| t[i] != b'-') {
            return None;
        }
        let mut written = [0u8; 16];
        let mut digits = t.iter().filter(|&&b| b != b'-');
        for byte in &mut written {
            let high = hex(*digits.next()?)?;
            let low = hex(*digits.next()?)?;
            *byte = high << 4 | low;
        }
        let w = written;
        Some(Guid([
            w[3], w[2], w[1], w[0], w[5], w[4], w[7], w[6], w[8], w[9], w[10], w[11], w[12], w[13],
            w[14], w[15],
        ]))
    }

    /// The GUID a boot payload's `disk.guid` holds: its text, then a newline or zeros to the
    /// end of the file's page.
    pub fn from_file(bytes: &[u8]) -> Option<Guid> {
        let text = bytes.get(..GUID_TEXT)?;
        let rest = &bytes[GUID_TEXT..];
        let rest = rest.strip_prefix(b"\n").unwrap_or(rest);
        if rest.iter().any(|&b| b != 0) {
            return None;
        }
        Guid::parse(core::str::from_utf8(text).ok()?)
    }
}

impl fmt::Display for Guid {
    /// The text form, upper case, as `sgdisk` prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let g = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-",
            g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6], g[8], g[9]
        )?;
        g[10..].iter().try_for_each(|b| write!(f, "{b:02X}"))
    }
}

/// The GPT header's signature.
pub const SIGNATURE: &[u8; 8] = b"EFI PART";
/// The smallest header GPT defines (UEFI 2.10, 5.3.2).
pub const HEADER_MIN: usize = 92;

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// The IEEE CRC-32 GPT uses.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The disk GUID of the primary GPT header `sector` holds (the disk's logical block 1):
/// `None` unless its signature, its size (92 bytes to the sector's), its own place (block 1)
/// and its CRC-32 are what GPT says.
pub fn disk_guid(sector: &[u8]) -> Option<Guid> {
    let header = header(sector)?;
    let mut guid = [0u8; 16];
    guid.copy_from_slice(&header[56..72]);
    Some(Guid(guid))
}

/// The primary GPT header `sector` holds, checked as [`disk_guid`] says.
fn header(sector: &[u8]) -> Option<&[u8]> {
    if sector.len() < HEADER_MIN || &sector[..8] != SIGNATURE {
        return None;
    }
    let size = le32(sector, 12) as usize;
    if !(HEADER_MIN..=sector.len()).contains(&size) || le64(sector, 24) != 1 {
        return None;
    }
    let mut header = alloc::vec::Vec::from(&sector[..size]);
    header[16..20].fill(0);
    if crc32(&header) != le32(sector, 16) {
        return None;
    }
    Some(&sector[..size])
}

/// The most bytes of partition entries read: 128 entries of 128 bytes, as every GPT tool
/// writes, four times over.
pub const ENTRIES_MAX: u64 = 64 * 1024;

/// Where a GPT's partition entries are, from its header, and the CRC-32 they must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entries {
    /// Their first logical block.
    pub lba: u64,
    /// How many there are.
    pub count: u32,
    /// The bytes of each: 128, or 128 times a power of two.
    pub size: u32,
    /// The CRC-32 of all of them.
    pub crc: u32,
}

impl Entries {
    /// Their bytes.
    pub fn bytes(&self) -> u64 {
        u64::from(self.count) * u64::from(self.size)
    }
}

/// The partition entries the primary header `sector` names: `None` unless the header is one
/// ([`disk_guid`]'s checks), they start after it, and they are 1 to [`ENTRIES_MAX`] bytes of
/// entries of a size GPT allows.
pub fn entries(sector: &[u8]) -> Option<Entries> {
    let header = header(sector)?;
    let entries = Entries {
        lba: le64(header, 72),
        count: le32(header, 80),
        size: le32(header, 84),
        crc: le32(header, 88),
    };
    let size_ok = entries.size.is_multiple_of(128) && (entries.size / 128).is_power_of_two();
    (entries.lba >= 2 && entries.count > 0 && size_ok && entries.bytes() <= ENTRIES_MAX)
        .then_some(entries)
}

/// A partition: its first and last logical blocks, the last inclusive, as GPT holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Its first logical block.
    pub first: u64,
    /// Its last logical block.
    pub last: u64,
}

impl Partition {
    /// Its logical blocks.
    pub fn blocks(&self) -> u64 {
        self.last - self.first + 1
    }
}

/// The name the partition that holds the node's engine layer carries (system design 1A).
pub const SYSTEM: &str = "moruna-system";

/// The serial the system partition is registered under in the device manager's directory.
pub const SYSTEM_SERIAL: &str = "system";

/// Whether a GPT entry's name, 36 UTF-16LE code units padded with zeros, is `name`.
fn named(entry: &[u8], name: &str) -> bool {
    let units: alloc::vec::Vec<u16> = entry[56..128]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
    units[..end].iter().copied().eq(name.encode_utf16())
}

/// The partition named `name` among `bytes`, the entries `entries` names: `None` when they are
/// not all there, their CRC-32 is not the header's, or no entry in use has that name and blocks
/// from the first to the last.
pub fn find(bytes: &[u8], entries: &Entries, name: &str) -> Option<Partition> {
    let all = bytes.get(..usize::try_from(entries.bytes()).ok()?)?;
    if crc32(all) != entries.crc {
        return None;
    }
    all.chunks_exact(entries.size as usize)
        .filter(|e| e[..16].iter().any(|b| *b != 0))
        .find(|e| named(e, name))
        .map(|e| Partition {
            first: le64(e, 32),
            last: le64(e, 40),
        })
        .filter(|p| p.first <= p.last)
}

/// A window of a disk served as a disk of its own: `sectors` sectors from the disk's sector
/// `first`. A request the window does not hold whole is refused, never cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    /// The disk's sector the window's first is.
    pub first: u64,
    /// The window's sectors.
    pub sectors: u64,
}

impl Slice {
    /// `partition` (in logical blocks of `block` bytes) as sectors of `sector` bytes of a disk
    /// of `disk` sectors: `None` unless the blocks are whole sectors and the disk holds it all.
    pub fn of(partition: &Partition, block: u64, sector: u64, disk: u64) -> Option<Slice> {
        if block == 0 || sector == 0 || !block.is_multiple_of(sector) {
            return None;
        }
        let per = block / sector;
        let first = partition.first.checked_mul(per)?;
        let sectors = partition.blocks().checked_mul(per)?;
        (first.checked_add(sectors)? <= disk).then_some(Slice { first, sectors })
    }

    /// The disk's sector of the window's sector `sector`, for a request of `count` sectors:
    /// `None` when the window does not hold all of them.
    pub fn map(&self, sector: u64, count: u64) -> Option<u64> {
        (sector.checked_add(count)? <= self.sectors).then(|| self.first + sector)
    }
}

/// Whether the disk whose logical block 1 is `sector` is the one the node booted from, the
/// node's image carrying `image`.
pub fn is_boot(sector: &[u8], image: &Guid) -> bool {
    disk_guid(sector).is_some_and(|g| g == *image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec, vec::Vec};

    /// The EFI system partition's type: the GUID whose on-disk bytes every GPT document shows.
    const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    const ESP_BYTES: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];

    /// A primary GPT header naming `guid`, as `sgdisk` writes one, in a sector of `bytes`.
    fn header(guid: &Guid, bytes: usize) -> Vec<u8> {
        let mut h = vec![0u8; bytes];
        h[..8].copy_from_slice(SIGNATURE);
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        h[32..40].copy_from_slice(&2047u64.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&2014u64.to_le_bytes());
        h[56..72].copy_from_slice(&guid.0);
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }

    #[test]
    fn a_guid_is_held_as_gpt_holds_it_and_printed_as_sgdisk_prints_it() {
        let esp = Guid::parse(ESP).unwrap();
        assert_eq!(esp.0, ESP_BYTES);
        assert_eq!(format!("{esp}"), ESP);
        assert_eq!(Guid::parse(&ESP.to_lowercase()), Some(esp));
        assert_eq!(Guid::parse(&ESP[1..]), None);
        assert_eq!(Guid::parse(&ESP.replace('-', "x")), None);
        assert_eq!(Guid::parse(&ESP.replace('C', "G")), None);
        assert_eq!(Guid::parse("C12A7328-F81F-11D2-BA4B00-A0C93EC93B"), None);
    }

    #[test]
    fn the_payloads_guid_is_its_text_then_a_newline_or_zeros() {
        let esp = Guid::parse(ESP).unwrap();
        let mut page = vec![0u8; 4096];
        page[..36].copy_from_slice(ESP.as_bytes());
        page[36] = b'\n';
        assert_eq!(Guid::from_file(&page), Some(esp));
        assert_eq!(Guid::from_file(ESP.as_bytes()), Some(esp));
        page[37] = b'x';
        assert_eq!(Guid::from_file(&page), None, "anything after the line");
        assert_eq!(Guid::from_file(&ESP.as_bytes()[..30]), None);
        assert_eq!(Guid::from_file(&[0xff; 40]), None);
    }

    #[test]
    fn crc32_is_the_ieee_one() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_boot_disk_is_the_one_whose_checked_header_names_the_images_guid() {
        let image = Guid::parse("0F5C3A9E-2B71-4D8A-9C3E-5A1B2C3D4E5F").unwrap();
        let other = Guid::parse(ESP).unwrap();
        let boot = header(&image, 512);
        assert_eq!(disk_guid(&boot), Some(image));
        assert!(is_boot(&boot, &image));
        assert!(
            !is_boot(&header(&other, 512), &image),
            "another image's disk"
        );
        // A 4 KiB sector holds the same header.
        assert!(is_boot(&header(&image, 4096), &image));
        // Every check of the header.
        let mut bad = boot.clone();
        bad[0] = b'X';
        assert!(!is_boot(&bad, &image), "signature");
        let mut bad = boot.clone();
        bad[60] ^= 1;
        assert!(!is_boot(&bad, &image), "a byte the CRC covers");
        let mut bad = boot.clone();
        bad[12..16].copy_from_slice(&91u32.to_le_bytes());
        assert!(!is_boot(&bad, &image), "a header too small");
        let mut bad = boot.clone();
        bad[12..16].copy_from_slice(&513u32.to_le_bytes());
        assert!(!is_boot(&bad, &image), "a header larger than its sector");
        let mut bad = header(&image, 512);
        bad[24..32].copy_from_slice(&2047u64.to_le_bytes());
        let crc = {
            let mut h = bad[..92].to_vec();
            h[16..20].fill(0);
            crc32(&h)
        };
        bad[16..20].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(disk_guid(&bad), None, "the backup header");
        assert!(!is_boot(&boot[..91], &image), "a short sector");
        assert!(!is_boot(&[0u8; 512], &image), "no partition table");
    }

    #[test]
    fn a_header_larger_than_512_bytes_is_checked_whole() {
        let image = Guid::parse(ESP).unwrap();
        let mut h = header(&image, 4096);
        h[12..16].copy_from_slice(&600u32.to_le_bytes());
        h[599] = 7;
        h[16..20].fill(0);
        let crc = crc32(&h[..600]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(disk_guid(&h), Some(image));
        h[599] = 8;
        assert_eq!(disk_guid(&h), None);
    }

    /// A GPT of `entries` 128-byte entries at LBA 2, on 512-byte blocks, with the partitions
    /// `parts` (`name`, first, last): the header's sector and the entries' bytes.
    fn gpt(parts: &[(&str, u64, u64)], entries: u32) -> (Vec<u8>, Vec<u8>) {
        let mut table = vec![0u8; entries as usize * 128];
        for (i, (name, first, last)) in parts.iter().enumerate() {
            let e = &mut table[i * 128..(i + 1) * 128];
            e[..16].copy_from_slice(&[0xaf; 16]);
            e[16..32].copy_from_slice(&[i as u8 + 1; 16]);
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
            for (j, unit) in name.encode_utf16().enumerate() {
                e[56 + 2 * j..58 + 2 * j].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let mut h = header(&Guid::parse(ESP).unwrap(), 512);
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&entries.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&table).to_le_bytes());
        h[16..20].fill(0);
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        (h, table)
    }

    #[test]
    fn the_system_partition_is_found_by_its_name_in_checked_entries() {
        let (h, table) = gpt(
            &[("EFI system", 2048, 133119), (SYSTEM, 133120, 141311)],
            128,
        );
        let e = entries(&h).unwrap();
        assert_eq!((e.lba, e.count, e.size, e.bytes()), (2, 128, 128, 16384));
        assert_eq!(
            find(&table, &e, SYSTEM),
            Some(Partition {
                first: 133120,
                last: 141311
            })
        );
        assert_eq!(find(&table, &e, SYSTEM).unwrap().blocks(), 8192);
        assert_eq!(find(&table, &e, "moruna-sys"), None, "a name is whole");
        assert_eq!(find(&table, &e, "moruna-system-2"), None);
        assert_eq!(find(&table[..1000], &e, SYSTEM), None, "entries cut short");
        let mut bad = table.clone();
        bad[200] ^= 1;
        assert_eq!(find(&bad, &e, SYSTEM), None, "the entries' CRC");
        // An entry not in use (its type all zeros) is no partition, whatever its name.
        let mut unused = table.clone();
        unused[128..144].fill(0);
        let e2 = Entries {
            crc: crc32(&unused),
            ..e
        };
        assert_eq!(find(&unused, &e2, SYSTEM), None);
        // Blocks that run backwards are no partition.
        let (h, table) = gpt(&[(SYSTEM, 10, 9)], 4);
        assert_eq!(find(&table, &entries(&h).unwrap(), SYSTEM), None);
    }

    #[test]
    fn entries_a_header_cannot_name_are_none() {
        let (h, _) = gpt(&[], 128);
        let with = |at: usize, value: &[u8]| {
            let mut b = h.clone();
            b[at..at + value.len()].copy_from_slice(value);
            b[16..20].fill(0);
            let crc = crc32(&b[..92]);
            b[16..20].copy_from_slice(&crc.to_le_bytes());
            b
        };
        assert!(entries(&h).is_some());
        assert_eq!(
            entries(&with(72, &1u64.to_le_bytes())),
            None,
            "over the header"
        );
        assert_eq!(entries(&with(80, &0u32.to_le_bytes())), None, "no entries");
        assert_eq!(
            entries(&with(84, &100u32.to_le_bytes())),
            None,
            "a size GPT has not"
        );
        assert_eq!(
            entries(&with(84, &384u32.to_le_bytes())),
            None,
            "not 128 times a power of two"
        );
        assert_eq!(
            entries(&with(80, &4096u32.to_le_bytes())),
            None,
            "more than we read"
        );
        assert!(entries(&with(84, &256u32.to_le_bytes())).is_some());
        assert_eq!(entries(&[0u8; 512]), None);
    }

    #[test]
    fn a_slice_holds_its_window_and_refuses_past_its_end() {
        let p = Partition {
            first: 133120,
            last: 141311,
        };
        let s = Slice::of(&p, 512, 512, 1 << 20).unwrap();
        assert_eq!((s.first, s.sectors), (133120, 8192));
        assert_eq!(s.map(0, 1), Some(133120));
        assert_eq!(s.map(8191, 1), Some(141311));
        assert_eq!(s.map(8192 - 16, 16), Some(141311 - 15));
        assert_eq!(s.map(8192, 1), None, "a read past its end");
        assert_eq!(s.map(8191, 2), None, "a read that runs past it");
        assert_eq!(s.map(u64::MAX, 1), None);
        // On 4 KiB logical blocks, each is eight of the driver's sectors.
        let s4 = Slice::of(
            &Partition {
                first: 10,
                last: 19,
            },
            4096,
            512,
            1000,
        )
        .unwrap();
        assert_eq!((s4.first, s4.sectors), (80, 80));
        assert_eq!(
            Slice::of(&p, 512, 4096, 1 << 20),
            None,
            "blocks smaller than sectors"
        );
        assert_eq!(Slice::of(&p, 512, 512, 141311), None, "past the disk's end");
        assert_eq!(Slice::of(&p, 0, 512, 1 << 20), None);
    }

    #[test]
    fn a_serial_is_the_visible_ascii_before_the_first_zero() {
        let mut id = [0u8; VIRTIO_ID_BYTES];
        id[..9].copy_from_slice(b"nyc-2024a");
        assert_eq!(serial(&id).as_deref(), Some("nyc-2024a"));
        assert_eq!(
            serial(b"01234567890123456789").as_deref(),
            Some("01234567890123456789")
        );
        assert_eq!(serial(&[0u8; 20]), None, "none stated");
        assert_eq!(serial(b"lun 0\0"), None, "a space");
        assert_eq!(serial(&[0xc3, 0xbc, 0]), None, "not ASCII");
        assert_eq!(
            serial(&[b'x'; SERIAL_MAX]).map(|s| s.len()),
            Some(SERIAL_MAX)
        );
        assert_eq!(serial(&[b'x'; SERIAL_MAX + 1]), None);
    }
}
