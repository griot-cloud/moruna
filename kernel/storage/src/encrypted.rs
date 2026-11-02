//! Actual NVMe cache/scratch wrapper using ONE shared envelope/key/cipher authority.
//! No plaintext cache volume is exposed through the object port, no fallback is home,
//! and these ephemeral objects are not historical task-row retention.
use crate::{
    device::Device,
    keys::{IssuedCipher, Keys},
    memory::Loan,
    session::Backend,
    volume::Volume,
};
use alloc::{format, string::String, vec::Vec};
use moruna_storage_format::{
    Metadata, Object, VERSION, Version,
    auth::{Action, Authorization, SessionRole, Verifier},
    cache::{Binding, PREFIX, Prefix, object_slot},
    wire::{Condition, Operation, Outcome, Refusal, Request},
};
use moruna_svc::{Handle, Memory, PrivateMemory};
/// Only an actual CacheScratch superblock, real workspace key capability and epoch.
pub struct Encrypted {
    raw: Volume,
    keys: Keys,
    failed: bool,
}
struct Loaded {
    plaintext: PrivateMemory,
    start: usize,
    binding: Binding,
    _cipher: IssuedCipher,
}
impl Encrypted {
    /// Mount a real exclusive raw cache namespace. Wrong role/missing key refuses;
    /// never formats, selects a different namespace, or opens plaintext cache access.
    pub fn mount(
        device: Device,
        service_account: &Handle,
        alignment: u32,
        keys: Keys,
    ) -> Result<Self, Refusal> {
        let raw = Volume::mount_cache(device, service_account, alignment)?;
        raw.cache_identity()?;
        Ok(Self {
            raw,
            keys,
            failed: false,
        })
    }
    /// Explicit cache provisioning on an exclusively leased, all-zero-root region.
    /// The owner must separately authorize this destructive action. Never called by
    /// mount or normal service startup; existing/old/corrupt media refuses. The raw
    /// cache volume remains private, so no plaintext object backend is exposed.
    pub fn format(
        device: Device,
        service_account: &Handle,
        geometry: moruna_storage_format::Superblock,
        keys: Keys,
    ) -> Result<Self, Refusal> {
        let raw = Volume::format_cache(device, service_account, geometry)?;
        raw.cache_identity()?;
        Ok(Self {
            raw,
            keys,
            failed: false,
        })
    }
    fn namespace(&self, requester: &str) -> Result<String, Refusal> {
        let epoch = self.keys.epoch_id()?;
        Ok(format!(
            "cache/{}/{}/",
            hex(&epoch),
            hex(blake3::hash(requester.as_bytes()).as_bytes())
        ))
    }
    fn slot(&self, request: &Request<'_>) -> Result<Object, Refusal> {
        let (workspace, bucket) = self.raw.cache_identity()?;
        let digest =
            object_slot(&request.scope.workspace, &request.object).map_err(|_| Refusal::Invalid)?;
        Ok(Object {
            workspace: workspace.into(),
            bucket: bucket.into(),
            key: format!(
                "{}scratch/{}",
                self.namespace(&request.scope.workspace)?,
                hex(&digest)
            ),
        })
    }
    /// Actual issued cipher/plaintext ownership, not a caller-asserted idle flag.
    pub fn quiescent(&self) -> bool {
        self.keys.quiescent()
    }
    /// Quiesce actual issued ciphers/private plaintext, destroy epoch FIRST, then clear
    /// and sync the actual cache log under its exclusive namespace lease. No physical
    /// media sanitization or user-returned plaintext erasure is promised.
    pub fn recycle(&mut self, service_account: &Handle) -> Result<(), Refusal> {
        if !self.keys.quiescent() {
            return Err(Refusal::Unavailable);
        }
        let result = self
            .keys
            .recycle_epoch()
            .and_then(|()| self.raw.recycle_cache(service_account));
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn load(
        &mut self,
        request: &Request<'_>,
        slot: &Object,
        account: &Handle,
        verifier: &dyn Verifier,
        expected: Option<&Object>,
    ) -> Result<Loaded, Refusal> {
        // A fresh exact-action decision occurs BEFORE even a local cache hit/index read.
        let authorization = authorization(request)?;
        let (key_id, cipher) = self.keys.cipher(verifier, &authorization)?;
        let meta = self.raw.current_slot(slot)?;
        let length = usize::try_from(meta.bytes).map_err(|_| Refusal::Capacity)?;
        if length < PREFIX {
            return Err(Refusal::Integrity);
        }
        let stage = Loan::new(
            Memory::create_in(meta.bytes, account).map_err(|_| Refusal::Capacity)?,
            0,
            meta.bytes,
            true,
        )
        .map_err(|_| Refusal::Unavailable)?;
        self.raw
            .read(slot, &meta.version, 0, meta.bytes, Some(&stage), account)?;
        // Never turn an aliased peer/driver Mapping into a Rust mutable slice.
        let mut plaintext =
            PrivateMemory::create_in(meta.bytes, account).map_err(|_| Refusal::Capacity)?;
        let (binding, start) = plaintext
            .with_bytes(|bytes| {
                stage
                    .read(0, &mut bytes[..length])
                    .map_err(|_| Refusal::Unavailable)?;
                let prefix = Prefix::decode(&bytes[..PREFIX]).map_err(|_| Refusal::Integrity)?;
                let start = PREFIX
                    .checked_add(prefix.binding_bytes as usize)
                    .ok_or(Refusal::Integrity)?;
                if start > length {
                    return Err(Refusal::Integrity);
                }
                let binding =
                    Binding::decode(&bytes[PREFIX..start]).map_err(|_| Refusal::Integrity)?;
                if binding.key_id != key_id {
                    return Err(Refusal::Unavailable);
                }
                let (header, payload) = bytes[..length].split_at_mut(start);
                cipher
                    .open(&prefix.nonce, &header[PREFIX..], &prefix.tag, payload)
                    .map_err(|_| Refusal::Integrity)?;
                if binding.requester != request.scope.workspace
                    || binding.bytes != payload.len() as u64
                    || expected.is_some_and(|object| object != &binding.object)
                {
                    return Err(Refusal::Integrity);
                }
                Ok::<_, Refusal>((binding, start))
            })
            .map_err(|_| Refusal::Unavailable)??;
        Ok(Loaded {
            plaintext,
            start,
            binding,
            _cipher: cipher,
        })
    }
    fn range_slot(
        &self,
        request: &Request<'_>,
        version: &Version,
        offset: u64,
        length: u64,
    ) -> Result<Object, Refusal> {
        let (workspace, bucket) = self.raw.cache_identity()?;
        let digest = moruna_storage_format::cache::path_identity(
            &request.scope.workspace,
            &request.object,
            version,
            offset,
            length,
        )
        .map_err(|_| Refusal::Invalid)?;
        Ok(Object {
            workspace: workspace.into(),
            bucket: bucket.into(),
            key: format!(
                "{}ranges/{}",
                self.namespace(&request.scope.workspace)?,
                digest
            ),
        })
    }
    fn seal(
        &self,
        binding: &Binding,
        source: Option<&Loan>,
        cipher: &IssuedCipher,
        account: &Handle,
    ) -> Result<(Loan, u64), Refusal> {
        let aad = binding.encode().map_err(|_| Refusal::Capacity)?;
        let n = usize::try_from(binding.bytes).map_err(|_| Refusal::Capacity)?;
        let start = PREFIX.checked_add(aad.len()).ok_or(Refusal::Capacity)?;
        let total = start.checked_add(n).ok_or(Refusal::Capacity)?;
        let mut nonce = [0; 12];
        moruna_svc::random(&mut nonce).map_err(|_| Refusal::Unavailable)?;
        let mut envelope =
            PrivateMemory::create_in(total as u64, account).map_err(|_| Refusal::Capacity)?;
        envelope
            .with_bytes(|bytes| {
                bytes[PREFIX..start].copy_from_slice(&aad);
                if let Some(source) = source {
                    source
                        .read(0, &mut bytes[start..total])
                        .map_err(|_| Refusal::Unavailable)?;
                } else if n != 0 {
                    return Err(Refusal::Invalid);
                }
                let (header, payload) = bytes[..total].split_at_mut(start);
                let tag = cipher
                    .seal(&nonce, &header[PREFIX..], payload)
                    .map_err(|_| Refusal::Integrity)?;
                header[..PREFIX].copy_from_slice(
                    &Prefix {
                        binding_bytes: aad.len() as u32,
                        nonce,
                        tag,
                    }
                    .encode()
                    .map_err(|_| Refusal::Integrity)?,
                );
                Ok::<_, Refusal>(())
            })
            .map_err(|_| Refusal::Unavailable)??;
        Ok((
            Loan::new(envelope.into_memory(), 0, total as u64, false)
                .map_err(|_| Refusal::Unavailable)?,
            total as u64,
        ))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Request authority, immutable version, offset, length, destination, fill mode, service Account and verifier are distinct checked inputs."
    )]
    fn range(
        &mut self,
        request: &Request<'_>,
        version: &Version,
        offset: u64,
        length: u64,
        payload: Option<Loan>,
        fill: bool,
        account: &Handle,
        verifier: &dyn Verifier,
    ) -> Result<Outcome, Refusal> {
        if payload.as_ref().map(Loan::len).unwrap_or(0) != length {
            return Err(Refusal::Invalid);
        }
        let authorization = authorization(request)?;
        let (key_id, cipher) = self.keys.cipher(verifier, &authorization)?;
        let slot = self.range_slot(request, version, offset, length)?;
        let existing = match self.raw.current_slot(&slot) {
            Ok(meta) => Some(meta),
            Err(Refusal::Missing) => None,
            Err(error) => return Err(error),
        };
        if existing.is_some() {
            let mut actual = self.load(request, &slot, account, verifier, Some(&request.object))?;
            if actual.binding.object_version != *version
                || actual.binding.offset != offset
                || actual.binding.bytes != length
            {
                return Err(Refusal::Integrity);
            }
            if fill {
                // Existing immutable range cannot be silently poisoned/replaced with
                // different bytes under the same pinned home identity.
                let mut stage = PrivateMemory::create_in(length.max(1), account)
                    .map_err(|_| Refusal::Capacity)?;
                let actual_start = actual.start;
                let same = stage.with_bytes(|bytes| {
                    if let Some(source) = &payload {
                        source
                            .read(0, &mut bytes[..length as usize])
                            .map_err(|_| Refusal::Unavailable)?;
                    }
                    actual
                        .plaintext
                        .with_bytes(|actual| {
                            Ok::<_, Refusal>(
                                blake3::hash(&bytes[..length as usize])
                                    == blake3::hash(
                                        &actual[actual_start..actual_start + length as usize],
                                    ),
                            )
                        })
                        .map_err(|_| Refusal::Unavailable)?
                });
                // actual_start is the authenticated envelope payload boundary.
                if !same.map_err(|_| Refusal::Unavailable)?? {
                    return Err(Refusal::Integrity);
                }
                return Ok(Outcome::Cached);
            }
            if length != 0 {
                actual
                    .plaintext
                    .with_bytes(|bytes| {
                        payload
                            .as_ref()
                            .ok_or(Refusal::Invalid)?
                            .write(0, &bytes[actual.start..actual.start + length as usize])
                            .map_err(|_| Refusal::Unavailable)
                    })
                    .map_err(|_| Refusal::Unavailable)??;
            }
            return Ok(Outcome::CacheHit { offset, length });
        }
        if !fill {
            return Ok(Outcome::CacheMiss);
        }
        let binding = Binding {
            version: VERSION,
            requester: request.scope.workspace.clone(),
            object: request.object.clone(),
            object_version: version.clone(),
            offset,
            bytes: length,
            key_id,
        };
        let (stored, total) = self.seal(&binding, payload.as_ref(), &cipher, account)?;
        self.raw
            .put(&slot, &Condition::Absent, Some(stored), total, account)?;
        Ok(Outcome::Cached)
    }
    fn put(
        &mut self,
        request: &Request<'_>,
        length: u64,
        source: Option<Loan>,
        condition: &Condition,
        account: &Handle,
        verifier: &dyn Verifier,
    ) -> Result<Outcome, Refusal> {
        let authorization = authorization(request)?;
        let (key_id, cipher) = self.keys.cipher(verifier, &authorization)?;
        if source.as_ref().map(Loan::len).unwrap_or(0) != length {
            return Err(Refusal::Invalid);
        }
        let slot = self.slot(request)?;
        let current = match self.raw.current_slot(&slot) {
            Ok(meta) => Some(meta),
            Err(Refusal::Missing) => None,
            Err(error) => return Err(error),
        };
        match condition {
            Condition::Absent if current.is_some() => return Err(Refusal::Conflict),
            Condition::Match(wanted) => {
                if current.is_none() {
                    return Err(Refusal::Conflict);
                }
                let actual = self.load(request, &slot, account, verifier, Some(&request.object))?;
                if actual.binding.object_version != *wanted {
                    return Err(Refusal::Conflict);
                }
            }
            _ => {}
        }
        let mut unique = [0; 16];
        let mut nonce = [0; 12];
        moruna_svc::random(&mut unique).map_err(|_| Refusal::Unavailable)?;
        moruna_svc::random(&mut nonce).map_err(|_| Refusal::Unavailable)?;
        let n = usize::try_from(length).map_err(|_| Refusal::Capacity)?;
        let mut plain =
            PrivateMemory::create_in(length.max(1), account).map_err(|_| Refusal::Capacity)?;
        let digest = plain
            .with_bytes(|bytes| {
                if let Some(source) = &source {
                    source
                        .read(0, &mut bytes[..n])
                        .map_err(|_| Refusal::Unavailable)?;
                }
                Ok::<_, Refusal>(*blake3::hash(&bytes[..n]).as_bytes())
            })
            .map_err(|_| Refusal::Unavailable)??;
        let version = Version {
            etag: Some(hex(&digest)),
            generation: Some(hex(&unique)),
        };
        let binding = Binding {
            version: VERSION,
            requester: request.scope.workspace.clone(),
            object: request.object.clone(),
            object_version: version.clone(),
            offset: 0,
            bytes: length,
            key_id,
        };
        let aad = binding.encode().map_err(|_| Refusal::Capacity)?;
        let start = PREFIX.checked_add(aad.len()).ok_or(Refusal::Capacity)?;
        let total = start.checked_add(n).ok_or(Refusal::Capacity)?;
        let mut envelope =
            PrivateMemory::create_in(total as u64, account).map_err(|_| Refusal::Capacity)?;
        envelope
            .with_bytes(|bytes| {
                bytes[PREFIX..start].copy_from_slice(&aad);
                plain
                    .with_bytes(|plain| {
                        bytes[start..total].copy_from_slice(&plain[..n]);
                    })
                    .map_err(|_| Refusal::Unavailable)?;
                let (header, payload) = bytes[..total].split_at_mut(start);
                let tag = cipher
                    .seal(&nonce, &header[PREFIX..], payload)
                    .map_err(|_| Refusal::Integrity)?;
                header[..PREFIX].copy_from_slice(
                    &Prefix {
                        binding_bytes: aad.len() as u32,
                        nonce,
                        tag,
                    }
                    .encode()
                    .map_err(|_| Refusal::Integrity)?,
                );
                Ok::<_, Refusal>(())
            })
            .map_err(|_| Refusal::Unavailable)??;
        drop(plain); // infallible zeroization BEFORE ciphertext export/publication
        let stored = Loan::new(envelope.into_memory(), 0, total as u64, false)
            .map_err(|_| Refusal::Unavailable)?;
        let condition = current
            .map(|meta| Condition::Match(meta.version))
            .unwrap_or(Condition::Absent);
        self.raw
            .put(&slot, &condition, Some(stored), total as u64, account)?;
        Ok(Outcome::Written(Metadata {
            object: request.object.clone(),
            version,
            bytes: length,
        }))
    }
}
fn authorization<'a>(request: &'a Request<'a>) -> Result<Authorization<'a>, Refusal> {
    let action = match request.operation {
        Operation::Current
        | Operation::Head { .. }
        | Operation::Read { .. }
        | Operation::CacheRead { .. } => Action::Read,
        Operation::Create { .. } | Operation::CacheFill { .. } => Action::Write,
        Operation::Replace { .. } => Action::Publish,
        Operation::List { .. } => Action::List,
    };
    let authorization = match request.proof.application() {
        Some(proof) => Authorization::new(&request.scope, &request.object, action, proof),
        None => Authorization::native(&request.scope, &request.object, action),
    }?;
    match &request.operation {
        Operation::Head { version }
        | Operation::Read { version, .. }
        | Operation::CacheRead { version, .. }
        | Operation::CacheFill { version, .. }
        | Operation::Replace {
            condition: Condition::Match(version),
            ..
        } => authorization.with_version(version),
        _ => Ok(authorization),
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
impl Backend for Encrypted {
    fn role(&self) -> SessionRole {
        SessionRole::CacheScratch
    }
    fn execute(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
        verifier: &dyn Verifier,
    ) -> Result<Outcome, Refusal> {
        if self.failed {
            return Err(Refusal::Unavailable);
        }
        match &request.operation {
            Operation::CacheRead {
                version,
                offset,
                length,
            } => self.range(
                request, version, *offset, *length, payload, false, account, verifier,
            ),
            Operation::CacheFill {
                version,
                offset,
                length,
            } => self.range(
                request, version, *offset, *length, payload, true, account, verifier,
            ),
            Operation::Create { length } => self.put(
                request,
                *length,
                payload,
                &Condition::Absent,
                account,
                verifier,
            ),
            Operation::Replace { condition, length } => {
                self.put(request, *length, payload, condition, account, verifier)
            }
            Operation::List { limit, after } => {
                let auth = authorization(request)?;
                verifier.verify(&auth)?;
                let namespace = format!("{}scratch/", self.namespace(&request.scope.workspace)?);
                let mut entries = Vec::new();
                for slot in self.raw.slots() {
                    if !slot.object.key.starts_with(&namespace) {
                        continue;
                    }
                    let loaded = self.load(request, &slot.object, account, verifier, None)?;
                    let object = &loaded.binding.object;
                    if object.workspace == request.object.workspace
                        && object.bucket == request.object.bucket
                        && object.key.starts_with(&request.object.key)
                        && after.as_ref().is_none_or(|after| &object.key > after)
                    {
                        entries.push(Metadata {
                            object: object.clone(),
                            version: loaded.binding.object_version.clone(),
                            bytes: loaded.binding.bytes,
                        });
                    }
                }
                entries.sort_by(|a, b| a.object.key.cmp(&b.object.key));
                let more = entries.len() > *limit as usize;
                entries.truncate(*limit as usize);
                let next = if more {
                    entries.last().map(|meta| meta.object.key.clone())
                } else {
                    None
                };
                Ok(Outcome::Listed { entries, next })
            }
            _ => {
                let slot = self.slot(request)?;
                let mut loaded =
                    self.load(request, &slot, account, verifier, Some(&request.object))?;
                if loaded.binding.offset != 0 {
                    return Err(Refusal::Integrity);
                }
                let metadata = Metadata {
                    object: loaded.binding.object.clone(),
                    version: loaded.binding.object_version.clone(),
                    bytes: loaded.binding.bytes,
                };
                match &request.operation {
                    Operation::Current => Ok(Outcome::Head(metadata)),
                    Operation::Head { version } if version == &metadata.version => {
                        Ok(Outcome::Head(metadata))
                    }
                    Operation::Read {
                        version,
                        offset,
                        length,
                    } => {
                        if version != &metadata.version {
                            return Err(Refusal::Conflict);
                        }
                        if offset
                            .checked_add(*length)
                            .is_none_or(|end| end > metadata.bytes)
                            || payload.as_ref().map(Loan::len).unwrap_or(0) != *length
                        {
                            return Err(Refusal::Invalid);
                        }
                        if *length != 0 {
                            loaded
                                .plaintext
                                .with_bytes(|bytes| {
                                    payload
                                        .as_ref()
                                        .ok_or(Refusal::Invalid)?
                                        .write(
                                            0,
                                            &bytes[loaded.start + *offset as usize
                                                ..loaded.start + (*offset + *length) as usize],
                                        )
                                        .map_err(|_| Refusal::Unavailable)
                                })
                                .map_err(|_| Refusal::Unavailable)??;
                        }
                        Ok(Outcome::Read {
                            metadata,
                            offset: *offset,
                            length: *length,
                        })
                    }
                    Operation::Head { .. } => Err(Refusal::Conflict),
                    _ => Err(Refusal::Invalid),
                }
            }
        }
    }
}
