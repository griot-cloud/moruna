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
    if sector.len() < HEADER_MIN || &sector[..8] != SIGNATURE {
        return None;
    }
    let size = le32(sector, 12) as usize;
    if !(HEADER_MIN..=sector.len()).contains(&size) || le64(sector, 24) != 1 {
        return None;
    }
    let mut header = alloc::vec::Vec::from(&sector[..size]);
    header[16..20].fill(0);
    let crc = crc32(&header);
    if crc != le32(sector, 16) {
        return None;
    }
    let mut guid = [0u8; 16];
    guid.copy_from_slice(&sector[56..72]);
    Some(Guid(guid))
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
