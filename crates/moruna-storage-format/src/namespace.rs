//! Sole generic namespace/page/control codec. Raw Superblock/Journal/receipts remain the
//! only disk authority. Namespace roots reference immutable bounded pages, not whole files.
#![forbid(unsafe_code)]
use crate::{Error, Metadata, Result, wire};
use alloc::{string::String, vec::Vec};
/// Reserved internal objects; ordinary object-port callers must not mutate this prefix.
pub const ROOT_PREFIX: &str = ".moruna-ns/";
/// One data page. A write copies at most this page per affected interval, never a whole file.
pub const DATA_BYTES: u64 = 64 * 1024;
/// Maximum leaf entries or branch children. Overflow splits before publication.
pub const FANOUT: usize = 32;
/// Maximum branch depth, checked before recursive reads or allocations.
pub const MAX_LEVEL: u8 = 8;
/// Maximum normalized UTF-8 relative path bytes. Non-UTF-8 paths are unsupported.
pub const MAX_PATH: usize = 1024;
/// Maximum basename bytes, excluding path separators.
pub const MAX_NAME: usize = 255;
/// Maximum commands in one admitted linked batch.
pub const MAX_STEPS: usize = 64;
/// Three independent ordered maps; mixed leaf types are refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// Fixed-width numeric inode key to inode facts.
    Inodes,
    /// One basename to stable inode id.
    Names,
    /// Fixed-width numeric logical data-page index to immutable full page.
    Data,
}
/// Stable inode facts. Directory names are relative to this inode, so subtree rename is
/// two parent-map changes rather than rewriting descendant paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inode {
    /// Exactly one directory entry, or false while an unlinked open inode is retained.
    pub linked: bool,
    /// Directories contain a Names tree; files contain a sparse Data tree.
    pub directory: bool,
    /// Logical file bytes. Directories always have length zero.
    pub length: u64,
    /// Exact immutable tree root, absent for an empty directory/file.
    pub tree: Option<Metadata>,
}
/// One leaf value or one branch separator's child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    /// Actual inode facts.
    Inode(Inode),
    /// Stable inode number, never a capability or caller identity.
    Name(u64),
    /// Actual acknowledged full DATA_BYTES immutable page.
    Data(Metadata),
    /// Actual child-page acknowledgement; key is that child's inclusive maximum.
    Child(Metadata),
}
/// One sorted unique page entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// Basename or canonical numeric key, depending on map kind.
    pub key: String,
    /// Matching typed leaf or branch value.
    pub value: Value,
}
/// Bounded immutable B+tree node. Logical depth and page byte size bound each path's IO
/// and metadata admission independently of total file or namespace size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    /// Exact map type.
    pub kind: Kind,
    /// Zero is a leaf; a branch points only to level-1 pages.
    pub level: u8,
    /// Sorted unique inclusive branch maxima or leaf keys.
    pub entries: Vec<Entry>,
}
/// The one namespace visibility transaction, conditionally published by the sole Journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Root {
    /// Actual current logical bytes, including any still-open unlinked inodes.
    pub logical_bytes: u64,
    /// Actual inode count, root included, for explicit admission.
    pub inode_count: u64,
    /// Next never-reused inode id, starting at two (one is the root directory).
    pub next_inode: u64,
    /// Actual immutable inode-tree receipt.
    pub inodes: Metadata,
}
/// Atomic name update mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rename {
    /// Destination must not exist.
    NoReplace,
    /// Both names must exist; exchange their inode links in one root commit.
    Exchange,
    /// Replace a compatible file or empty directory.
    Replace,
}
/// Validate the reserved exact root key. Namespace ids are bounded ASCII labels, not
/// paths, credentials or aliases into another namespace's immutable-object prefix.
pub fn root_object(object: &crate::Object) -> Result<()> {
    object.validate()?;
    let id = object
        .key
        .strip_prefix(ROOT_PREFIX)
        .and_then(|rest| rest.strip_suffix("/root"))
        .ok_or(Error::Invalid)?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// Validate normalized relative UTF-8 paths. Empty means the namespace root.
pub fn path(value: &str) -> Result<()> {
    if value.len() > MAX_PATH
        || value.contains('\0')
        || value.starts_with('/')
        || !value.is_empty()
            && value
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".." || part.len() > MAX_NAME)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// Canonical fixed-width numeric key; lexical order is unsigned numeric order.
pub fn number(value: u64) -> String {
    alloc::format!("{value:016x}")
}
/// Parse exactly canonical numeric keys, refusing aliases/overflow/upper case.
pub fn parse_number(value: &str) -> Result<u64> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid);
    }
    u64::from_str_radix(value, 16).map_err(|_| Error::Invalid)
}
fn metadata(value: &Metadata) -> Result<()> {
    value.object.validate()?;
    value.version.validate()?;
    Ok(())
}
impl Page {
    /// Validate one bounded node. Consumers additionally compare expected kind, depth and
    /// actual child maximum when traversing a pinned parent reference.
    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() || self.entries.len() > FANOUT || self.level > MAX_LEVEL {
            return Err(Error::Limit);
        }
        for (at, entry) in self.entries.iter().enumerate() {
            if at > 0 && self.entries[at - 1].key >= entry.key {
                return Err(Error::Invalid);
            }
            match self.kind {
                Kind::Names => {
                    path(&entry.key)?;
                    if entry.key.is_empty() || entry.key.contains('/') {
                        return Err(Error::Invalid);
                    }
                }
                _ => {
                    parse_number(&entry.key)?;
                }
            }
            match (&entry.value, self.level, self.kind) {
                (Value::Child(child), 1..=MAX_LEVEL, _) => metadata(child)?,
                (Value::Inode(inode), 0, Kind::Inodes) => {
                    if inode.directory && inode.length != 0 {
                        return Err(Error::Invalid);
                    }
                    if let Some(tree) = &inode.tree {
                        metadata(tree)?;
                    }
                }
                (Value::Name(id), 0, Kind::Names) if *id > 0 => {}
                (Value::Data(data), 0, Kind::Data) if data.bytes == DATA_BYTES => metadata(data)?,
                _ => return Err(Error::Invalid),
            }
        }
        Ok(())
    }
    /// Inclusive maximum used by the parent; empty nodes are never published.
    pub fn maximum(&self) -> Result<&str> {
        self.entries
            .last()
            .map(|entry| entry.key.as_str())
            .ok_or(Error::Invalid)
    }
    /// Encode into caller-admitted memory; no heap-sized opaque frame is allocated.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        self.validate()?;
        let mut out = wire::Out::new(dst)?;
        out.u8(21)?;
        out.u8(kind(self.kind))?;
        out.u8(self.level)?;
        out.u32(self.entries.len() as u32)?;
        for entry in &self.entries {
            out.text(&entry.key)?;
            match &entry.value {
                Value::Child(child) => {
                    out.u8(1)?;
                    out.metadata(child)?;
                }
                Value::Inode(inode) => {
                    out.u8(2)?;
                    out.u8(u8::from(inode.linked))?;
                    out.u8(u8::from(inode.directory))?;
                    out.u64(inode.length)?;
                    match &inode.tree {
                        Some(tree) => {
                            out.u8(1)?;
                            out.metadata(tree)?;
                        }
                        None => out.u8(0)?,
                    }
                }
                Value::Name(id) => {
                    out.u8(3)?;
                    out.u64(*id)?;
                }
                Value::Data(data) => {
                    out.u8(4)?;
                    out.metadata(data)?;
                }
            }
        }
        out.finish()
    }
    /// Decode exact framing, bounds and typed values before admitting traversal.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 21 {
            return Err(Error::Invalid);
        }
        let kind = read_kind(input.u8()?)?;
        let level = input.u8()?;
        let count = input.u32()? as usize;
        if count > FANOUT {
            return Err(Error::Limit);
        }
        let mut entries = Vec::new();
        for _ in 0..count {
            let key = input.text()?;
            let value = match input.u8()? {
                1 => Value::Child(input.metadata()?),
                2 => {
                    let linked = match input.u8()? {
                        0 => false,
                        1 => true,
                        _ => return Err(Error::Invalid),
                    };
                    let directory = match input.u8()? {
                        0 => false,
                        1 => true,
                        _ => return Err(Error::Invalid),
                    };
                    let length = input.u64()?;
                    let tree = match input.u8()? {
                        0 => None,
                        1 => Some(input.metadata()?),
                        _ => return Err(Error::Invalid),
                    };
                    Value::Inode(Inode {
                        linked,
                        directory,
                        length,
                        tree,
                    })
                }
                3 => Value::Name(input.u64()?),
                4 => Value::Data(input.metadata()?),
                _ => return Err(Error::Invalid),
            };
            entries.push(Entry { key, value });
        }
        input.finish()?;
        let page = Self {
            kind,
            level,
            entries,
        };
        page.validate()?;
        Ok(page)
    }
}
impl Root {
    /// Encode one actual inode-tree acknowledgement; never a speculative publication.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        if self.next_inode < 2 || self.inode_count == 0 || self.inode_count >= self.next_inode {
            return Err(Error::Invalid);
        }
        metadata(&self.inodes)?;
        let mut out = wire::Out::new(dst)?;
        out.u8(20)?;
        out.u64(self.next_inode)?;
        out.u64(self.logical_bytes)?;
        out.u64(self.inode_count)?;
        out.metadata(&self.inodes)?;
        out.finish()
    }
    /// Decode exact root. Namespace service verifies referenced pages and root inode.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 20 {
            return Err(Error::Invalid);
        }
        let root = Self {
            next_inode: input.u64()?,
            logical_bytes: input.u64()?,
            inode_count: input.u64()?,
            inodes: input.metadata()?,
        };
        input.finish()?;
        if root.next_inode < 2 || root.inode_count == 0 || root.inode_count >= root.next_inode {
            return Err(Error::Invalid);
        }
        metadata(&root.inodes)?;
        Ok(root)
    }
}
fn kind(kind: Kind) -> u8 {
    match kind {
        Kind::Inodes => 1,
        Kind::Names => 2,
        Kind::Data => 3,
    }
}
fn read_kind(kind: u8) -> Result<Kind> {
    match kind {
        1 => Ok(Kind::Inodes),
        2 => Ok(Kind::Names),
        3 => Ok(Kind::Data),
        _ => Err(Error::Invalid),
    }
}

