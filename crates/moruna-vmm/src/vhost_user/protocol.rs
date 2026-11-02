//! The vhost-user messages the vsock backend speaks (the vhost-user protocol of QEMU's
//! `docs/interop/vhost-user.rst`): their layout, read and written as bytes, so every target
//! tests them. The transport (the socket, its file descriptors, guest memory) is the Linux
//! half, [`super::backend`].

/// A message's header: `u32` request, `u32` flags, `u32` payload size, little endian.
pub const HEADER_BYTES: usize = 12;
/// The protocol's version, in the low bits of every message's flags.
pub const VERSION: u32 = 0x1;
/// Flags: the message is a reply; the front end wants one.
pub const REPLY: u32 = 0x4;
/// The front end asks for a reply to a request that has none of its own (`REPLY_ACK`).
pub const NEED_REPLY: u32 = 0x8;
/// The most payload a message of this backend carries: a memory table of 8 regions.
pub const PAYLOAD_MAX: usize = 8 + 8 * REGION_BYTES;
/// The most regions a memory table holds, and so the most descriptors one message carries.
pub const REGIONS_MAX: usize = 8;
/// One region of a memory table.
pub const REGION_BYTES: usize = 32;

/// The requests (front end to back end), each the number vhost-user gives the request of its
/// name (`VHOST_USER_` and the name).
#[allow(missing_docs)]
pub mod req {
    pub const GET_FEATURES: u32 = 1;
    pub const SET_FEATURES: u32 = 2;
    pub const SET_OWNER: u32 = 3;
    pub const RESET_OWNER: u32 = 4;
    pub const SET_MEM_TABLE: u32 = 5;
    pub const SET_LOG_BASE: u32 = 6;
    pub const SET_LOG_FD: u32 = 7;
    pub const SET_VRING_NUM: u32 = 8;
    pub const SET_VRING_ADDR: u32 = 9;
    pub const SET_VRING_BASE: u32 = 10;
    pub const GET_VRING_BASE: u32 = 11;
    pub const SET_VRING_KICK: u32 = 12;
    pub const SET_VRING_CALL: u32 = 13;
    pub const SET_VRING_ERR: u32 = 14;
    pub const GET_PROTOCOL_FEATURES: u32 = 15;
    pub const SET_PROTOCOL_FEATURES: u32 = 16;
    pub const GET_QUEUE_NUM: u32 = 17;
    pub const SET_VRING_ENABLE: u32 = 18;
    pub const GET_CONFIG: u32 = 24;
    pub const SET_CONFIG: u32 = 25;
}

/// `VHOST_USER_F_PROTOCOL_FEATURES`: the protocol features are negotiated, and rings start
/// disabled until `SET_VRING_ENABLE`.
pub const F_PROTOCOL_FEATURES: u64 = 1 << 30;
/// `VHOST_USER_PROTOCOL_F_CONFIG`: the front end reads the device's configuration space
/// (the guest's cid) from the back end.
pub const PF_CONFIG: u64 = 1 << 9;
/// A vring index's flag in `SET_VRING_KICK`, `CALL` and `ERR`: no descriptor is sent.
pub const VRING_NOFD: u64 = 1 << 8;

/// Why a message is not one this backend reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed(pub String);

