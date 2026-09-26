//! The virtio-vsock packet header (virtio 1.2, 5.10.6): 44 bytes, little-endian.

/// Header size.
pub const HDR_BYTES: usize = 44;
/// Socket type: stream (the only type this device carries).
pub const TYPE_STREAM: u16 = 1;

/// Operations.
pub mod op {
    /// Connect.
    pub const REQUEST: u16 = 1;
    /// Connection accepted.
    pub const RESPONSE: u16 = 2;
    /// Connection refused or torn down.
    pub const RST: u16 = 3;
    /// One side will send or receive no more.
    pub const SHUTDOWN: u16 = 4;
    /// Data.
    pub const RW: u16 = 5;
    /// Unsolicited credit update.
    pub const CREDIT_UPDATE: u16 = 6;
    /// Please send a credit update.
    pub const CREDIT_REQUEST: u16 = 7;
}

/// `SHUTDOWN` flag: the sender will receive no more.
pub const SHUTDOWN_RCV: u32 = 1;
/// `SHUTDOWN` flag: the sender will send no more.
pub const SHUTDOWN_SEND: u32 = 2;

/// A decoded header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// Source context id.
    pub src_cid: u64,
    /// Destination context id.
    pub dst_cid: u64,
    /// Source port.
    pub src_port: u32,
    /// Destination port.
    pub dst_port: u32,
    /// Payload length.
    pub len: u32,
    /// Socket type.
    pub type_: u16,
    /// Operation.
    pub op: u16,
    /// Operation flags.
    pub flags: u32,
    /// The sender's receive buffer size.
    pub buf_alloc: u32,
    /// Bytes the sender has consumed from its receive buffer, ever.
    pub fwd_cnt: u32,
}

impl Header {
    /// Decode 44 bytes.
    pub fn decode(b: &[u8; HDR_BYTES]) -> Self {
        let u64_at = |o: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[o..o + 8]);
            u64::from_le_bytes(a)
        };
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Header {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            type_: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    /// Encode to 44 bytes.
    pub fn encode(&self) -> [u8; HDR_BYTES] {
        let mut b = [0u8; HDR_BYTES];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.type_.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_t10_header_round_trip_and_layout() {
        let h = Header {
            src_cid: 3,
            dst_cid: 2,
            src_port: 1025,
            dst_port: 5000,
            len: 17,
            type_: TYPE_STREAM,
            op: op::RW,
            flags: SHUTDOWN_SEND,
            buf_alloc: 262_144,
            fwd_cnt: 99,
        };
        let b = h.encode();
        assert_eq!(Header::decode(&b), h);
        // Field offsets, byte-exact against the specification's struct.
        assert_eq!(&b[0..8], &3u64.to_le_bytes());
        assert_eq!(&b[16..20], &1025u32.to_le_bytes());
        assert_eq!(&b[24..28], &17u32.to_le_bytes());
        assert_eq!(&b[30..32], &op::RW.to_le_bytes());
        assert_eq!(&b[40..44], &99u32.to_le_bytes());
        assert_eq!(Header::decode(&[0; HDR_BYTES]), Header::default());
    }
}
