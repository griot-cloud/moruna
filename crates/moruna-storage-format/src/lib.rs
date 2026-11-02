//! The single durable storage codec and identity authority (SDD32), usable by host
//! adapters and a no_std Moruna-node service. No filesystem, runtime, SDK or credentials.
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
extern crate alloc;
pub mod auth;
pub mod banks;
pub mod bindings;
#[cfg(feature = "crypto")]
pub mod cache;
pub mod config;
pub mod grants;
pub mod journal;
pub mod namespace;
pub mod provider;
pub mod wire;
use alloc::{string::String, vec::Vec};
use core::fmt;

/// Canonical inner-record major. Raw media additionally requires banks::VERSION roots.
pub const VERSION: u32 = 1;
/// Fixed frame header: magic, major, payload size, BLAKE3 payload digest.
pub const HEADER: usize = 48;
/// Metadata ceiling, independent from object payload size.
pub const MAX_PAYLOAD: usize = 1 << 20;
const MAGIC: &[u8; 8] = b"MORUSTO1";

/// A format refusal, without platform-specific error types or secret data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Incomplete bytes; a log writer may discard an unacknowledged truncated tail.
    Incomplete,
    /// Unknown major version, never a compatibility fallback.
    Version,
    /// Invalid identity, geometry, tag or reserved bytes.
    Invalid,
    /// Metadata or count exceeds the format ceiling.
    Limit,
    /// A complete frame's checksum does not match.
    Integrity,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Incomplete => "incomplete storage frame",
            Self::Version => "unsupported storage major",
            Self::Invalid => "invalid storage format",
            Self::Limit => "storage metadata limit",
            Self::Integrity => "storage frame checksum mismatch",
        })
    }
}
impl core::error::Error for Error {}
/// Maximum explicitly granted versioned native secret key page, independent of whether
/// a consumer enables crypto. The layout/encoding itself remains cache::WorkspaceKey.
pub const MAX_KEY_PAGE: usize = 4096;
/// Format operation result.
pub type Result<T> = core::result::Result<T, Error>;