fn bad<T>(what: impl Into<String>) -> Result<T, Malformed> {
    Err(Malformed(what.into()))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, Malformed> {
    b.get(at..at + 4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .ok_or_else(|| Malformed(format!("a payload of {} bytes, read at {at}", b.len())))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, Malformed> {
    b.get(at..at + 8)
        .map(|w| {
            let mut x = [0u8; 8];
            x.copy_from_slice(w);
            u64::from_le_bytes(x)
        })
        .ok_or_else(|| Malformed(format!("a payload of {} bytes, read at {at}", b.len())))
}

/// A message's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The request.
    pub request: u32,
    /// The version and the reply flags.
    pub flags: u32,
    /// The payload's length in bytes.
    pub size: u32,
}

impl Header {
    /// The header in `b`, of a version this backend speaks, with a payload it can hold.
    pub fn decode(b: &[u8; HEADER_BYTES]) -> Result<Header, Malformed> {
        let h = Header {
            request: u32_at(b, 0)?,
            flags: u32_at(b, 4)?,
            size: u32_at(b, 8)?,
        };
        if h.flags & 0x3 != VERSION {
            return bad(format!(
                "request {} of version {}",
                h.request,
                h.flags & 0x3
            ));
        }
        if h.size as usize > PAYLOAD_MAX {
            return bad(format!(
                "request {} with {} bytes of payload",
                h.request, h.size
            ));
        }
        Ok(h)
    }
}

/// A reply to `request` carrying `payload`.
pub fn reply(request: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&request.to_le_bytes());
    out.extend_from_slice(&(VERSION | REPLY).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A payload of one `u64`.
pub fn u64_payload(b: &[u8]) -> Result<u64, Malformed> {
    if b.len() != 8 {
        return bad(format!("a u64 payload of {} bytes", b.len()));
    }
    u64_at(b, 0)
}

/// A vring's state (`SET_VRING_NUM`, `SET_VRING_BASE`, `GET_VRING_BASE`, `SET_VRING_ENABLE`):
/// its index and a number.
pub fn vring_state(b: &[u8]) -> Result<(u32, u32), Malformed> {
    if b.len() != 8 {
        return bad(format!("a vring state of {} bytes", b.len()));
    }
    Ok((u32_at(b, 0)?, u32_at(b, 4)?))
}

/// The state as a reply carries it.
pub fn vring_state_bytes(index: u32, num: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&index.to_le_bytes());
    b[4..].copy_from_slice(&num.to_le_bytes());
    b
}

/// A vring's addresses (`SET_VRING_ADDR`), in the front end's address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VringAddr {
    /// The vring.
    pub index: u32,
    /// Its flags (logging, which this back end does not do).
    pub flags: u32,
    /// The descriptor table.
    pub desc: u64,
    /// The device (used) ring.
    pub used: u64,
    /// The driver (available) ring.
    pub avail: u64,
    /// Where writes are logged, for migration.
    pub log: u64,
}

/// `SET_VRING_ADDR`'s payload.
pub fn vring_addr(b: &[u8]) -> Result<VringAddr, Malformed> {
    if b.len() != 40 {
        return bad(format!("a vring address of {} bytes", b.len()));
    }
    Ok(VringAddr {
        index: u32_at(b, 0)?,
        flags: u32_at(b, 4)?,
        desc: u64_at(b, 8)?,
        used: u64_at(b, 16)?,
        avail: u64_at(b, 24)?,
        log: u64_at(b, 32)?,
    })
}

/// One region of guest memory the front end shares: its guest address, its size, where it
/// is in the front end's address space, and where in its descriptor's file it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// Its guest physical address.
    pub guest: u64,
    /// Its length in bytes.
    pub size: u64,
    /// Where the front end maps it.
    pub user: u64,
    /// Where it starts in the file its descriptor names.
    pub offset: u64,
}

/// `SET_MEM_TABLE`'s regions, as many as `fds` descriptors arrived with it.
pub fn mem_table(b: &[u8], fds: usize) -> Result<Vec<Region>, Malformed> {
    let n = u32_at(b, 0)? as usize;
    if n == 0 || n > REGIONS_MAX || n != fds {
        return bad(format!(
            "a memory table of {n} regions with {fds} descriptors"
        ));
    }
    if b.len() != 8 + n * REGION_BYTES {
        return bad(format!(
            "a memory table of {n} regions in {} bytes",
            b.len()
        ));
    }
    (0..n)
        .map(|i| {
            let at = 8 + i * REGION_BYTES;
            let r = Region {
                guest: u64_at(b, at)?,
                size: u64_at(b, at + 8)?,
                user: u64_at(b, at + 16)?,
                offset: u64_at(b, at + 24)?,
            };
            if r.size == 0
                || r.guest.checked_add(r.size).is_none()
                || r.user.checked_add(r.size).is_none()
            {
                return bad(format!("region {i}: {r:?}"));
            }
            Ok(r)
        })
        .collect()
}

/// The guest address of the front end's address `user`, through the region holding it.
pub fn translate(regions: &[Region], user: u64) -> Option<u64> {
    regions
        .iter()
        .find(|r| user >= r.user && user - r.user < r.size)
        .map(|r| r.guest + (user - r.user))
}

