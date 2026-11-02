//! Identify data (NVMe Base 2.0, 5.17.2; NVM Command Set 1.0, 4.1.5): the controller,
//! the active namespace list and a namespace, each one 4 KiB page the controller
//! writes. Every field a size, an index or a count is taken from is checked here.

use alloc::{format, string::String, vec::Vec};

use crate::{MAX_TRANSFER_PAGES, NvmeError, PAGE};

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// An ASCII field, its trailing spaces and NULs dropped, any other byte that is not
/// printable shown as `?`.
fn text(b: &[u8]) -> String {
    let s: String = b
        .iter()
        .map(|&c| {
            if c.is_ascii_graphic() || c == b' ' {
                c as char
            } else {
                '?'
            }
        })
        .collect();
    String::from(s.trim_end_matches([' ', '?']))
}

fn page(bytes: &[u8]) -> Result<&[u8], NvmeError> {
    bytes
        .get(..PAGE)
        .ok_or_else(|| NvmeError::Malformed(format!("an Identify page of {} bytes", bytes.len())))
}

/// What Identify Controller says that a block driver uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerId {
    pub vendor: u16,
    pub serial: String,
    pub model: String,
    pub firmware: String,
    /// The largest transfer, in 4 KiB pages, at most [`MAX_TRANSFER_PAGES`].
    pub max_pages: u32,
    /// The controller has a volatile write cache, which Flush writes out.
    pub write_cache: bool,
    /// The most namespace identifiers.
    pub namespaces: u32,
}

impl ControllerId {
    /// Parses Identify Controller. A controller that is not an I/O controller, or
    /// whose queue entry sizes cannot be 64 and 16 bytes, is refused.
    pub fn parse(bytes: &[u8]) -> Result<ControllerId, NvmeError> {
        let b = page(bytes)?;
        let controller_type = b[111];
        if !matches!(controller_type, 0 | 1) {
            return Err(NvmeError::Unsupported(format!(
                "controller type {controller_type} (1 is an I/O controller)"
            )));
        }
        let (sqes, cqes) = (b[512], b[513]);
        let fits = |v: u8, want: u8| v & 0xf <= want && want <= v >> 4;
        if !fits(sqes, 6) || !fits(cqes, 4) {
            return Err(NvmeError::Unsupported(format!(
                "queue entry sizes {sqes:#04x} and {cqes:#04x}, which do not allow 64 and 16 bytes"
            )));
        }
        // MDTS is a power of two of the smallest page (4 KiB here); zero is no limit.
        let mdts = b[77];
        let max_pages = match mdts {
            0 => MAX_TRANSFER_PAGES,
            n if n >= 9 => MAX_TRANSFER_PAGES,
            n => (1u32 << n).min(MAX_TRANSFER_PAGES),
        };
        Ok(ControllerId {
            vendor: u16::from_le_bytes([b[0], b[1]]),
            serial: text(&b[4..24]),
            model: text(&b[24..64]),
            firmware: text(&b[64..72]),
            max_pages,
            write_cache: b[525] & 1 != 0,
            namespaces: u32_at(b, 516),
        })
    }
}

/// Parses the active namespace list: identifiers in increasing order up to the first
/// zero, each at most `most` (Identify Controller's NN).
pub fn active_namespaces(bytes: &[u8], most: u32) -> Result<Vec<u32>, NvmeError> {
    let b = page(bytes)?;
    let mut ids = Vec::new();
    for at in (0..PAGE).step_by(4) {
        let id = u32_at(b, at);
        if id == 0 {
            break;
        }
        if id > most || ids.last().is_some_and(|last| *last >= id) || id == u32::MAX {
            return Err(NvmeError::Malformed(format!(
                "an active namespace list naming {id} after {:?} (at most {most})",
                ids.last()
            )));
        }
        ids.push(id);
    }
    Ok(ids)
}

/// What Identify Namespace says that a block driver uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceId {
    /// The namespace's size in logical blocks (NSZE).
    pub blocks: u64,
    /// A logical block is `1 << block_shift` bytes: 9 to 12.
    pub block_shift: u8,
    /// The namespace is write protected (NSATTR bit 0).
    pub read_only: bool,
}

impl NamespaceId {
    /// Parses Identify Namespace. A namespace that is not active (all zero), a format
    /// index beyond the formats it lists, blocks outside 512 to 4096 bytes, and a
    /// format with metadata are refused.
    pub fn parse(bytes: &[u8]) -> Result<NamespaceId, NvmeError> {
        let b = page(bytes)?;
        let blocks = u64_at(b, 0);
        let capacity = u64_at(b, 8);
        if blocks == 0 {
            return Err(NvmeError::Unsupported(String::from(
                "an inactive namespace (its size is zero)",
            )));
        }
        if capacity > blocks {
            return Err(NvmeError::Malformed(format!(
                "a namespace of {blocks} blocks with a capacity of {capacity}"
            )));
        }
        let formats = b[25] as usize + 1;
        let flbas = b[26];
        let mut index = (flbas & 0xf) as usize;
        if formats > 16 {
            index |= (((flbas >> 5) & 0x3) as usize) << 4;
        }
        if index >= formats || index >= 64 {
            return Err(NvmeError::Malformed(format!(
                "format {index} in use of {formats} listed"
            )));
        }
        let lbaf = u32_at(b, 128 + 4 * index);
        let metadata = lbaf as u16;
        let block_shift = (lbaf >> 16) as u8;
        if metadata != 0 {
            return Err(NvmeError::Unsupported(format!(
                "a format with {metadata} bytes of metadata a block"
            )));
        }
        if !(9..=12).contains(&block_shift) {
            return Err(NvmeError::Unsupported(format!(
                "blocks of 2^{block_shift} bytes (512 to 4096 are served)"
            )));
        }
        if blocks
            .checked_shl(block_shift as u32)
            .is_none_or(|b| b >> block_shift != blocks)
        {
            return Err(NvmeError::Malformed(format!(
                "a namespace of {blocks} blocks of 2^{block_shift} bytes"
            )));
        }
        Ok(NamespaceId {
            blocks,
            block_shift,
            read_only: b[99] & 1 != 0,
        })
    }
}

/// Bytes of a namespace's block.
pub fn block_bytes(ns: &NamespaceId) -> u32 {
    1 << ns.block_shift
}