/// Exact workspace/bucket/key identity, with no filesystem interpretation.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Object {
    /// Resource-owning workspace, distinct from the requester's authorization/cache scope.
    pub workspace: String,
    /// Explicit home bucket.
    pub bucket: String,
    /// Object key; empty is legal only for prefix listing.
    pub key: String,
}
impl Object {
    /// Reject malformed/oversized identities before IO.
    pub fn validate(&self) -> Result<()> {
        Self::validate_parts(&self.workspace, &self.bucket, &self.key)
    }
    /// Shared borrowed identity validation, including superblock ownership before any IO.
    pub fn validate_parts(workspace: &str, bucket: &str, key: &str) -> Result<()> {
        if workspace.is_empty()
            || bucket.is_empty()
            || key.starts_with('/')
            || key.split('/').any(|p| p == "." || p == "..")
            || [workspace, bucket, key]
                .iter()
                .any(|s| s.contains('\0') || s.len() > MAX_PAYLOAD)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
/// Nonsecret requester budget/cache scope, distinct from a resource owner identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Scope {
    /// Requesting workspace.
    pub workspace: String,
    /// Run whose ledger reserves ingress staging.
    pub run: String,
}
impl Scope {
    /// Reject absent or oversized requester identities without copying their strings.
    pub fn validate_parts(workspace: &str, run: &str) -> Result<()> {
        if [workspace, run]
            .iter()
            .any(|s| s.is_empty() || s.len() > MAX_PAYLOAD || s.contains(char::from(0)))
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Validate this complete requester identity.
    pub fn validate(&self) -> Result<()> {
        Self::validate_parts(&self.workspace, &self.run)
    }
}

/// Preserve both backend conditional identities, without interpreting entity tags.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Version {
    /// Backend entity tag.
    pub etag: Option<String>,
    /// Backend generation identity.
    pub generation: Option<String>,
}
impl Version {
    /// Require an actual conditional identity.
    pub fn validate(&self) -> Result<()> {
        if self.etag.as_ref().is_none_or(String::is_empty)
            && self.generation.as_ref().is_none_or(String::is_empty)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
/// Object metadata shared by protocol and durable commit frames.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Metadata {
    /// Exact identity.
    pub object: Object,
    /// Complete conditional identity.
    pub version: Version,
    /// Object payload length, excluding extent padding.
    pub bytes: u64,
}
/// Explicit local role; no cloud-disk or bucket-failure default exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Regular-file format fixture only, never physical device proof.
    FormatProof,
    /// Small durable local checkpoint metadata, not portable home by itself.
    CheckpointMetadata,
    /// Workspace-encrypted node cache/scratch only.
    CacheScratch,
    /// Explicitly configured single-node raw-device home.
    SingleNodeHome,
}
/// A raw volume's immutable format identity and validated geometry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    /// Explicit capacity of the device/fixture.
    pub capacity: u64,
    /// Required block and buffer alignment.
    pub alignment: u32,
    /// Start of the bounded commit log.
    pub log_offset: u64,
    /// Reserved commit log bytes, a multiple of alignment.
    pub log_bytes: u64,
    /// Beginning of contiguous immutable payload extents.
    pub data_offset: u64,
    /// Explicit role.
    pub role: Role,
    /// Owning workspace.
    pub workspace: String,
    /// Explicit home bucket label.
    pub bucket: String,
}
impl Superblock {
    /// Geometry validation shared by Linux and the native-node service.
    pub fn validate(&self) -> Result<()> {
        Object::validate_parts(&self.workspace, &self.bucket, "")?;
        let align = u64::from(self.alignment);
        if self.alignment < 512
            || !self.alignment.is_power_of_two()
            || self.workspace.is_empty()
            || self.bucket.is_empty()
            || self.log_offset < align
            || self.log_bytes == 0
            || self.data_offset >= self.capacity
            || [
                self.capacity,
                self.log_offset,
                self.log_bytes,
                self.data_offset,
            ]
            .iter()
            .any(|n| !n.is_multiple_of(align))
            || self.log_offset.checked_add(self.log_bytes) != Some(self.data_offset)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
/// One level of a block-digest tree; data and tree nodes occupy separate contiguous extents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeLevel {
    /// Byte offset of this level's nodes.
    pub offset: u64,
    /// Number of aligned node pages.
    pub nodes: u64,
}
/// Immutable payload location, interpreted by the platform adapter, never another codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Location {
    /// A filesystem format adapter's immutable data filename, 32 random identity bytes.
    File([u8; 32]),
    /// Contiguous raw volume payload and its authenticated block-digest tree.
    Extent {
        /// Payload start, aligned to the superblock geometry.
        offset: u64,
        /// Payload span including zero tail padding.
        padded_bytes: u64,
        /// Digest tree levels, leaves first.
        tree: Vec<TreeLevel>,
        /// Root node digest recorded in the durable commit.
        root: [u8; 32],
    },
}
/// The one durable object/conditional-commit record shared across adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Commit {
    /// Monotonic local commit sequence.
    pub sequence: u64,
    /// Exact committed object facts.
    pub metadata: Metadata,
    /// Hash of the unpadded object bytes.
    pub content_hash: [u8; 32],
    /// Immutable data location.
    pub location: Location,
}
impl Commit {
    /// Validate extent/tree bounds against the same authoritative superblock on either
    /// platform. Raw homes never accept filesystem-only location records.
    pub fn validate_in(&self, volume: &Superblock) -> Result<()> {
        volume.validate()?;
        self.metadata.object.validate()?;
        self.metadata.version.validate()?;
        if self.metadata.object.workspace != volume.workspace
            || self.metadata.object.bucket != volume.bucket
            || self.metadata.object.key.is_empty()
        {
            return Err(Error::Invalid);
        }
        match &self.location {
            Location::File(_) => match volume.role {
                Role::FormatProof | Role::CheckpointMetadata => Ok(()),
                Role::CacheScratch | Role::SingleNodeHome => Err(Error::Invalid),
            },
            Location::Extent {
                offset,
                padded_bytes,
                tree,
                root,
            } => {
                let align = u64::from(volume.alignment);
                if !offset.is_multiple_of(align)
                    || !padded_bytes.is_multiple_of(align)
                    || *offset < volume.data_offset
                    || self.metadata.bytes > *padded_bytes
                {
                    return Err(Error::Invalid);
                }
                let mut end = offset.checked_add(*padded_bytes).ok_or(Error::Invalid)?;
                if end > volume.capacity {
                    return Err(Error::Invalid);
                }
                if self.metadata.bytes == 0 {
                    return if *padded_bytes == 0
                        && tree.is_empty()
                        && root == blake3::hash(&[]).as_bytes()
                    {
                        Ok(())
                    } else {
                        Err(Error::Invalid)
                    };
                }
                if self.metadata.bytes.div_ceil(align).checked_mul(align) != Some(*padded_bytes) {
                    return Err(Error::Invalid);
                }
                let fanout = align / 32;
                let mut count = padded_bytes / align;
                for (index, level) in tree.iter().enumerate() {
                    let needed = count.div_ceil(fanout);
                    if level.nodes != needed
                        || level.offset != end
                        || !level.offset.is_multiple_of(align)
                    {
                        return Err(Error::Invalid);
                    }
                    end = level
                        .nodes
                        .checked_mul(align)
                        .and_then(|bytes| end.checked_add(bytes))
                        .ok_or(Error::Invalid)?;
                    if end > volume.capacity || (needed == 1 && index + 1 != tree.len()) {
                        return Err(Error::Invalid);
                    }
                    count = needed;
                }
                if tree.is_empty() || count != 1 {
                    return Err(Error::Invalid);
                }
                Ok(())
            }
        }
    }
}