/// `GET_CONFIG`'s request: the offset and size of the configuration space asked for.
pub fn config_request(b: &[u8]) -> Result<(u32, u32, u32), Malformed> {
    let offset = u32_at(b, 0)?;
    let size = u32_at(b, 4)?;
    let flags = u32_at(b, 8)?;
    if b.len() != 12 + size as usize || size > 256 {
        return bad(format!("a config request of {size} bytes in {}", b.len()));
    }
    Ok((offset, size, flags))
}

/// `GET_CONFIG`'s reply: the request's offset, size and flags, then the bytes.
pub fn config_reply(offset: u32, flags: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(request: u32, flags: u32, size: u32) -> [u8; HEADER_BYTES] {
        let mut b = [0u8; HEADER_BYTES];
        b[..4].copy_from_slice(&request.to_le_bytes());
        b[4..8].copy_from_slice(&flags.to_le_bytes());
        b[8..].copy_from_slice(&size.to_le_bytes());
        b
    }

    #[test]
    fn kd2_t11_headers_and_replies() {
        let h = Header::decode(&header(req::GET_FEATURES, VERSION, 0)).unwrap();
        assert_eq!((h.request, h.size), (req::GET_FEATURES, 0));
        assert!(Header::decode(&header(1, 2, 0)).is_err(), "another version");
        assert!(
            Header::decode(&header(1, VERSION, 4096)).is_err(),
            "past the payload bound"
        );
        let r = reply(req::GET_FEATURES, &7u64.to_le_bytes());
        assert_eq!(&r[..4], &1u32.to_le_bytes());
        assert_eq!(&r[4..8], &(VERSION | REPLY).to_le_bytes());
        assert_eq!(&r[8..12], &8u32.to_le_bytes());
        assert_eq!(u64_payload(&r[12..]), Ok(7));
        assert!(u64_payload(&[0; 4]).is_err());
    }

    #[test]
    fn kd2_t11_vrings_and_memory() {
        assert_eq!(vring_state(&vring_state_bytes(1, 128)), Ok((1, 128)));
        assert!(vring_state(&[0; 7]).is_err());
        let mut a = Vec::new();
        a.extend_from_slice(&1u32.to_le_bytes());
        a.extend_from_slice(&0u32.to_le_bytes());
        for v in [0x7f00_0000_1000u64, 0x7f00_0000_3000, 0x7f00_0000_2000, 0] {
            a.extend_from_slice(&v.to_le_bytes());
        }
        let v = vring_addr(&a).unwrap();
        assert_eq!(
            (v.index, v.desc, v.used, v.avail),
            (1, 0x7f00_0000_1000, 0x7f00_0000_3000, 0x7f00_0000_2000)
        );
        let mut t = Vec::new();
        t.extend_from_slice(&2u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        for r in [
            [0u64, 0x8000_0000, 0x7f00_0000_0000, 0],
            [0x1_0000_0000, 0x1000_0000, 0x7f80_0000_0000, 0x8000_0000],
        ] {
            for v in r {
                t.extend_from_slice(&v.to_le_bytes());
            }
        }
        let regions = mem_table(&t, 2).unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(translate(&regions, 0x7f00_0000_1000), Some(0x1000));
        assert_eq!(translate(&regions, 0x7f80_0000_0010), Some(0x1_0000_0010));
        assert_eq!(
            translate(&regions, 0x7f00_8000_0000),
            None,
            "past the first region"
        );
        assert!(mem_table(&t, 1).is_err(), "a descriptor a region");
        assert!(mem_table(&t[..40], 2).is_err());
        let mut zero = t.clone();
        zero[16..24].copy_from_slice(&0u64.to_le_bytes());
        assert!(mem_table(&zero, 2).is_err(), "an empty region");
    }

    #[test]
    fn kd2_t11_config_space() {
        let mut q = Vec::new();
        for v in [0u32, 8, 0] {
            q.extend_from_slice(&v.to_le_bytes());
        }
        q.extend_from_slice(&[0; 8]);
        assert_eq!(config_request(&q), Ok((0, 8, 0)));
        assert!(config_request(&q[..12]).is_err());
        let r = config_reply(0, 0, &3u64.to_le_bytes());
        assert_eq!(config_request(&r), Ok((0, 8, 0)));
        assert_eq!(&r[12..], &3u64.to_le_bytes());
    }
}