/// Explicit actual per-session admission. Physical availability can refuse sooner; these
/// figures never pretend to reserve bytes in another namespace or guarantee free media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Maximum logical bytes in one file, no more than the supported tree address space.
    pub file_bytes: u64,
    /// Maximum total logical bytes (open-unlinked files included).
    pub namespace_bytes: u64,
    /// Maximum linked or open-unlinked inode count.
    pub inodes: u64,
    /// Maximum simultaneously open actual file handles.
    pub handles: usize,
    /// One operation's caller payload ceiling.
    pub transfer_bytes: u64,
    /// Actual page IOs allowed before a single transaction refuses.
    pub page_io: u32,
    /// Charged metadata bytes per bounded page, at most wire::MAX_HEADER.
    pub metadata_bytes: usize,
}
impl Limits {
    /// Privileged bounded walker limits, not a tenant file quota or a retained-byte
    /// guarantee. Uses the sole supported tree domain and the explicitly supplied IO
    /// and exact canonical metadata workspace; no duplicated maxima in boot adapters.
    pub fn maintenance(page_io: u32, metadata_bytes: usize) -> crate::Result<Self> {
        let limits = Self {
            file_bytes: 1u64 << 61,
            namespace_bytes: u64::MAX,
            inodes: 1u64 << 45,
            handles: 1,
            transfer_bytes: DATA_BYTES,
            page_io,
            metadata_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Validate explicit limits before any service resource or IO admission.
    pub fn validate(&self) -> crate::Result<()> {
        if self.file_bytes == 0
            || self.file_bytes > (1u64 << 61)
            || self.namespace_bytes < self.file_bytes
            || self.inodes < 1
            || self.handles == 0
            || self.transfer_bytes == 0
            || self.page_io == 0
            || self.metadata_bytes != wire::MAX_HEADER
        {
            return Err(crate::Error::Invalid);
        }
        Ok(())
    }
}

/// Exact byte-operation outcome. Cancelled bytes are only bytes actually exposed to the
/// caller; unpublished COW writes report zero even when orphan chunks reached storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Io {
    /// Exact completed bytes (zero for sync/truncate).
    Done(u64),
    /// Cancellation observed between original operations.
    Cancelled(u64),
    /// Failure with exact already exposed read bytes; no guessed successful write count.
    Failed(u64, wire::Refusal),
}
/// Actual opened file/directory facts, scoped to this connection-owned Namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Opened {
    /// Session-local opaque handle, not authority outside the actual channel.
    pub handle: u64,
    /// Stable namespace-local inode.
    pub inode: u64,
    /// Directory or file.
    pub directory: bool,
    /// Actual logical file bytes.
    pub length: u64,
}