/// A versioned durable frame. Readers never guess the payload type from a path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// Volume geometry and ownership identity.
    Superblock(Superblock),
    /// One small durable commit after the immutable data has been synced.
    Commit(Commit),
    /// Receipt written and synced only after its complete commit and payload are synced.
    Receipt {
        /// Exact monotonic log position of the committed record.
        sequence: u64,
        /// BLAKE3 of the canonical encoded Commit, excluding cell padding.
        commit_hash: [u8; 32],
    },
}
/// The total encoded frame length, validated before allocating its payload.
pub fn frame_size(header: &[u8]) -> Result<usize> {
    if header.len() < HEADER {
        return Err(Error::Incomplete);
    }
    if header.get(..8) != Some(MAGIC.as_slice()) {
        return Err(Error::Invalid);
    }
    let major = u32::from_le_bytes(array(header.get(8..12).ok_or(Error::Incomplete)?)?);
    if major != VERSION {
        return Err(Error::Version);
    }
    let len = u32::from_le_bytes(array(header.get(12..16).ok_or(Error::Incomplete)?)?) as usize;
    if len == 0 || len > MAX_PAYLOAD {
        return Err(Error::Limit);
    }
    HEADER.checked_add(len).ok_or(Error::Limit)
}
impl Frame {
    /// Encode the canonical bounded frame, without padding. Raw adapters pad only with zeros.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut writer = Writer(Vec::new());
        match self {
            Self::Receipt {
                sequence,
                commit_hash,
            } => {
                writer.u8(3)?;
                writer.u64(*sequence)?;
                writer.bytes(commit_hash)?;
            }
            Self::Superblock(s) => {
                s.validate()?;
                writer.u8(1)?;
                writer.u64(s.capacity)?;
                writer.u32(s.alignment)?;
                writer.u64(s.log_offset)?;
                writer.u64(s.log_bytes)?;
                writer.u64(s.data_offset)?;
                writer.u8(match s.role {
                    Role::FormatProof => 0,
                    Role::CheckpointMetadata => 1,
                    Role::CacheScratch => 2,
                    Role::SingleNodeHome => 3,
                })?;
                writer.text(&s.workspace)?;
                writer.text(&s.bucket)?;
            }
            Self::Commit(c) => {
                c.metadata.object.validate()?;
                c.metadata.version.validate()?;
                if c.metadata.object.key.is_empty() {
                    return Err(Error::Invalid);
                }
                writer.u8(2)?;
                writer.u64(c.sequence)?;
                for text in [
                    &c.metadata.object.workspace,
                    &c.metadata.object.bucket,
                    &c.metadata.object.key,
                ] {
                    writer.text(text)?;
                }
                writer.optional(&c.metadata.version.etag)?;
                writer.optional(&c.metadata.version.generation)?;
                writer.u64(c.metadata.bytes)?;
                writer.bytes(&c.content_hash)?;
                match &c.location {
                    Location::File(id) => {
                        writer.u8(0)?;
                        writer.bytes(id)?;
                    }
                    Location::Extent {
                        offset,
                        padded_bytes,
                        tree,
                        root,
                    } => {
                        if tree.len() > 64 {
                            return Err(Error::Limit);
                        }
                        writer.u8(1)?;
                        writer.u64(*offset)?;
                        writer.u64(*padded_bytes)?;
                        writer.u32(tree.len() as u32)?;
                        for level in tree {
                            writer.u64(level.offset)?;
                            writer.u64(level.nodes)?;
                        }
                        writer.bytes(root)?;
                    }
                }
            }
        }
        let mut frame = Vec::with_capacity(HEADER + writer.0.len());
        frame.extend_from_slice(MAGIC);
        frame.extend_from_slice(&VERSION.to_le_bytes());
        frame.extend_from_slice(&(writer.0.len() as u32).to_le_bytes());
        frame.extend_from_slice(blake3::hash(&writer.0).as_bytes());
        frame.extend_from_slice(&writer.0);
        Ok(frame)
    }
    /// Decode exactly one canonical frame, accepting only zero padding afterward.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let size = frame_size(bytes)?;
        let payload = bytes.get(HEADER..size).ok_or(Error::Incomplete)?;
        if bytes.get(16..48) != Some(blake3::hash(payload).as_bytes().as_slice()) {
            return Err(Error::Integrity);
        }
        if bytes
            .get(size..)
            .is_some_and(|pad| pad.iter().any(|b| *b != 0))
        {
            return Err(Error::Invalid);
        }
        let mut read = Reader {
            bytes: payload,
            at: 0,
        };
        let frame = match read.u8()? {
            1 => {
                let (capacity, alignment, log_offset, log_bytes, data_offset) = (
                    read.u64()?,
                    read.u32()?,
                    read.u64()?,
                    read.u64()?,
                    read.u64()?,
                );
                let role = match read.u8()? {
                    0 => Role::FormatProof,
                    1 => Role::CheckpointMetadata,
                    2 => Role::CacheScratch,
                    3 => Role::SingleNodeHome,
                    _ => return Err(Error::Invalid),
                };
                let s = Superblock {
                    capacity,
                    alignment,
                    log_offset,
                    log_bytes,
                    data_offset,
                    role,
                    workspace: read.text()?,
                    bucket: read.text()?,
                };
                s.validate()?;
                Self::Superblock(s)
            }
            2 => {
                let sequence = read.u64()?;
                let object = Object {
                    workspace: read.text()?,
                    bucket: read.text()?,
                    key: read.text()?,
                };
                object.validate()?;
                if object.key.is_empty() {
                    return Err(Error::Invalid);
                }
                let version = Version {
                    etag: read.optional()?,
                    generation: read.optional()?,
                };
                version.validate()?;
                let bytes = read.u64()?;
                let content_hash = read.array()?;
                let location = match read.u8()? {
                    0 => Location::File(read.array()?),
                    1 => {
                        let offset = read.u64()?;
                        let padded_bytes = read.u64()?;
                        let levels = read.u32()?;
                        if levels > 64 {
                            return Err(Error::Limit);
                        }
                        let mut tree = Vec::with_capacity(levels as usize);
                        for _ in 0..levels {
                            tree.push(TreeLevel {
                                offset: read.u64()?,
                                nodes: read.u64()?,
                            });
                        }
                        Location::Extent {
                            offset,
                            padded_bytes,
                            tree,
                            root: read.array()?,
                        }
                    }
                    _ => return Err(Error::Invalid),
                };
                Self::Commit(Commit {
                    sequence,
                    metadata: Metadata {
                        object,
                        version,
                        bytes,
                    },
                    content_hash,
                    location,
                })
            }
            3 => Self::Receipt {
                sequence: read.u64()?,
                commit_hash: read.array()?,
            },
            _ => return Err(Error::Invalid),
        };
        if read.at != payload.len() {
            return Err(Error::Invalid);
        }
        Ok(frame)
    }
}
fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| Error::Incomplete)
}
struct Writer(Vec<u8>);
impl Writer {
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > MAX_PAYLOAD)
        {
            return Err(Error::Limit);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn u8(&mut self, n: u8) -> Result<()> {
        self.bytes(&[n])
    }
    fn u32(&mut self, n: u32) -> Result<()> {
        self.bytes(&n.to_le_bytes())
    }
    fn u64(&mut self, n: u64) -> Result<()> {
        self.bytes(&n.to_le_bytes())
    }
    fn text(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_PAYLOAD {
            return Err(Error::Limit);
        }
        self.u32(text.len() as u32)?;
        self.bytes(text.as_bytes())
    }
    fn optional(&mut self, text: &Option<String>) -> Result<()> {
        match text {
            Some(text) => {
                self.u8(1)?;
                self.text(text)
            }
            None => self.u8(0),
        }
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Limit)?;
        let bytes = self.bytes.get(self.at..end).ok_or(Error::Incomplete)?;
        self.at = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        array(self.take(N)?)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn text(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        if len > MAX_PAYLOAD {
            return Err(Error::Limit);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| Error::Invalid)
    }
    fn optional(&mut self) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.text()?)),
            _ => Err(Error::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};
    fn commit() -> Commit {
        Commit {
            sequence: 0,
            metadata: Metadata {
                object: Object {
                    workspace: "workspace".to_string(),
                    bucket: "bucket".to_string(),
                    key: "object".to_string(),
                },
                version: Version {
                    etag: Some("etag".to_string()),
                    generation: Some("generation".to_string()),
                },
                bytes: 8,
            },
            content_hash: *blake3::hash(b"12345678").as_bytes(),
            location: Location::File([3; 32]),
        }
    }
    fn volume() -> Superblock {
        Superblock {
            capacity: 1 << 20,
            alignment: 4096,
            log_offset: 4096,
            log_bytes: 4096 * 8,
            data_offset: 4096 * 9,
            role: Role::SingleNodeHome,
            workspace: "workspace".to_string(),
            bucket: "bucket".to_string(),
        }
    }
    #[test]
    fn st_f1_one_codec_roundtrips_host_and_native_records() {
        for frame in [Frame::Commit(commit()), Frame::Superblock(volume())] {
            let mut encoded = frame.encode().unwrap();
            assert_eq!(Frame::decode(&encoded).unwrap(), frame);
            encoded.extend_from_slice(&[0; 16]);
            assert_eq!(Frame::decode(&encoded).unwrap(), frame);
            *encoded.last_mut().unwrap() = 1;
            assert_eq!(Frame::decode(&encoded), Err(Error::Invalid));
        }
    }
    #[test]
    fn st_f2_every_truncation_corruption_and_unknown_version_is_refused() {
        let encoded = Frame::Commit(commit()).encode().unwrap();
        for end in 0..encoded.len() {
            assert!(Frame::decode(&encoded[..end]).is_err(), "cut at {end}");
        }
        for at in 0..encoded.len() {
            let mut bad = encoded.clone();
            bad[at] ^= 0x40;
            assert!(Frame::decode(&bad).is_err(), "corruption at {at}");
        }
        let mut future = encoded.clone();
        future[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(Frame::decode(&future), Err(Error::Version));
        let mut large = encoded;
        large[12..16].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(frame_size(&large), Err(Error::Limit));
    }
    #[test]
    fn st_f3_raw_geometry_and_digest_tree_have_one_bounds_reader() {
        let volume = volume();
        let mut record = commit();
        assert_eq!(record.validate_in(&volume), Err(Error::Invalid));
        record.location = Location::Extent {
            offset: volume.data_offset,
            padded_bytes: 4096,
            tree: vec![TreeLevel {
                offset: volume.data_offset + 4096,
                nodes: 1,
            }],
            root: [5; 32],
        };
        assert_eq!(record.validate_in(&volume), Ok(()));
        let bytes = Frame::Commit(record.clone()).encode().unwrap();
        assert_eq!(
            Frame::decode(&bytes).unwrap(),
            Frame::Commit(record.clone())
        );
        if let Location::Extent { offset, .. } = &mut record.location {
            *offset += 1;
        }
        assert_eq!(record.validate_in(&volume), Err(Error::Invalid));
    }
}

