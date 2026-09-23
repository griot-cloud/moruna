//! The Moruna aligned binary format `MRB1` (contracts e.4): the header reader and writer over
//! byte slices only, no IO. Used by `TensorSink`, `TensorSource` and staging segments for
//! tensor payloads. Little-endian throughout.

use crate::error::MorunaError;
use crate::payload::DType;

/// The magic at offset 0.
pub const MAGIC: [u8; 4] = *b"MRB1";
/// The only version this build writes or accepts.
pub const VERSION: u16 = 1;
/// The largest rank the format encodes.
pub const MAX_NDIM: usize = 8;

fn bad(msg: impl Into<String>) -> MorunaError {
    MorunaError::Io {
        op: "mrb1",
        target: "header".into(),
        msg: msg.into(),
    }
}

/// A parsed `MRB1` header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Element type.
    pub dtype: DType,
    /// Shape; `ndim` entries, rank 0 to 8.
    pub shape: Vec<i64>,
    /// Absolute byte offset of the payload; the smallest multiple of `page_bytes` that is at or
    /// above the header end.
    pub data_offset: u64,
}

impl Header {
    /// The header's own length before padding: `8 + 8 * ndim + 8` bytes (e.4).
    pub fn header_end(ndim: usize) -> u64 {
        16 + 8 * ndim as u64
    }

    /// The `data_offset` a header of this rank takes for `page_bytes`: the smallest multiple of
    /// `page_bytes` at or above the header end.
    pub fn data_offset_for(ndim: usize, page_bytes: u64) -> crate::Result<u64> {
        if page_bytes == 0 {
            return Err(bad("page size is zero"));
        }
        let end = Self::header_end(ndim);
        Ok(end.div_ceil(page_bytes) * page_bytes)
    }

    /// Element count: the product of the shape; 1 for rank 0.
    pub fn element_count(&self) -> u64 {
        self.shape.iter().map(|d| *d as u64).product()
    }

    /// Payload bytes: `element_count * item_size`.
    pub fn payload_len(&self) -> u64 {
        self.element_count() * self.dtype.item_size() as u64
    }

    /// The end of the payload: `data_offset + payload_len`.
    pub fn payload_end(&self) -> u64 {
        self.data_offset + self.payload_len()
    }

    /// Write the header into `out`, whose length must be at least `data_offset` (the header and
    /// its zero padding). Returns `data_offset`, where the payload begins.
    pub fn write(&self, out: &mut [u8]) -> crate::Result<u64> {
        if self.shape.len() > MAX_NDIM {
            return Err(bad(format!("ndim {} exceeds {MAX_NDIM}", self.shape.len())));
        }
        if self.shape.iter().any(|d| *d < 0) {
            return Err(bad(format!("negative dimension in shape {:?}", self.shape)));
        }
        if !self.data_offset.is_multiple_of(4096) {
            return Err(bad(format!(
                "data offset {} is not a multiple of 4096",
                self.data_offset
            )));
        }
        if self.data_offset < Self::header_end(self.shape.len()) {
            return Err(bad(format!(
                "data offset {} is inside the header (ends at {})",
                self.data_offset,
                Self::header_end(self.shape.len())
            )));
        }
        let end = self.data_offset as usize;
        if out.len() < end {
            return Err(bad(format!(
                "buffer of {} bytes is short of {end}",
                out.len()
            )));
        }
        out[..end].fill(0);
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6] = self.dtype.code();
        out[7] = self.shape.len() as u8;
        for (i, d) in self.shape.iter().enumerate() {
            out[8 + 8 * i..16 + 8 * i].copy_from_slice(&d.to_le_bytes());
        }
        let off = 8 + 8 * self.shape.len();
        out[off..off + 8].copy_from_slice(&self.data_offset.to_le_bytes());
        Ok(self.data_offset)
    }

    /// Parse a header from the start of `buf`, validating magic, version, ndim, that
    /// `data_offset` is a multiple of 4096 and at or above the header end, and that
    /// `buf.len() >= data_offset + payload_len`. Any failure is `Io { op: "mrb1" }`; an unknown
    /// version is rejected, not skipped.
    pub fn read(buf: &[u8]) -> crate::Result<Header> {
        if buf.len() < 16 {
            return Err(bad(format!(
                "buffer of {} bytes is shorter than a header",
                buf.len()
            )));
        }
        if buf[0..4] != MAGIC {
            return Err(bad("magic is not MRB1"));
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != VERSION {
            return Err(bad(format!("unknown version {version}")));
        }
        let Some(dtype) = DType::from_code(buf[6]) else {
            return Err(bad(format!("unknown dtype code {}", buf[6])));
        };
        let ndim = buf[7] as usize;
        if ndim > MAX_NDIM {
            return Err(bad(format!("ndim {ndim} exceeds {MAX_NDIM}")));
        }
        let end = Self::header_end(ndim) as usize;
        if buf.len() < end {
            return Err(bad(format!(
                "buffer of {} bytes is short of the {end}-byte header",
                buf.len()
            )));
        }
        let mut shape = Vec::with_capacity(ndim);
        for i in 0..ndim {
            let mut d = [0u8; 8];
            d.copy_from_slice(&buf[8 + 8 * i..16 + 8 * i]);
            let d = i64::from_le_bytes(d);
            if d < 0 {
                return Err(bad(format!("negative dimension {d} at axis {i}")));
            }
            shape.push(d);
        }
        let mut off = [0u8; 8];
        off.copy_from_slice(&buf[8 + 8 * ndim..16 + 8 * ndim]);
        let data_offset = u64::from_le_bytes(off);
        if !data_offset.is_multiple_of(4096) {
            return Err(bad(format!(
                "data offset {data_offset} is not a multiple of 4096"
            )));
        }
        if data_offset < Self::header_end(ndim) {
            return Err(bad(format!(
                "data offset {data_offset} is inside the header"
            )));
        }
        let header = Header {
            dtype,
            shape,
            data_offset,
        };
        let needed = header.payload_end();
        if (buf.len() as u64) < needed {
            return Err(bad(format!(
                "buffer of {} bytes is short of the {needed} bytes the payload needs",
                buf.len()
            )));
        }
        Ok(header)
    }

    /// The payload bytes of a validated `MRB1` buffer.
    pub fn payload<'a>(&self, buf: &'a [u8]) -> crate::Result<&'a [u8]> {
        let start = self.data_offset as usize;
        let end = self.payload_end() as usize;
        buf.get(start..end)
            .ok_or_else(|| bad("payload is past the end of the buffer"))
    }
}

/// The bytes a whole `MRB1` record occupies for a tensor of this rank and size: the header,
/// its padding to `data_offset`, the payload, and the padding to the next 64-byte boundary
/// (e.4, CT-I9).
pub fn record_len(ndim: usize, payload_len: u64, page_bytes: u64) -> crate::Result<u64> {
    let data_offset = Header::data_offset_for(ndim, page_bytes)?;
    let end = data_offset + payload_len;
    Ok(end.div_ceil(crate::ids::ALIGNMENT as u64) * crate::ids::ALIGNMENT as u64)
}