/// Exact namespace-open request. Actual Scope/Account/role come from the existing bound
/// connection, never these bytes. Creation still requires real live namespace authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenRequest {
    /// Exact reserved durable root object.
    pub root: crate::Object,
    /// Explicit resource ceilings; no implicit expansion on exhaustion.
    pub limits: Limits,
    /// Create only on genuine missing root; malformed/corrupt roots never initialize.
    pub create: bool,
}
/// One native operation. Handles are scoped to the already-bound actual channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Open a normalized path.
    Open {
        /// Relative path.
        path: String,
        /// Create if genuinely absent.
        create: bool,
        /// Creation must not replace an existing name.
        exclusive: bool,
        /// Admit write-capable handle only under actual live policy.
        writable: bool,
    },
    /// Release one actual open handle.
    Close {
        /// Actual session handle.
        handle: u64,
    },
    /// Read a requested range into the caller's owned payload loan.
    ReadAt {
        /// Actual session handle.
        handle: u64,
        /// File byte offset.
        offset: u64,
        /// Requested destination bytes; true EOF can complete fewer.
        length: u64,
        /// Start within the admitted caller payload.
        payload_offset: u64,
    },
    /// Atomically publish one logical byte range.
    WriteAt {
        /// Actual session handle.
        handle: u64,
        /// File byte offset.
        offset: u64,
        /// Exact source bytes.
        length: u64,
        /// Start within the admitted caller payload.
        payload_offset: u64,
    },
    /// Change logical file length, preserving zero-hole semantics.
    Truncate {
        /// Actual session handle.
        handle: u64,
        /// New logical length.
        length: u64,
    },
    /// Real data+metadata stable-storage barrier.
    SyncFile {
        /// Actual session handle.
        handle: u64,
    },
    /// Real directory/root stable-storage barrier.
    SyncDirectory {
        /// Actual directory handle.
        handle: u64,
    },
    /// Actual current facts of an open inode.
    Stat {
        /// Actual session handle.
        handle: u64,
    },
    /// Create one directory (parent must exist).
    CreateDirectory {
        /// Normalized relative path.
        path: String,
    },
    /// Single-root atomic name change/exchange.
    Rename {
        /// Existing normalized source path.
        from: String,
        /// Destination path.
        to: String,
        /// Actual operation; no unlink/rename emulation.
        mode: Rename,
    },
    /// Unlink file/empty directory, retaining actual open inode references.
    Remove {
        /// Normalized relative path.
        path: String,
        /// Require a directory, otherwise require a file.
        directory: bool,
    },
}
/// A command's link controls only its immediate successor. A failure/cancellation skips
/// the rest of that linked chain; unrelated chains still get their actual own outcomes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Step {
    /// Operation to execute exactly once.
    pub command: Command,
    /// Success is required before submitting the next step.
    pub link: bool,
}
/// Bounded native submission, not an io_uring ring or a fake completion record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// Exact caller transaction correlation, independent of namespace authority.
    pub id: u64,
    /// Ordered actual operations and link edges.
    pub steps: Vec<Step>,
}
/// One observed outcome, with no success substituted for skipped or cancelled work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Cancellation observed before a non-byte operation was submitted.
    Cancelled,
    /// Actual byte/sync/truncate result.
    Io(Io),
    /// Actual newly admitted open handle or current facts of an existing handle.
    Opened(Opened),
    /// Actual successful non-byte namespace operation.
    Done,
    /// Live policy/resource/operation refused before success.
    Refused(wire::Refusal),
    /// Predecessor failed/cancelled; this linked step was never submitted.
    Skipped,
}
/// Complete typed response. Receipt is only the last successful ROOT publication, not
/// a claim that all submitted steps succeeded or every cancellation prevented writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// Exact request correlation.
    pub id: u64,
    /// Actual last published namespace visibility root.
    pub receipt: Metadata,
    /// One actual result per requested step, in order.
    pub outcomes: Vec<Outcome>,
}
fn boolean(input: &mut wire::Input<'_>) -> Result<bool> {
    match input.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::Invalid),
    }
}
impl OpenRequest {
    /// Canonical bounded open control; no account/capability/Scope labels are serialized.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        root_object(&self.root)?;
        self.limits.validate()?;
        let mut out = wire::Out::new(dst)?;
        out.u8(22)?;
        out.object(&self.root)?;
        out.u8(u8::from(self.create))?;
        out.u64(self.limits.file_bytes)?;
        out.u64(self.limits.namespace_bytes)?;
        out.u64(self.limits.inodes)?;
        out.u32(u32::try_from(self.limits.handles).map_err(|_| Error::Limit)?)?;
        out.u64(self.limits.transfer_bytes)?;
        out.u32(self.limits.page_io)?;
        out.u32(u32::try_from(self.limits.metadata_bytes).map_err(|_| Error::Limit)?)?;
        out.finish()
    }
    /// Decode exact complete control before any actual lease or payload admission.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 22 {
            return Err(Error::Invalid);
        }
        let root = input.object()?;
        let create = boolean(&mut input)?;
        let limits = Limits {
            file_bytes: input.u64()?,
            namespace_bytes: input.u64()?,
            inodes: input.u64()?,
            handles: input.u32()? as usize,
            transfer_bytes: input.u64()?,
            page_io: input.u32()?,
            metadata_bytes: input.u32()? as usize,
        };
        input.finish()?;
        root_object(&root)?;
        limits.validate()?;
        Ok(Self {
            root,
            create,
            limits,
        })
    }
}
impl Request {
    /// Reject empty/unbounded chains and invalid paths/ranges before executable IO.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty()
            || self.steps.len() > MAX_STEPS
            || self.steps.last().is_some_and(|step| step.link)
        {
            return Err(Error::Limit);
        }
        let mut read = false;
        let mut write = false;
        for step in &self.steps {
            match &step.command {
                Command::Open { path: name, .. }
                | Command::CreateDirectory { path: name }
                | Command::Remove { path: name, .. } => path(name)?,
                Command::Rename { from, to, .. } => {
                    path(from)?;
                    path(to)?;
                }
                Command::ReadAt {
                    offset,
                    length,
                    payload_offset,
                    ..
                } => {
                    read = true;
                    offset.checked_add(*length).ok_or(Error::Limit)?;
                    payload_offset.checked_add(*length).ok_or(Error::Limit)?;
                }
                Command::WriteAt {
                    offset,
                    length,
                    payload_offset,
                    ..
                } => {
                    write = true;
                    offset.checked_add(*length).ok_or(Error::Limit)?;
                    payload_offset.checked_add(*length).ok_or(Error::Limit)?;
                }
                _ => {}
            }
        }
        if read && write {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Actual payload bytes required; source and destination batches cannot be mixed.
    pub fn payload(&self) -> Result<(u64, bool)> {
        self.validate()?;
        let mut bytes = 0;
        let mut writable = false;
        for step in &self.steps {
            match step.command {
                Command::ReadAt {
                    length,
                    payload_offset,
                    ..
                } => {
                    writable = true;
                    bytes = bytes.max(payload_offset.checked_add(length).ok_or(Error::Limit)?);
                }
                Command::WriteAt {
                    length,
                    payload_offset,
                    ..
                } => bytes = bytes.max(payload_offset.checked_add(length).ok_or(Error::Limit)?),
                _ => {}
            }
        }
        Ok((bytes, writable))
    }
    /// Encode a bounded complete submission into actual admitted control memory.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        self.validate()?;
        let mut out = wire::Out::new(dst)?;
        out.u8(23)?;
        out.u64(self.id)?;
        out.u32(self.steps.len() as u32)?;
        for step in &self.steps {
            out.u8(u8::from(step.link))?;
            match &step.command {
                Command::Open {
                    path,
                    create,
                    exclusive,
                    writable,
                } => {
                    out.u8(1)?;
                    out.text(path)?;
                    out.u8(u8::from(*create))?;
                    out.u8(u8::from(*exclusive))?;
                    out.u8(u8::from(*writable))?;
                }
                Command::Close { handle } => {
                    out.u8(2)?;
                    out.u64(*handle)?;
                }
                Command::ReadAt {
                    handle,
                    offset,
                    length,
                    payload_offset,
                }
                | Command::WriteAt {
                    handle,
                    offset,
                    length,
                    payload_offset,
                } => {
                    out.u8(if matches!(&step.command, Command::ReadAt { .. }) {
                        3
                    } else {
                        4
                    })?;
                    out.u64(*handle)?;
                    out.u64(*offset)?;
                    out.u64(*length)?;
                    out.u64(*payload_offset)?;
                }
                Command::Truncate { handle, length } => {
                    out.u8(5)?;
                    out.u64(*handle)?;
                    out.u64(*length)?;
                }
                Command::SyncFile { handle } => {
                    out.u8(6)?;
                    out.u64(*handle)?;
                }
                Command::SyncDirectory { handle } => {
                    out.u8(7)?;
                    out.u64(*handle)?;
                }
                Command::Stat { handle } => {
                    out.u8(8)?;
                    out.u64(*handle)?;
                }
                Command::CreateDirectory { path } => {
                    out.u8(9)?;
                    out.text(path)?;
                }
                Command::Rename { from, to, mode } => {
                    out.u8(10)?;
                    out.text(from)?;
                    out.text(to)?;
                    out.u8(match mode {
                        Rename::NoReplace => 1,
                        Rename::Exchange => 2,
                        Rename::Replace => 3,
                    })?;
                }
                Command::Remove { path, directory } => {
                    out.u8(11)?;
                    out.text(path)?;
                    out.u8(u8::from(*directory))?;
                }
            }
        }
        out.finish()
    }
    /// Decode exact canonical operations, refusing unknown opcodes/booleans/link tails.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 23 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        let count = input.u32()? as usize;
        if count > MAX_STEPS {
            return Err(Error::Limit);
        }
        let mut steps = Vec::new();
        for _ in 0..count {
            let link = boolean(&mut input)?;
            let command = match input.u8()? {
                1 => Command::Open {
                    path: input.text()?,
                    create: boolean(&mut input)?,
                    exclusive: boolean(&mut input)?,
                    writable: boolean(&mut input)?,
                },
                2 => Command::Close {
                    handle: input.u64()?,
                },
                3 => Command::ReadAt {
                    handle: input.u64()?,
                    offset: input.u64()?,
                    length: input.u64()?,
                    payload_offset: input.u64()?,
                },
                4 => Command::WriteAt {
                    handle: input.u64()?,
                    offset: input.u64()?,
                    length: input.u64()?,
                    payload_offset: input.u64()?,
                },
                5 => Command::Truncate {
                    handle: input.u64()?,
                    length: input.u64()?,
                },
                6 => Command::SyncFile {
                    handle: input.u64()?,
                },
                7 => Command::SyncDirectory {
                    handle: input.u64()?,
                },
                8 => Command::Stat {
                    handle: input.u64()?,
                },
                9 => Command::CreateDirectory {
                    path: input.text()?,
                },
                10 => {
                    let from = input.text()?;
                    let to = input.text()?;
                    let mode = match input.u8()? {
                        1 => Rename::NoReplace,
                        2 => Rename::Exchange,
                        3 => Rename::Replace,
                        _ => return Err(Error::Invalid),
                    };
                    Command::Rename { from, to, mode }
                }
                11 => Command::Remove {
                    path: input.text()?,
                    directory: boolean(&mut input)?,
                },
                _ => return Err(Error::Invalid),
            };
            steps.push(Step { command, link });
        }
        input.finish()?;
        let request = Self { id, steps };
        request.validate()?;
        Ok(request)
    }
}
impl Response {
    /// Encode only actual facts. MAX_STEPS and fixed-size results bound reply admission.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        if self.outcomes.len() > MAX_STEPS {
            return Err(Error::Limit);
        }
        let mut out = wire::Out::new(dst)?;
        out.u8(24)?;
        out.u64(self.id)?;
        out.metadata(&self.receipt)?;
        out.u32(self.outcomes.len() as u32)?;
        for outcome in &self.outcomes {
            match outcome {
                Outcome::Cancelled => out.u8(8)?,
                Outcome::Done => out.u8(1)?,
                Outcome::Skipped => out.u8(2)?,
                Outcome::Refused(code) => {
                    out.u8(3)?;
                    out.u8(wire::refusal_code(*code))?;
                }
                Outcome::Opened(facts) => {
                    out.u8(4)?;
                    out.u64(facts.handle)?;
                    out.u64(facts.inode)?;
                    out.u8(u8::from(facts.directory))?;
                    out.u64(facts.length)?;
                }
                Outcome::Io(Io::Done(bytes)) => {
                    out.u8(5)?;
                    out.u64(*bytes)?;
                }
                Outcome::Io(Io::Cancelled(bytes)) => {
                    out.u8(6)?;
                    out.u64(*bytes)?;
                }
                Outcome::Io(Io::Failed(bytes, code)) => {
                    out.u8(7)?;
                    out.u64(*bytes)?;
                    out.u8(wire::refusal_code(*code))?;
                }
            }
        }
        out.finish()
    }
    /// Decode exact response; clients additionally check id, result count and actual root.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 24 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        let receipt = input.metadata()?;
        let count = input.u32()? as usize;
        if count > MAX_STEPS {
            return Err(Error::Limit);
        }
        let mut outcomes = Vec::new();
        for _ in 0..count {
            outcomes.push(match input.u8()? {
                8 => Outcome::Cancelled,
                1 => Outcome::Done,
                2 => Outcome::Skipped,
                3 => Outcome::Refused(wire::refusal_from_code(input.u8()?)?),
                4 => Outcome::Opened(Opened {
                    handle: input.u64()?,
                    inode: input.u64()?,
                    directory: boolean(&mut input)?,
                    length: input.u64()?,
                }),
                5 => Outcome::Io(Io::Done(input.u64()?)),
                6 => Outcome::Io(Io::Cancelled(input.u64()?)),
                7 => Outcome::Io(Io::Failed(
                    input.u64()?,
                    wire::refusal_from_code(input.u8()?)?,
                )),
                _ => return Err(Error::Invalid),
            });
        }
        input.finish()?;
        Ok(Self {
            id,
            receipt,
            outcomes,
        })
    }
}

