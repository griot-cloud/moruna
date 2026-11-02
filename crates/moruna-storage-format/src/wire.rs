//! Sole versioned object-port control codec. Payload bytes travel separately into caller
//! owned charged memory; this codec neither authorizes capabilities nor opens transports.
#![forbid(unsafe_code)]
use crate::{Error, Metadata, Object, Result, Scope, VERSION, Version};
use alloc::{string::String, vec::Vec};
/// Maximum control envelope; payload storage is independently admitted.
pub const MAX_HEADER: usize = 64 * 1024;
/// Opaque capability bound, without any assumed identity-service syntax.
pub const MAX_CAPABILITY: usize = 16 * 1024;
const MAGIC: &[u8; 8] = b"MORUOBJ1";
/// Fixed canonical control prefix needed before admitting/mapping a complete envelope.
pub const PREFIX_BYTES: usize = 16;
/// Validate the canonical magic/major/declared bound from a prefix (possibly padded).
/// This admits only a bounded length, not the body or its authority. Call the exact
/// request/admission/session decoder on precisely the returned bytes after reading them.
pub fn header_bytes(prefix: &[u8]) -> Result<usize> {
    let prefix = prefix.get(..PREFIX_BYTES).ok_or(Error::Incomplete)?;
    if &prefix[..8] != MAGIC {
        return Err(Error::Invalid);
    }
    let major = u32::from_le_bytes(prefix[8..12].try_into().map_err(|_| Error::Incomplete)?);
    if major != VERSION {
        return Err(Error::Version);
    }
    let length =
        u32::from_le_bytes(prefix[12..16].try_into().map_err(|_| Error::Incomplete)?) as usize;
    if !(PREFIX_BYTES..=MAX_HEADER).contains(&length) {
        return Err(Error::Limit);
    }
    Ok(length)
}

