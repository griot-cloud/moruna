//! Tiers, the places bytes physically live (contracts d.2, f.6).

use crate::ids::NodeId;

/// Where a payload's bytes physically are.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Tier {
    /// Accelerator memory on the given device.
    Device(crate::ids::DeviceId),
    /// Page-locked host memory; a valid DMA source and target.
    PinnedHost,
    /// Ordinary host memory.
    Host,
    /// A staging segment on local disk; the payload has no resident bytes.
    Disk(SegmentRef),
    /// Registered memory on another node of the same run, reachable by one-sided
    /// RDMA through the reactor. Reserved: no v1 component produces this variant,
    /// and every v1 component that matches on `Tier` handles it by returning
    /// `AmoruError::Unsupported("rdma")` rather than by a wildcard arm (CT-I11).
    Remote(NodeId, RemoteRef),
}

/// Location of a payload in another node's registered memory. Fields are those
/// a one-sided RDMA read needs and nothing else; the memory key is opaque here.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct RemoteRef {
    /// Virtual address on the owning node.
    pub addr: u64,
    /// Remote memory key as registered with the owning node's NIC.
    pub rkey: u32,
    /// Byte length.
    pub len: u64,
}

/// Location of a demoted payload inside a staging segment.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SegmentRef {
    /// Segment file number within the run's staging directory.
    pub segment: u32,
    /// Byte offset of the payload's first byte; a multiple of the page size.
    pub offset: u64,
    /// Byte length of the payload as written.
    pub len: u64,
}

/// A tier without its payload (no device id, no segment, no remote ref); what a
/// water mark, a knob or a per-tier array is keyed by. `Tier::kind()` maps to it and
/// `TierKind::index()` equals `Tier::index()` for the corresponding tier.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum TierKind {
    /// Accelerator memory.
    Device,
    /// Page-locked host memory.
    PinnedHost,
    /// Ordinary host memory.
    Host,
    /// A staging segment on local disk.
    Disk,
    /// Registered memory on another node (reserved).
    Remote,
}

/// How a staging segment's records are encoded. `Raw` is the payload's in-memory
/// layout written as is (page-aligned Arrow IPC or AMB1), moved by DMA with no CPU
/// in the path (PL-I4). Reserved for a compressed variant (a Vortex-encoded record,
/// architecture 5.6) that trades CPU on the demotion path for disk bytes when the
/// controller decides the run is disk-bound with idle cores; every v1 `match`
/// handles the enum explicitly (CT-I11) and the segment record header carries
/// the codec byte, so adding the variant changes no layout.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub enum StagingCodec {
    /// The in-memory layout written as is.
    #[default]
    Raw,
}

impl StagingCodec {
    /// The codec byte written in a segment record header (09 e.3): `Raw` is 0.
    pub fn code(&self) -> u8 {
        match self {
            StagingCodec::Raw => 0,
        }
    }

    /// The codec for a header byte; `None` for a value this build does not know, which a
    /// reader reports as `Unsupported("codec")`.
    pub fn from_code(code: u8) -> Option<StagingCodec> {
        match code {
            0 => Some(StagingCodec::Raw),
            _ => None,
        }
    }
}

/// Length of every per-tier array in the contract (`bytes_by_tier`, water marks, reservations).
pub const TIER_COUNT: usize = 5;

impl Tier {
    /// True for Device, PinnedHost and Host; false for Disk and Remote.
    pub fn is_resident(&self) -> bool {
        match self {
            Tier::Device(_) | Tier::PinnedHost | Tier::Host => true,
            Tier::Disk(_) => false,
            Tier::Remote(_, _) => false,
        }
    }

    /// Ordering used by the placement engine: Device > PinnedHost > Host > Remote > Disk
    /// (Device = 4, PinnedHost = 3, Host = 2, Remote = 1, Disk = 0; f.6).
    pub fn rank(&self) -> u8 {
        match self {
            Tier::Device(_) => 4,
            Tier::PinnedHost => 3,
            Tier::Host => 2,
            Tier::Disk(_) => 0,
            Tier::Remote(_, _) => 1,
        }
    }

    /// Index into per-tier arrays: Device = 0, PinnedHost = 1, Host = 2, Disk = 3, Remote = 4.
    pub fn index(&self) -> usize {
        self.kind().index()
    }

    /// The tier without its payload.
    pub fn kind(&self) -> TierKind {
        match self {
            Tier::Device(_) => TierKind::Device,
            Tier::PinnedHost => TierKind::PinnedHost,
            Tier::Host => TierKind::Host,
            Tier::Disk(_) => TierKind::Disk,
            Tier::Remote(_, _) => TierKind::Remote,
        }
    }

    /// True for the two host tiers, `Host` and `PinnedHost` (the tiers whose bytes a CPU can address).
    pub fn is_host(&self) -> bool {
        match self {
            Tier::PinnedHost | Tier::Host => true,
            Tier::Device(_) => false,
            Tier::Disk(_) => false,
            Tier::Remote(_, _) => false,
        }
    }
}

impl TierKind {
    /// Index into per-tier arrays: Device = 0, PinnedHost = 1, Host = 2, Disk = 3, Remote = 4.
    pub fn index(&self) -> usize {
        match self {
            TierKind::Device => 0,
            TierKind::PinnedHost => 1,
            TierKind::Host => 2,
            TierKind::Disk => 3,
            TierKind::Remote => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_byte_round_trip() {
        assert_eq!(StagingCodec::Raw.code(), 0);
        assert_eq!(StagingCodec::from_code(0), Some(StagingCodec::Raw));
        assert_eq!(StagingCodec::from_code(7), None);
        assert_eq!(StagingCodec::default(), StagingCodec::Raw);
    }

    #[test]
    fn host_tiers() {
        assert!(Tier::Host.is_host());
        assert!(Tier::PinnedHost.is_host());
        assert!(!Tier::Device(crate::ids::DeviceId(0)).is_host());
        let seg = SegmentRef {
            segment: 1,
            offset: 4096,
            len: 10,
        };
        assert!(!Tier::Disk(seg).is_host());
        let r = RemoteRef {
            addr: 1,
            rkey: 2,
            len: 3,
        };
        assert!(!Tier::Remote(NodeId(1), r).is_host());
    }
}