impl Superblock {
    /// Plan one contiguous immutable data extent and its digest-tree nodes without IO.
    /// This is the shared host/native allocation arithmetic, not an allocation itself.
    pub fn extent(&self, start: u64, bytes: u64) -> Result<Location> {
        self.validate()?;
        let align = u64::from(self.alignment);
        if start < self.data_offset || !start.is_multiple_of(align) {
            return Err(Error::Invalid);
        }
        let blocks = bytes.div_ceil(align);
        let padded_bytes = blocks.checked_mul(align).ok_or(Error::Limit)?;
        let mut at = start.checked_add(padded_bytes).ok_or(Error::Limit)?;
        let mut tree = Vec::new();
        let mut children = blocks;
        while children > 0 {
            let nodes = children.div_ceil(align / 32);
            tree.push(TreeLevel { offset: at, nodes });
            at = nodes
                .checked_mul(align)
                .and_then(|n| at.checked_add(n))
                .ok_or(Error::Limit)?;
            if nodes == 1 {
                break;
            }
            children = nodes;
        }
        if at > self.capacity {
            return Err(Error::Limit);
        }
        Ok(Location::Extent {
            offset: start,
            padded_bytes,
            tree,
            root: if bytes == 0 {
                *blake3::hash(&[]).as_bytes()
            } else {
                [0; 32]
            },
        })
    }
}
/// Encode a digest-tree node into a caller-owned aligned page, including deterministic
/// zero padding. Hash arithmetic/encoding is shared by every platform adapter.
pub fn tree_node(destination: &mut [u8], children: &[[u8; 32]]) -> Result<[u8; 32]> {
    if destination.len() < 512
        || !destination.len().is_power_of_two()
        || children.len() > destination.len() / 32
    {
        return Err(Error::Invalid);
    }
    destination.fill(0);
    for (slot, child) in children.iter().enumerate() {
        destination[slot * 32..slot * 32 + 32].copy_from_slice(child);
    }
    Ok(*blake3::hash(destination).as_bytes())
}
/// Hash an object's tail as a full block with bounded zero-padding state, without
/// allocating another payload-sized buffer.
pub fn leaf_digest(bytes: &[u8], block_bytes: usize) -> Result<[u8; 32]> {
    if block_bytes < 512 || !block_bytes.is_power_of_two() || bytes.len() > block_bytes {
        return Err(Error::Invalid);
    }
    let mut hash = blake3::Hasher::new();
    hash.update(bytes);
    let zeros = [0u8; 4096];
    let mut left = block_bytes - bytes.len();
    while left != 0 {
        let part = left.min(zeros.len());
        hash.update(&zeros[..part]);
        left -= part;
    }
    Ok(*hash.finalize().as_bytes())
}
/// One metadata page needed to verify a raw block, root first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofStep {
    /// Exact aligned node offset.
    pub offset: u64,
    /// Digest slot selected for this block/path.
    pub slot: usize,
}
/// Streaming proof state. Each accepted metadata node narrows the committed root to
/// the data block's digest; no native adapter needs another proof parser.
pub struct BlockProof {
    steps: Vec<ProofStep>,
    next: usize,
    expected: [u8; 32],
    block_bytes: usize,
}
impl BlockProof {
    /// Derive bounded metadata offsets from the committed extent and common geometry.
    pub fn new(commit: &Commit, volume: &Superblock, block: u64) -> Result<Self> {
        commit.validate_in(volume)?;
        let align = u64::from(volume.alignment);
        if block >= commit.metadata.bytes.div_ceil(align) {
            return Err(Error::Invalid);
        }
        let Location::Extent { tree, root, .. } = &commit.location else {
            return Err(Error::Invalid);
        };
        let fanout = align / 32;
        let mut divisors = Vec::with_capacity(tree.len());
        let mut divisor = 1u64;
        for _ in tree {
            divisors.push(divisor);
            divisor = divisor.checked_mul(fanout).ok_or(Error::Limit)?;
        }
        let mut steps = Vec::with_capacity(tree.len());
        for (index, level) in tree.iter().enumerate().rev() {
            let child = block / divisors[index];
            let node = child / fanout;
            if node >= level.nodes {
                return Err(Error::Invalid);
            }
            let offset = node
                .checked_mul(align)
                .and_then(|n| level.offset.checked_add(n))
                .ok_or(Error::Limit)?;
            steps.push(ProofStep {
                offset,
                slot: (child % fanout) as usize,
            });
        }
        Ok(Self {
            steps,
            next: 0,
            expected: *root,
            block_bytes: volume.alignment as usize,
        })
    }
    /// Remaining exact node request, or None when the original data block may be checked.
    pub fn next_node(&self) -> Option<&ProofStep> {
        self.steps.get(self.next)
    }
    /// Verify the next complete node against the currently committed digest.
    pub fn accept_node(&mut self, node: &[u8]) -> Result<()> {
        let step = self.next_node().ok_or(Error::Invalid)?;
        if node.len() != self.block_bytes || blake3::hash(node).as_bytes() != &self.expected {
            return Err(Error::Integrity);
        }
        let start = step.slot.checked_mul(32).ok_or(Error::Limit)?;
        self.expected = array(node.get(start..start + 32).ok_or(Error::Invalid)?)?;
        self.next += 1;
        Ok(())
    }
    /// Verify the complete padded data page before exposing any requested subrange.
    pub fn verify_data(&self, bytes: &[u8]) -> Result<()> {
        if self.next != self.steps.len() || bytes.len() != self.block_bytes {
            return Err(Error::Invalid);
        }
        if blake3::hash(bytes).as_bytes() != &self.expected {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use alloc::{string::ToString, vec};
    #[test]
    fn st_f4_shared_proof_checks_metadata_then_data_and_refuses_each_corruption() {
        let volume = Superblock {
            capacity: 1 << 20,
            alignment: 4096,
            log_offset: 4096,
            log_bytes: 4096 * 8,
            data_offset: 4096 * 9,
            role: Role::SingleNodeHome,
            workspace: "workspace".to_string(),
            bucket: "bucket".to_string(),
        };
        let mut location = volume.extent(volume.data_offset, 5).unwrap();
        let mut node = vec![0; 4096];
        let leaf = leaf_digest(b"hello", 4096).unwrap();
        let root = tree_node(&mut node, &[leaf]).unwrap();
        if let Location::Extent { root: at, .. } = &mut location {
            *at = root;
        }
        let commit = Commit {
            sequence: 0,
            metadata: Metadata {
                object: Object {
                    workspace: "workspace".to_string(),
                    bucket: "bucket".to_string(),
                    key: "object".to_string(),
                },
                version: Version {
                    etag: Some("content".to_string()),
                    generation: Some("generation".to_string()),
                },
                bytes: 5,
            },
            content_hash: *blake3::hash(b"hello").as_bytes(),
            location,
        };
        let mut proof = BlockProof::new(&commit, &volume, 0).unwrap();
        assert!(proof.next_node().is_some());
        let mut corrupt = node.clone();
        corrupt[0] ^= 1;
        assert_eq!(proof.accept_node(&corrupt), Err(Error::Integrity));
        proof.accept_node(&node).unwrap();
        assert!(proof.next_node().is_none());
        let mut data = vec![0; 4096];
        data[..5].copy_from_slice(b"hello");
        assert_eq!(proof.verify_data(&data), Ok(()));
        data[5] = 1;
        assert_eq!(proof.verify_data(&data), Err(Error::Integrity));
        assert!(BlockProof::new(&commit, &volume, 1).is_err());
        assert!(volume.extent(volume.capacity, 1).is_err());
    }
}