/// One native conditional publication operation, never read/unconditional-write emulation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Condition {
    /// Key must be absent.
    Absent,
    /// Exact currently published version must match.
    Match(Version),
}
/// Versioned object operation. Listing is not authorization to read returned objects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Exact current metadata under Read authority, without prefix enumeration.
    Current,
    /// Lookup exact original HOME object/version/range in local encrypted cache.
    CacheRead {
        /// Actual pinned home version.
        version: Version,
        /// Original range start.
        offset: u64,
        /// Exact caller destination bytes.
        length: u64,
    },
    /// Explicit trusted engine fill, requiring CacheScratch WRITE authority for the
    /// original pinned home identity; never mints a substitute home version.
    CacheFill {
        /// Actual pinned home version.
        version: Version,
        /// Original range start.
        offset: u64,
        /// Exact admitted source bytes.
        length: u64,
    },
    /// Exact pinned metadata, without prefix-list authority.
    Head {
        /// Exact immutable backend identity.
        version: Version,
    },
    /// Exact immutable version and caller-destination payload range.
    Read {
        /// Pinned backend identity.
        version: Version,
        /// Start within the immutable object.
        offset: u64,
        /// Exact response body bytes.
        length: u64,
    },
    /// Create one complete immutable object; existing keys conflict.
    Create {
        /// Exact request body bytes.
        length: u64,
    },
    /// Recursive bounded prefix page. `after` is the last exact returned key, exclusive.
    List {
        /// Positive maximum entries, still bounded by the control envelope.
        limit: u32,
        /// Exclusive key cursor; no provider-private token or credential.
        after: Option<String>,
    },
    /// Atomic conditional whole-object publication.
    Replace {
        /// Native atomic condition.
        condition: Condition,
        /// Exact request body bytes.
        length: u64,
    },
}
/// Borrowed opaque capability, intentionally no Debug or secret-copying Clone.
pub struct Request<'a> {
    /// Correlation identity, never a credential.
    pub id: u64,
    /// Requesting ledger/key scope, distinct from resource ownership.
    pub scope: Scope,
    /// Exact owner/bucket/key or prefix.
    pub object: Object,
    /// Borrowed secret bytes; verifier interprets, transport does not log or inspect.
    pub proof: crate::auth::Proof<'a>,
    /// Optional required configured backend class; never an authorization grant.
    pub expected_role: Option<crate::auth::SessionRole>,
    /// Requested exact action and range/publication condition.
    pub operation: Operation,
}
impl Request<'_> {
    fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        self.object.validate()?;
        self.proof.validate().map_err(|_| Error::Invalid)?;
        match &self.operation {
            Operation::Read {
                version,
                offset,
                length,
            }
            | Operation::CacheRead {
                version,
                offset,
                length,
            }
            | Operation::CacheFill {
                version,
                offset,
                length,
            } => {
                version.validate()?;
                offset.checked_add(*length).ok_or(Error::Limit)?;
            }
            Operation::Head { version: v }
            | Operation::Replace {
                condition: Condition::Match(v),
                ..
            } => v.validate()?,
            Operation::List { limit, .. } if *limit == 0 => return Err(Error::Invalid),
            _ => {}
        }
        if self.object.key.is_empty() && !matches!(self.operation, Operation::List { .. }) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Encode into admitted caller space, without a temporary credential-bearing Vec.
    /// Caller owns wiping/releasing that space after the control send completes.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        self.validate()?;
        let mut out = Out::new(dst)?;
        out.u8(1)?;
        out.u64(self.id)?;
        out.text(&self.scope.workspace)?;
        out.text(&self.scope.run)?;
        out.object(&self.object)?;
        self.proof.encode(&mut out)?;
        match self.expected_role {
            None => out.u8(0)?,
            Some(role) => {
                out.u8(1)?;
                role.write(&mut out)?;
            }
        }
        match &self.operation {
            Operation::Current => out.u8(6)?,
            Operation::CacheRead {
                version,
                offset,
                length,
            }
            | Operation::CacheFill {
                version,
                offset,
                length,
            } => {
                out.u8(if matches!(self.operation, Operation::CacheRead { .. }) {
                    7
                } else {
                    8
                })?;
                out.version(version)?;
                out.u64(*offset)?;
                out.u64(*length)?;
            }

            Operation::Head { version } => {
                out.u8(5)?;
                out.version(version)?;
            }
            Operation::Read {
                version,
                offset,
                length,
            } => {
                out.u8(1)?;
                out.version(version)?;
                out.u64(*offset)?;
                out.u64(*length)?;
            }
            Operation::Create { length } => {
                out.u8(2)?;
                out.u64(*length)?;
            }
            Operation::List { limit, after } => {
                out.u8(3)?;
                out.u32(*limit)?;
                out.optional(after)?;
            }
            Operation::Replace { condition, length } => {
                out.u8(4)?;
                match condition {
                    Condition::Absent => out.u8(0)?,
                    Condition::Match(v) => {
                        out.u8(1)?;
                        out.version(v)?;
                    }
                };
                out.u64(*length)?;
            }
        }
        out.finish()
    }
}
impl<'a> Request<'a> {
    /// Decode only a complete bounded control header. Capability borrows the caller's
    /// secret-bearing buffer; body consumption must wait for the service's live verifier.
    pub fn decode(src: &'a [u8]) -> Result<Self> {
        let mut input = Input::new(src)?;
        if input.u8()? != 1 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        let scope = Scope {
            workspace: input.text()?,
            run: input.text()?,
        };
        let object = input.object()?;
        let proof = crate::auth::Proof::decode(&mut input)?;
        let expected_role = match input.u8()? {
            0 => None,
            1 => Some(crate::auth::SessionRole::read(&mut input)?),
            _ => return Err(Error::Invalid),
        };
        let operation = match input.u8()? {
            1 => Operation::Read {
                version: input.version()?,
                offset: input.u64()?,
                length: input.u64()?,
            },
            2 => Operation::Create {
                length: input.u64()?,
            },
            3 => Operation::List {
                limit: input.u32()?,
                after: input.optional()?,
            },
            4 => {
                let condition = match input.u8()? {
                    0 => Condition::Absent,
                    1 => Condition::Match(input.version()?),
                    _ => return Err(Error::Invalid),
                };
                Operation::Replace {
                    condition,
                    length: input.u64()?,
                }
            }
            6 => Operation::Current,
            7 => Operation::CacheRead {
                version: input.version()?,
                offset: input.u64()?,
                length: input.u64()?,
            },
            8 => Operation::CacheFill {
                version: input.version()?,
                offset: input.u64()?,
                length: input.u64()?,
            },
            5 => Operation::Head {
                version: input.version()?,
            },
            _ => return Err(Error::Invalid),
        };
        input.finish()?;
        let request = Self {
            id,
            scope,
            object,
            proof,
            expected_role,
            operation,
        };
        request.validate()?;
        Ok(request)
    }
}
/// Typed public refusal, without arbitrary provider URLs/messages or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// Exact action not authorized.
    Denied,
    /// Object/version absent.
    Missing,
    /// Conditional identity changed.
    Conflict,
    /// Required adapter/capability unavailable.
    Unavailable,
    /// Required protocol/operation unsupported.
    Unsupported,
    /// Actual admitted capacity insufficient.
    Capacity,
    /// Payload or committed metadata authentication failed.
    Integrity,
    /// Invalid request/body/range.
    Invalid,
    /// Healthy owner has original asynchronous IO in flight; no new IO was admitted.
    Busy,
}
/// Sole response metadata contract; exact body bytes follow only successful Read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Exact original range was authenticated and copied; no invented full object facts.
    CacheHit {
        /// Original range start.
        offset: u64,
        /// Actual destination bytes.
        length: u64,
    },
    /// Genuine absence only; no caller destination bytes were changed.
    CacheMiss,
    /// Actual encrypted local fill acknowledgement, never a durable-home receipt.
    Cached,
    /// Authorized before accepting a create/replace body, not a durability acknowledgement.
    Ready {
        /// Exact admitted body bytes.
        length: u64,
    },
    /// Exact pinned object metadata.
    Head(Metadata),
    /// Immutable read range, preserving full object metadata.
    Read {
        /// Actual pinned object facts.
        metadata: Metadata,
        /// Actual requested start.
        offset: u64,
        /// Exact body bytes after this header.
        length: u64,
    },
    /// Complete durable create/conditional publication acknowledgement.
    Written(Metadata),
    /// Recursive page in lexicographic key order; truncated pages carry next exclusive key.
    Listed {
        /// Exact owner/version facts; each later read needs independent verification.
        entries: Vec<Metadata>,
        /// Exclusive key for the next bounded page, or complete.
        next: Option<String>,
    },
    /// Explicit refusal; no payload follows.
    Refused(Refusal),
}
/// Response correlation and exact typed result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// Request identity echoed unchanged.
    pub id: u64,
    /// Actual service outcome, not a speculative acknowledgement.
    pub outcome: Outcome,
}
impl Response {
    /// Encode into the caller's bounded header allocation.
    pub fn encode_into(&self, dst: &mut [u8]) -> Result<usize> {
        let mut out = Out::new(dst)?;
        out.u8(2)?;
        out.u64(self.id)?;
        match &self.outcome {
            Outcome::CacheHit { offset, length } => {
                offset.checked_add(*length).ok_or(Error::Limit)?;
                out.u8(7)?;
                out.u64(*offset)?;
                out.u64(*length)?;
            }
            Outcome::CacheMiss => out.u8(8)?,
            Outcome::Cached => out.u8(9)?,
            Outcome::Ready { length } => {
                out.u8(5)?;
                out.u64(*length)?;
            }
            Outcome::Head(metadata) => {
                out.u8(6)?;
                out.metadata(metadata)?;
            }
            Outcome::Read {
                metadata,
                offset,
                length,
            } => {
                range(metadata, *offset, *length)?;
                out.u8(1)?;
                out.metadata(metadata)?;
                out.u64(*offset)?;
                out.u64(*length)?;
            }
            Outcome::Written(metadata) => {
                out.u8(2)?;
                out.metadata(metadata)?;
            }
            Outcome::Listed { entries, next } => {
                out.u8(3)?;
                out.u32(u32::try_from(entries.len()).map_err(|_| Error::Limit)?)?;
                for entry in entries {
                    out.metadata(entry)?;
                }
                out.optional(next)?;
            }
            Outcome::Refused(refusal) => {
                out.u8(4)?;
                out.u8(refusal_code(*refusal))?;
            }
        }
        out.finish()
    }
    /// Decode a complete bounded response. The request owner must also verify id, identity,
    /// version and range equal its pending request before consuming the response body.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut input = Input::new(src)?;
        if input.u8()? != 2 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        let outcome = match input.u8()? {
            7 => {
                let offset = input.u64()?;
                let length = input.u64()?;
                offset.checked_add(length).ok_or(Error::Limit)?;
                Outcome::CacheHit { offset, length }
            }
            8 => Outcome::CacheMiss,
            9 => Outcome::Cached,
            1 => {
                let metadata = input.metadata()?;
                let offset = input.u64()?;
                let length = input.u64()?;
                range(&metadata, offset, length)?;
                Outcome::Read {
                    metadata,
                    offset,
                    length,
                }
            }
            2 => Outcome::Written(input.metadata()?),
            3 => {
                let count = input.u32()? as usize;
                if count > MAX_HEADER / 32 {
                    return Err(Error::Limit);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push(input.metadata()?);
                }
                Outcome::Listed {
                    entries,
                    next: input.optional()?,
                }
            }
            4 => Outcome::Refused(refusal_from_code(input.u8()?)?),
            5 => Outcome::Ready {
                length: input.u64()?,
            },
            6 => Outcome::Head(input.metadata()?),
            _ => return Err(Error::Invalid),
        };
        input.finish()?;
        Ok(Self { id, outcome })
    }
}
fn range(metadata: &Metadata, offset: u64, length: u64) -> Result<()> {
    if offset
        .checked_add(length)
        .is_none_or(|n| n > metadata.bytes)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// Exact-length body accounting for charged receive-into adapters. EOF before complete,
/// overdelivery or destination overflow is a refusal, never a successful partial object.
pub struct Body {
    remaining: u64,
}
impl Body {
    /// Start only after the header and exact action/range were validated and authorized.
    pub fn new(length: u64) -> Self {
        Self { remaining: length }
    }
    /// Bound one receive by remaining body and charged destination space.
    pub fn limit(&self, destination_bytes: u64, offset: u64, max: u32) -> Result<u32> {
        let available = destination_bytes
            .checked_sub(offset)
            .ok_or(Error::Invalid)?;
        Ok(self.remaining.min(available).min(u64::from(max)) as u32)
    }
    /// Account actual bytes returned by the owned receive operation.
    pub fn received(&mut self, bytes: u32) -> Result<()> {
        if bytes == 0 && self.remaining != 0 {
            return Err(Error::Incomplete);
        }
        self.remaining = self
            .remaining
            .checked_sub(u64::from(bytes))
            .ok_or(Error::Invalid)?;
        Ok(())
    }
    /// Exact completion, no silent truncation.
    pub fn complete(&self) -> bool {
        self.remaining == 0
    }
}
pub(crate) struct Out<'a> {
    dst: &'a mut [u8],
    at: usize,
}
impl<'a> Out<'a> {
    pub(crate) fn new(dst: &'a mut [u8]) -> Result<Self> {
        let mut out = Self { dst, at: 0 };
        out.bytes(MAGIC)?;
        out.u32(VERSION)?;
        out.u32(0)?;
        Ok(out)
    }
    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.at.checked_add(bytes.len()).ok_or(Error::Limit)?;
        if end > MAX_HEADER {
            return Err(Error::Limit);
        }
        self.dst
            .get_mut(self.at..end)
            .ok_or(Error::Limit)?
            .copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }
    pub(crate) fn u8(&mut self, v: u8) -> Result<()> {
        self.bytes(&[v])
    }
    pub(crate) fn u32(&mut self, v: u32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub(crate) fn u64(&mut self, v: u64) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub(crate) fn blob(&mut self, v: &[u8]) -> Result<()> {
        self.u32(u32::try_from(v.len()).map_err(|_| Error::Limit)?)?;
        self.bytes(v)
    }
    pub(crate) fn text(&mut self, v: &str) -> Result<()> {
        self.blob(v.as_bytes())
    }
    pub(crate) fn optional(&mut self, v: &Option<String>) -> Result<()> {
        match v {
            Some(v) => {
                self.u8(1)?;
                self.text(v)
            }
            None => self.u8(0),
        }
    }
    pub(crate) fn object(&mut self, v: &Object) -> Result<()> {
        v.validate()?;
        self.text(&v.workspace)?;
        self.text(&v.bucket)?;
        self.text(&v.key)
    }
    pub(crate) fn version(&mut self, v: &Version) -> Result<()> {
        v.validate()?;
        self.optional(&v.etag)?;
        self.optional(&v.generation)
    }
    pub(crate) fn metadata(&mut self, v: &Metadata) -> Result<()> {
        self.object(&v.object)?;
        self.version(&v.version)?;
        self.u64(v.bytes)
    }
    pub(crate) fn finish(self) -> Result<usize> {
        self.dst[12..16].copy_from_slice(&(self.at as u32).to_le_bytes());
        Ok(self.at)
    }
}
pub(crate) struct Input<'a> {
    src: &'a [u8],
    at: usize,
}
impl<'a> Input<'a> {
    pub(crate) fn new(src: &'a [u8]) -> Result<Self> {
        if src.len() > MAX_HEADER {
            return Err(Error::Limit);
        }
        if header_bytes(src)? != src.len() {
            return Err(Error::Incomplete);
        }
        Ok(Self {
            src,
            at: PREFIX_BYTES,
        })
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Limit)?;
        let bytes = self.src.get(self.at..end).ok_or(Error::Incomplete)?;
        self.at = end;
        Ok(bytes)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| Error::Incomplete)?,
        ))
    }
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Incomplete)?,
        ))
    }
    pub(crate) fn blob(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }
    pub(crate) fn text(&mut self) -> Result<String> {
        String::from_utf8(self.blob()?.to_vec()).map_err(|_| Error::Invalid)
    }
    pub(crate) fn optional(&mut self) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.text()?)),
            _ => Err(Error::Invalid),
        }
    }
    pub(crate) fn object(&mut self) -> Result<Object> {
        let object = Object {
            workspace: self.text()?,
            bucket: self.text()?,
            key: self.text()?,
        };
        object.validate()?;
        Ok(object)
    }
    pub(crate) fn version(&mut self) -> Result<Version> {
        let version = Version {
            etag: self.optional()?,
            generation: self.optional()?,
        };
        version.validate()?;
        Ok(version)
    }
    pub(crate) fn metadata(&mut self) -> Result<Metadata> {
        Ok(Metadata {
            object: self.object()?,
            version: self.version()?,
            bytes: self.u64()?,
        })
    }
    pub(crate) fn finish(self) -> Result<()> {
        if self.at != self.src.len() {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

pub(crate) fn refusal_code(code: Refusal) -> u8 {
    match code {
        Refusal::Denied => 1,
        Refusal::Missing => 2,
        Refusal::Conflict => 3,
        Refusal::Unavailable => 4,
        Refusal::Unsupported => 5,
        Refusal::Capacity => 6,
        Refusal::Integrity => 7,
        Refusal::Invalid => 8,
        Refusal::Busy => 9,
    }
}
pub(crate) fn refusal_from_code(code: u8) -> Result<Refusal> {
    Ok(match code {
        1 => Refusal::Denied,
        2 => Refusal::Missing,
        3 => Refusal::Conflict,
        4 => Refusal::Unavailable,
        5 => Refusal::Unsupported,
        6 => Refusal::Capacity,
        7 => Refusal::Integrity,
        8 => Refusal::Invalid,
        9 => Refusal::Busy,
        _ => return Err(Error::Invalid),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn bounded_prefix_requires_exact_magic_major_and_declared_length() {
        let mut bytes = [0u8; 4096];
        bytes[..8].copy_from_slice(super::MAGIC);
        bytes[8..12].copy_from_slice(&super::VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&1024u32.to_le_bytes());
        assert_eq!(super::header_bytes(&bytes).unwrap(), 1024);
        for cut in 0..super::PREFIX_BYTES {
            assert!(super::header_bytes(&bytes[..cut]).is_err());
        }
        for length in [0, 15, super::MAX_HEADER as u32 + 1] {
            bytes[12..16].copy_from_slice(&length.to_le_bytes());
            assert!(super::header_bytes(&bytes).is_err());
        }
        bytes[12..16].copy_from_slice(&1024u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&(super::VERSION + 1).to_le_bytes());
        assert!(matches!(
            super::header_bytes(&bytes),
            Err(super::Error::Version)
        ));
        bytes[8..12].copy_from_slice(&super::VERSION.to_le_bytes());
        bytes[0] ^= 1;
        assert!(super::header_bytes(&bytes).is_err());
    }

    use super::*;
    use alloc::{string::ToString, vec};
    fn object() -> Object {
        Object {
            workspace: "owner".to_string(),
            bucket: "home".to_string(),
            key: "column/block".to_string(),
        }
    }
    fn version() -> Version {
        Version {
            etag: Some("etag".to_string()),
            generation: Some("generation".to_string()),
        }
    }
    #[test]
    fn wire_requests_roundtrip_all_actions_and_borrow_secret_from_control_buffer() {
        for operation in [
            Operation::Read {
                version: version(),
                offset: 7,
                length: 31,
            },
            Operation::Create { length: 41 },
            Operation::List {
                limit: 10,
                after: Some("column/a".to_string()),
            },
            Operation::Replace {
                condition: Condition::Match(version()),
                length: 23,
            },
        ] {
            let request = Request {
                id: 17,
                scope: Scope {
                    workspace: "requester".to_string(),
                    run: "run".to_string(),
                },
                object: object(),
                expected_role: None,
                proof: crate::auth::Proof::Application(b"TEST-ONLY-OPAQUE"),
                operation,
            };
            let mut buffer = vec![0; MAX_HEADER];
            let len = request.encode_into(&mut buffer).unwrap();
            let bytes = &buffer[..len];
            let parsed = Request::decode(bytes).unwrap();
            assert_eq!(parsed.id, 17);
            assert_eq!(parsed.operation, request.operation);
            assert_eq!(parsed.object, request.object);
            assert_eq!(parsed.scope, request.scope);
            assert_eq!(
                parsed.proof.application().unwrap(),
                request.proof.application().unwrap()
            );
            let start = bytes.as_ptr() as usize;
            let cap = parsed.proof.application().unwrap().as_ptr() as usize;
            assert!(
                cap >= start
                    && cap + parsed.proof.application().unwrap().len() <= start + bytes.len()
            );
            for cut in 0..len {
                assert!(Request::decode(&bytes[..cut]).is_err());
            }
            let mut wrong = bytes.to_vec();
            wrong[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
            assert!(matches!(Request::decode(&wrong), Err(Error::Version)));
        }
    }
    #[test]
    fn wire_response_binds_range_and_unknown_refusal_is_not_success() {
        let metadata = Metadata {
            object: object(),
            version: version(),
            bytes: 64,
        };
        for outcome in [
            Outcome::Read {
                metadata: metadata.clone(),
                offset: 7,
                length: 57,
            },
            Outcome::Written(metadata.clone()),
            Outcome::Listed {
                entries: vec![metadata.clone()],
                next: Some(metadata.object.key.clone()),
            },
            Outcome::Refused(Refusal::Denied),
        ] {
            let response = Response { id: 1, outcome };
            let mut bytes = vec![0; MAX_HEADER];
            let len = response.encode_into(&mut bytes).unwrap();
            assert_eq!(Response::decode(&bytes[..len]).unwrap(), response);
        }
        let mut bytes = vec![0; MAX_HEADER];
        assert!(
            Response {
                id: 1,
                outcome: Outcome::Read {
                    metadata,
                    offset: 63,
                    length: 2
                }
            }
            .encode_into(&mut bytes)
            .is_err()
        );
        let len = Response {
            id: 1,
            outcome: Outcome::Refused(Refusal::Denied),
        }
        .encode_into(&mut bytes)
        .unwrap();
        bytes[len - 1] = 255;
        assert!(Response::decode(&bytes[..len]).is_err());
    }
    #[test]
    fn wire_body_bounds_charged_destination_and_refuses_truncation_overdelivery() {
        let mut body = Body::new(15);
        assert_eq!(body.limit(16, 8, 32).unwrap(), 8);
        body.received(8).unwrap();
        assert!(!body.complete());
        assert!(matches!(body.received(0), Err(Error::Incomplete)));
        assert!(body.received(8).is_err());
        assert!(!body.complete());
        body.received(7).unwrap();
        assert!(body.complete());
        assert!(body.limit(16, 17, 1).is_err());
    }
}