/// Exact metadata identity for a sealed native GC set, using the EXISTING canonical
/// metadata field codec. Oversized identity fails closed instead of meaning nonmembership.
pub fn metadata_fingerprint(metadata: &Metadata) -> Result<[u8; 32]> {
    let mut bytes = [0; 2048];
    let mut out = wire::Out::new(&mut bytes)?;
    out.u8(25)?;
    out.metadata(metadata)?;
    let length = out.finish()?;
    Ok(*blake3::hash(&bytes[..length]).as_bytes())
}

/// Actual bound namespace session, returned by the service after real open/root IO.
/// No caller-selected root, role or account label substitutes this attestation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// Actual admission context from the private bound connection.
    pub admission: crate::auth::Admission,
    /// Exact root last successfully published/read during native namespace open.
    pub receipt: Metadata,
    /// Actual admitted namespace limits.
    pub limits: Limits,
}
impl Session {
    /// Encode actual bound facts using the SAME OpenRequest limit/root codec.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        let mut config = [0; 2048];
        let length = OpenRequest {
            root: self.receipt.object.clone(),
            limits: self.limits,
            create: false,
        }
        .encode_into(&mut config)?;
        let mut out = wire::Out::new(dst)?;
        out.u8(26)?;
        self.admission.write(&mut out)?;
        out.metadata(&self.receipt)?;
        out.blob(&config[..length])?;
        out.finish()
    }
    /// Decode exact actual attestation and reject inconsistent duplicate root facts.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(src)?;
        if input.u8()? != 26 {
            return Err(Error::Invalid);
        }
        let admission = crate::auth::Admission::read(&mut input)?;
        let receipt = input.metadata()?;
        let config = OpenRequest::decode(input.blob()?)?;
        input.finish()?;
        if config.root != receipt.object || config.create {
            return Err(Error::Invalid);
        }
        Ok(Self {
            admission,
            receipt,
            limits: config.limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(bytes: u64) -> Metadata {
        Metadata {
            object: crate::Object {
                workspace: "owner".into(),
                bucket: "local".into(),
                key: ".moruna-ns/test/root/objects/one".into(),
            },
            version: crate::Version {
                etag: Some("actual-hash-fixture".into()),
                generation: Some("version-1".into()),
            },
            bytes,
        }
    }
    fn roundtrip(page: Page) {
        let mut bytes = alloc::vec![0;wire::MAX_HEADER];
        let length = page.encode_into(&mut bytes).unwrap();
        assert_eq!(Page::decode(&bytes[..length]).unwrap(), page);
        for cut in 0..length {
            assert!(Page::decode(&bytes[..cut]).is_err(), "truncated page {cut}");
        }
        bytes[length] = 1;
        assert!(Page::decode(&bytes[..length + 1]).is_err());
    }
    #[test]
    fn sparse_gb_page_indices_and_all_typed_node_variants_roundtrip_without_flat_file_limit() {
        let index = (37u64 << 30) / DATA_BYTES;
        assert_eq!(parse_number(&number(index)).unwrap(), index);
        roundtrip(Page {
            kind: Kind::Data,
            level: 0,
            entries: alloc::vec![Entry {
                key: number(index),
                value: Value::Data(metadata(DATA_BYTES))
            }],
        });
        roundtrip(Page {
            kind: Kind::Names,
            level: 0,
            entries: alloc::vec![
                Entry {
                    key: "a".into(),
                    value: Value::Name(2)
                },
                Entry {
                    key: "z".into(),
                    value: Value::Name(3)
                }
            ],
        });
        roundtrip(Page {
            kind: Kind::Inodes,
            level: 0,
            entries: alloc::vec![
                Entry {
                    key: number(1),
                    value: Value::Inode(Inode {
                        linked: true,
                        directory: true,
                        length: 0,
                        tree: Some(metadata(512))
                    })
                },
                Entry {
                    key: number(2),
                    value: Value::Inode(Inode {
                        linked: false,
                        directory: false,
                        length: 37u64 << 30,
                        tree: Some(metadata(512))
                    })
                }
            ],
        });
        for kind in [Kind::Data, Kind::Inodes, Kind::Names] {
            roundtrip(Page {
                kind,
                level: MAX_LEVEL,
                entries: alloc::vec![Entry {
                    key: if kind == Kind::Names {
                        "z".into()
                    } else {
                        number(u64::MAX)
                    },
                    value: Value::Child(metadata(512))
                }],
            });
        }
    }
    #[test]
    fn malformed_tree_geometry_wrong_types_and_root_aliases_refuse() {
        let page = Page {
            kind: Kind::Data,
            level: 0,
            entries: alloc::vec![Entry {
                key: number(0),
                value: Value::Data(metadata(DATA_BYTES))
            }],
        };
        let mut wrong = page.clone();
        wrong.entries[0].value = Value::Name(1);
        assert!(wrong.validate().is_err());
        wrong = page.clone();
        wrong.level = MAX_LEVEL + 1;
        assert!(wrong.validate().is_err());
        wrong = page.clone();
        wrong.entries = alloc::vec![page.entries[0].clone();FANOUT+1];
        assert!(wrong.validate().is_err());
        wrong = page.clone();
        wrong.entries.push(page.entries[0].clone());
        assert!(wrong.validate().is_err());
        wrong = page.clone();
        wrong.entries[0].value = Value::Data(metadata(DATA_BYTES - 1));
        assert!(wrong.validate().is_err());
        for invalid in ["/absolute", "a/../b", "a/./b", "a//b", "a/", "\0"] {
            assert!(path(invalid).is_err());
        }
        let mut root = metadata(512).object;
        root.key = ".moruna-ns/test/root".into();
        assert!(root_object(&root).is_ok());
        for invalid in [
            ".moruna-ns/test/root/objects/injected/root",
            ".moruna-ns/../root",
            ".moruna-ns//root",
            "ordinary/root",
        ] {
            root.key = invalid.into();
            assert!(root_object(&root).is_err());
        }
        assert!(parse_number("000000000000000A").is_err());
        assert!(parse_number("1").is_err());
    }
    #[test]
    fn root_quota_and_exact_version_fingerprint_are_persisted_not_sampled_or_name_only() {
        let root = Root {
            logical_bytes: 37u64 << 30,
            inode_count: 12,
            next_inode: 20,
            inodes: metadata(512),
        };
        let mut bytes = alloc::vec![0;wire::MAX_HEADER];
        let length = root.encode_into(&mut bytes).unwrap();
        assert_eq!(Root::decode(&bytes[..length]).unwrap(), root);
        let original = metadata(DATA_BYTES);
        let first = metadata_fingerprint(&original).unwrap();
        let mut changed = original.clone();
        changed.version.generation = Some("version-2".into());
        assert_ne!(metadata_fingerprint(&changed).unwrap(), first);
        changed = original.clone();
        changed.bytes -= 1;
        assert_ne!(metadata_fingerprint(&changed).unwrap(), first);
        changed = original;
        changed.object.workspace = "other-owner".into();
        assert_ne!(metadata_fingerprint(&changed).unwrap(), first);
        assert!(Limits::maintenance(0, wire::MAX_HEADER).is_err());
        assert!(Limits::maintenance(1, wire::MAX_HEADER - 1).is_err());
        assert!(Limits::maintenance(1, wire::MAX_HEADER).is_ok());
    }
    #[test]
    fn canonical_commands_and_real_partial_or_skipped_facts_roundtrip() {
        let commands = alloc::vec![
            Command::Open {
                path: "dir/file".into(),
                create: true,
                exclusive: true,
                writable: true
            },
            Command::Close { handle: 1 },
            Command::WriteAt {
                handle: 1,
                offset: 37u64 << 30,
                length: 4096,
                payload_offset: 0
            },
            Command::Truncate {
                handle: 1,
                length: 8193
            },
            Command::SyncFile { handle: 1 },
            Command::SyncDirectory { handle: 2 },
            Command::Stat { handle: 1 },
            Command::CreateDirectory { path: "dir".into() },
            Command::Rename {
                from: "a".into(),
                to: "b".into(),
                mode: Rename::NoReplace
            },
            Command::Rename {
                from: "a".into(),
                to: "b".into(),
                mode: Rename::Exchange
            },
            Command::Rename {
                from: "a".into(),
                to: "b".into(),
                mode: Rename::Replace
            },
            Command::Remove {
                path: "empty".into(),
                directory: true
            }
        ];
        let request = Request {
            id: 44,
            steps: commands
                .into_iter()
                .map(|command| Step {
                    command,
                    link: false,
                })
                .collect(),
        };
        let mut bytes = alloc::vec![0;wire::MAX_HEADER];
        let length = request.encode_into(&mut bytes).unwrap();
        assert_eq!(Request::decode(&bytes[..length]).unwrap(), request);
        assert_eq!(request.payload().unwrap(), (4096, false));
        let response = Response {
            id: 44,
            receipt: metadata(512),
            outcomes: alloc::vec![
                Outcome::Done,
                Outcome::Skipped,
                Outcome::Cancelled,
                Outcome::Refused(wire::Refusal::Busy),
                Outcome::Opened(Opened {
                    handle: 1,
                    inode: 2,
                    directory: false,
                    length: 37u64 << 30
                }),
                Outcome::Io(Io::Done(17)),
                Outcome::Io(Io::Cancelled(13)),
                Outcome::Io(Io::Failed(9, wire::Refusal::Unavailable))
            ],
        };
        let length = response.encode_into(&mut bytes).unwrap();
        assert_eq!(Response::decode(&bytes[..length]).unwrap(), response);
        let read = Request {
            id: 45,
            steps: alloc::vec![Step {
                command: Command::ReadAt {
                    handle: 1,
                    offset: 0,
                    length: 7,
                    payload_offset: 2
                },
                link: false
            }],
        };
        let length = read.encode_into(&mut bytes).unwrap();
        assert_eq!(Request::decode(&bytes[..length]).unwrap(), read);
        assert_eq!(read.payload().unwrap(), (9, true));
    }
    #[test]
    fn linked_tail_mixed_payload_aliases_and_overflow_are_rejected_before_io() {
        let write = Step {
            command: Command::WriteAt {
                handle: 1,
                offset: 0,
                length: 7,
                payload_offset: 0,
            },
            link: true,
        };
        let mut request = Request {
            id: 1,
            steps: alloc::vec![write],
        };
        assert!(request.validate().is_err());
        request.steps.push(Step {
            command: Command::SyncFile { handle: 1 },
            link: false,
        });
        assert!(request.validate().is_ok());
        request.steps.push(Step {
            command: Command::ReadAt {
                handle: 1,
                offset: 0,
                length: 7,
                payload_offset: 0,
            },
            link: false,
        });
        assert!(request.validate().is_err());
        request.steps = alloc::vec![Step {
            command: Command::WriteAt {
                handle: 1,
                offset: u64::MAX,
                length: 1,
                payload_offset: 0
            },
            link: false
        }];
        assert!(request.validate().is_err());
        request.steps = alloc::vec![Step {command:Command::Stat {handle:1},link:false};MAX_STEPS+1];
        assert!(request.validate().is_err());
    }
}
