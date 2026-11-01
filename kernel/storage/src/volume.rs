//! Native receipt-backed volume on the REAL exclusive block driver. The sole portable
//! Journal/Frame/BlockProof authority is shared with Linux; no alternate disk parser or
//! read+write CAS exists. Current adapter uses explicitly charged one-block DMA/CPU
//! staging; its copy counter is honest and does not assert direct caller-arena ingress.
use crate::{
    device::Device,
    memory::{Loan, dma},
    session::Backend,
};
use alloc::{format, rc::Rc, string::String, vec::Vec};
use core::cell::Cell;
use moruna_storage_format::{
    BlockProof, Commit, Frame, Location, Metadata, Object, Role, Superblock, Version,
    auth::SessionRole,
    journal::Journal,
    leaf_digest, tree_node,
    wire::{Condition, Operation, Outcome, Refusal, Request},
};
use moruna_svc::{Handle, Memory, PrivateMemory};
#[path = "volume_async.rs"]
mod async_io;
#[path = "volume_compact.rs"]
mod compact;
#[path = "volume_reserved.rs"]
mod reserved;
pub(crate) use reserved::ReservedRead;

/// Actual mounted explicit raw-home/checkpoint volume. Plaintext CacheScratch roles
/// refuse: the separate encrypted cache must use the sole workspace Cipher authority.
pub struct Volume {
    device: Device,
    journal: Journal,
    root: moruna_storage_format::banks::Root,
    owner_identity: Rc<()>,
    copied: u64,
    failed: bool,
    async_failed: Cell<bool>,
    _index_budget: Memory,
}
impl Volume {
    /// Read actual superblock and receipt log under the device's real exclusive lease.
    /// Caller supplies the explicit boot service Account for shared index/scratch budget.
    /// No empty-device formatting, role inference or fallback occurs on mount failure.
    pub fn mount(
        device: Device,
        account: &Handle,
        alignment: u32,
        expected_role: Role,
    ) -> Result<Self, Refusal> {
        if expected_role == Role::CacheScratch {
            return Err(Refusal::Unsupported);
        }
        Self::mount_inner(device, account, alignment, expected_role)
    }
    pub(crate) fn mount_cache(
        device: Device,
        account: &Handle,
        alignment: u32,
    ) -> Result<Self, Refusal> {
        Self::mount_inner(device, account, alignment, Role::CacheScratch)
    }
    fn mount_inner(
        mut device: Device,
        account: &Handle,
        alignment: u32,
        expected_role: Role,
    ) -> Result<Self, Refusal> {
        if !matches!(
            expected_role,
            Role::SingleNodeHome | Role::CheckpointMetadata | Role::CacheScratch
        ) || alignment < device.info().sector_size
            || !alignment.is_power_of_two()
            || !alignment.is_multiple_of(device.info().sector_size)
        {
            return Err(Refusal::Invalid);
        }
        let mut first =
            PrivateMemory::create_in(alignment as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut second =
            PrivateMemory::create_in(alignment as u64, account).map_err(|_| Refusal::Capacity)?;
        let recovery = first
            .with_bytes(|a| {
                second
                    .with_bytes(|b| {
                        read_block(&mut device, account, 0, &mut a[..alignment as usize])?;
                        read_block(
                            &mut device,
                            account,
                            u64::from(alignment),
                            &mut b[..alignment as usize],
                        )?;
                        moruna_storage_format::banks::Root::recover(
                            &a[..alignment as usize],
                            &b[..alignment as usize],
                        )
                        .map_err(|_| Refusal::Integrity)
                    })
                    .map_err(|_| Refusal::Unavailable)?
            })
            .map_err(|_| Refusal::Unavailable)??;
        let root = recovery.root().clone();
        let geometry = root.geometry().clone();
        if root.physical_capacity() != device.capacity()
            || geometry.alignment != alignment
            || geometry.role != expected_role
        {
            return Err(Refusal::Invalid);
        }
        let index_budget =
            Memory::create_in(geometry.log_bytes.max(1), account).map_err(|_| Refusal::Capacity)?;
        let mut journal = Journal::new(geometry).map_err(|_| Refusal::Integrity)?;
        let cell = journal.cell_bytes() as usize;
        let mut record =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut receipt =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        while journal.has_capacity() {
            let at = journal.sequence();
            let accepted = record
                .with_bytes(|record| {
                    receipt
                        .with_bytes(|receipt| {
                            read_aligned(&mut device, account, at, &mut record[..cell])?;
                            read_aligned(
                                &mut device,
                                account,
                                at + cell as u64,
                                &mut receipt[..cell],
                            )?;
                            journal
                                .replay(&record[..cell], &receipt[..cell])
                                .map_err(|_| Refusal::Integrity)
                        })
                        .map_err(|_| Refusal::Unavailable)?
                })
                .map_err(|_| Refusal::Unavailable)??;
            if !accepted {
                break;
            }
        }
        if let Some(retire) = recovery.retirement() {
            first
                .with_bytes(|bytes| {
                    bytes.fill(0);
                    write_aligned(&mut device, account, retire, &bytes[..alignment as usize])?;
                    device.flush().map_err(device_refusal)
                })
                .map_err(|_| Refusal::Unavailable)??;
        }
        Ok(Self {
            device,
            journal,
            root,
            owner_identity: Rc::new(()),
            copied: 0,
            failed: false,
            async_failed: Cell::new(false),
            _index_budget: index_budget,
        })
    }
    /// Explicit provisioning ONLY on an actual exclusively leased device whose initial
    /// superblock bytes are all zero. Never called as a mount/read fallback. This clears
    /// the journal, not all historical media; no physical sanitization is claimed.
    pub fn format(device: Device, account: &Handle, geometry: Superblock) -> Result<Self, Refusal> {
        if geometry.role == Role::CacheScratch {
            return Err(Refusal::Unsupported);
        }
        Self::provision(device, account, geometry)
    }
    pub(crate) fn format_cache(
        device: Device,
        account: &Handle,
        geometry: Superblock,
    ) -> Result<Self, Refusal> {
        if geometry.role != Role::CacheScratch {
            return Err(Refusal::Invalid);
        }
        Self::provision(device, account, geometry)
    }
    fn provision(
        mut device: Device,
        account: &Handle,
        geometry: Superblock,
    ) -> Result<Self, Refusal> {
        geometry.validate().map_err(|_| Refusal::Invalid)?;
        if geometry.capacity != device.capacity()
            || !matches!(
                geometry.role,
                Role::SingleNodeHome | Role::CheckpointMetadata | Role::CacheScratch
            )
            || !geometry.alignment.is_multiple_of(device.info().sector_size)
        {
            return Err(Refusal::Invalid);
        }
        let root = moruna_storage_format::banks::Root::initial(
            device.capacity(),
            geometry.alignment,
            geometry.log_bytes,
            geometry.role,
            geometry.workspace.clone(),
            geometry.bucket.clone(),
        )
        .map_err(|_| Refusal::Invalid)?;
        let geometry = root.geometry().clone();
        Journal::new(geometry.clone()).map_err(|_| Refusal::Invalid)?;
        let a = geometry.alignment as usize;
        let encoded = root.encode().map_err(|_| Refusal::Capacity)?;
        let mut scratch =
            PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?;
        scratch
            .with_bytes(|bytes| {
                for slot in [0, a as u64] {
                    read_block(&mut device, account, slot, &mut bytes[..a])?;
                    if bytes[..a].iter().any(|byte| *byte != 0) {
                        return Err(Refusal::Conflict);
                    }
                }
                bytes.fill(0);
                for at in (geometry.log_offset..geometry.data_offset).step_by(a) {
                    write_aligned(&mut device, account, at, &bytes[..a])?;
                }
                device.flush().map_err(device_refusal)?;
                bytes[..encoded.len()].copy_from_slice(&encoded);
                write_aligned(&mut device, account, 0, &bytes[..a])?;
                device.flush().map_err(device_refusal)
            })
            .map_err(|_| Refusal::Unavailable)??;
        Self::mount_inner(device, account, geometry.alignment, geometry.role)
    }
    /// Actual last block-driver diagnostic, retained rather than hidden by compact wire codes.
    pub fn last_device_error(&self) -> Option<crate::device::Error> {
        self.device.last_error()
    }
    /// Actual bytes copied across the caller-loan/private-block boundary. Driver and
    /// metadata staging are additional costs; this is NOT a total copy-cost metric.
    pub fn caller_copy_bytes(&self) -> u64 {
        self.copied
    }
    /// Actual validated mounted identity; never supplied by an untrusted request label.
    pub(crate) fn identity(&self) -> (&str, &str, Role) {
        let geometry = self.journal.geometry();
        (&geometry.workspace, &geometry.bucket, geometry.role)
    }
    pub(crate) fn cache_identity(&self) -> Result<(&str, &str), Refusal> {
        let geometry = self.journal.geometry();
        if geometry.role != Role::CacheScratch {
            return Err(Refusal::Denied);
        }
        Ok((&geometry.workspace, &geometry.bucket))
    }
    /// Actual internal transaction health, not a permission or a reset facility.
    /// Failed/dropped asynchronous operations cannot be reopened through this accessor.
    pub(crate) fn admission_open(&self) -> bool {
        !self.failed && !self.async_failed.get()
    }
    /// Identity of this actual mounted owner, never serialized or caller-provided.
    pub(crate) fn owner_identity(&self) -> Rc<()> {
        self.owner_identity.clone()
    }
    /// Bounded actual committed namespace roots, not a caller-provided GC list.
    pub(crate) fn namespace_roots(&self, limit: usize) -> Result<Vec<Metadata>, Refusal> {
        if self.failed || self.async_failed.get() || limit == 0 {
            return Err(Refusal::Unavailable);
        }
        let mut roots = Vec::new();
        roots
            .try_reserve_exact(limit)
            .map_err(|_| Refusal::Capacity)?;
        for entry in self.journal.entries() {
            if moruna_storage_format::namespace::root_object(&entry.metadata.object).is_ok() {
                if roots.len() == limit {
                    return Err(Refusal::Capacity);
                }
                roots.push(entry.metadata.clone());
            }
        }
        Ok(roots)
    }
    pub(crate) fn slots(&self) -> Vec<Metadata> {
        self.journal
            .entries()
            .map(|entry| entry.metadata.clone())
            .collect()
    }
    pub(crate) fn current_slot(&self, object: &Object) -> Result<Metadata, Refusal> {
        self.object(object)?;
        Ok(self
            .journal
            .current(&object.key)
            .ok_or(Refusal::Missing)?
            .metadata
            .clone())
    }
    pub(crate) fn recycle_cache(&mut self, account: &Handle) -> Result<(), Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        let geometry = self.journal.geometry().clone();
        if geometry.role != Role::CacheScratch {
            return Err(Refusal::Denied);
        }
        let a = geometry.alignment as usize;
        let mut scratch =
            PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?;
        let result = scratch
            .with_bytes(|bytes| {
                bytes.fill(0);
                for at in (geometry.log_offset..geometry.data_offset).step_by(a) {
                    write_aligned(&mut self.device, account, at, &bytes[..a])?;
                }
                self.device.flush().map_err(device_refusal)
            })
            .map_err(|_| Refusal::Unavailable)?;
        if let Err(cause) = result {
            self.failed = true;
            return Err(cause);
        }
        self.journal = Journal::new(geometry).map_err(|_| Refusal::Integrity)?;
        Ok(())
    }
    fn object(&self, object: &Object) -> Result<(), Refusal> {
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        object.validate().map_err(|_| Refusal::Invalid)?;
        let geometry = self.journal.geometry();
        if object.workspace != geometry.workspace || object.bucket != geometry.bucket {
            return Err(Refusal::Denied);
        }
        Ok(())
    }
    pub(crate) fn put(
        &mut self,
        object: &Object,
        condition: &Condition,
        source: Option<Loan>,
        bytes: u64,
        account: &Handle,
    ) -> Result<Metadata, Refusal> {
        self.object(object)?;
        if source.as_ref().map(Loan::len).unwrap_or(0) != bytes {
            return Err(Refusal::Invalid);
        }
        let a = self.journal.geometry().alignment as usize;
        let mut location = self
            .journal
            .plan(object, condition, bytes)
            .map_err(|_| Refusal::Capacity)?
            .ok_or(Refusal::Conflict)?;
        let leaves = bytes.div_ceil(a as u64);
        // Refuse unrepresentable metadata and unavailable entropy BEFORE payload IO.
        let mut unique = [0; 16];
        moruna_svc::random(&mut unique).map_err(|_| Refusal::Unavailable)?;
        let preview = Frame::Commit(Commit {
            sequence: self.journal.sequence(),
            metadata: Metadata {
                object: object.clone(),
                bytes,
                version: Version {
                    etag: Some(hex(&[0; 32])),
                    generation: Some(hex(&unique)),
                },
            },
            content_hash: [0; 32],
            location: location.clone(),
        })
        .encode()
        .map_err(|_| Refusal::Capacity)?;
        if preview.len() as u64 > self.journal.cell_bytes() {
            return Err(Refusal::Capacity);
        }

        // Actual charged reservation bounds the temporary digest vectors before IO.
        // The service's own heap remains independently charged by its kernel Account.
        let _digest_budget = Memory::create_in(
            leaves.checked_mul(64).ok_or(Refusal::Capacity)?.max(1),
            account,
        )
        .map_err(|_| Refusal::Capacity)?;
        let mut scratch =
            PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut digests = Vec::new();
        digests
            .try_reserve_exact(usize::try_from(leaves).map_err(|_| Refusal::Capacity)?)
            .map_err(|_| Refusal::Capacity)?;
        let mut hash = blake3::Hasher::new();
        if let Location::Extent {
            offset, tree, root, ..
        } = &mut location
        {
            for block in 0..leaves {
                let count = (bytes - block * a as u64).min(a as u64) as usize;
                let digest = scratch
                    .with_bytes(|data| {
                        data.fill(0);
                        source
                            .as_ref()
                            .ok_or(Refusal::Invalid)?
                            .read(block * a as u64, &mut data[..count])
                            .map_err(|_| Refusal::Unavailable)?;
                        hash.update(&data[..count]);
                        let digest =
                            leaf_digest(&data[..count], a).map_err(|_| Refusal::Integrity)?;
                        write_aligned(
                            &mut self.device,
                            account,
                            *offset + block * a as u64,
                            &data[..a],
                        )?;
                        Ok::<_, Refusal>(digest)
                    })
                    .map_err(|_| Refusal::Unavailable)??;
                self.copied = self
                    .copied
                    .checked_add(count as u64)
                    .ok_or(Refusal::Capacity)?;
                digests.push(digest);
            }
            for level in tree {
                let mut parents = Vec::new();
                parents
                    .try_reserve_exact(usize::try_from(level.nodes).map_err(|_| Refusal::Capacity)?)
                    .map_err(|_| Refusal::Capacity)?;
                for (index, children) in digests.chunks(a / 32).enumerate() {
                    let digest = scratch
                        .with_bytes(|data| {
                            let digest = tree_node(&mut data[..a], children)
                                .map_err(|_| Refusal::Integrity)?;
                            write_aligned(
                                &mut self.device,
                                account,
                                level.offset + index as u64 * a as u64,
                                &data[..a],
                            )?;
                            Ok::<_, Refusal>(digest)
                        })
                        .map_err(|_| Refusal::Unavailable)??;
                    parents.push(digest);
                }
                digests = parents;
            }
            if let Some(digest) = digests.first() {
                *root = *digest;
            }
        } else {
            return Err(Refusal::Integrity);
        }
        let digest = *hash.finalize().as_bytes();

        let metadata = Metadata {
            object: object.clone(),
            bytes,
            version: Version {
                etag: Some(hex(&digest)),
                generation: Some(hex(&unique)),
            },
        };
        let commit = Commit {
            sequence: self.journal.sequence(),
            metadata: metadata.clone(),
            content_hash: digest,
            location,
        };
        self.journal
            .validate_commit(&commit)
            .map_err(|_| Refusal::Integrity)?;
        let record = Frame::Commit(commit)
            .encode()
            .map_err(|_| Refusal::Capacity)?;
        let cell = self.journal.cell_bytes() as usize;
        if record.len() > cell {
            return Err(Refusal::Capacity);
        }
        self.device.flush().map_err(device_refusal)?;
        let receipt = Frame::Receipt {
            sequence: self.journal.sequence(),
            commit_hash: *blake3::hash(&record).as_bytes(),
        }
        .encode()
        .map_err(|_| Refusal::Capacity)?;
        let mut record_cell =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        let mut receipt_cell =
            PrivateMemory::create_in(cell as u64, account).map_err(|_| Refusal::Capacity)?;
        record_cell
            .with_bytes(|data| {
                data.fill(0);
                data[..record.len()].copy_from_slice(&record);
                write_aligned(
                    &mut self.device,
                    account,
                    self.journal.sequence(),
                    &data[..cell],
                )
            })
            .map_err(|_| Refusal::Unavailable)??;
        self.device.flush().map_err(device_refusal)?;
        receipt_cell
            .with_bytes(|data| {
                data.fill(0);
                data[..receipt.len()].copy_from_slice(&receipt);
                write_aligned(
                    &mut self.device,
                    account,
                    self.journal.sequence() + cell as u64,
                    &data[..cell],
                )
            })
            .map_err(|_| Refusal::Unavailable)??;
        self.device.flush().map_err(device_refusal)?;
        let applied = record_cell
            .with_bytes(|record| {
                receipt_cell
                    .with_bytes(|receipt| {
                        self.journal
                            .replay(&record[..cell], &receipt[..cell])
                            .map_err(|_| Refusal::Integrity)
                    })
                    .map_err(|_| Refusal::Unavailable)?
            })
            .map_err(|_| Refusal::Unavailable)??;
        if !applied {
            return Err(Refusal::Integrity);
        }
        Ok(metadata)
    }
    pub(crate) fn read(
        &mut self,
        object: &Object,
        version: &Version,
        offset: u64,
        length: u64,
        destination: Option<&Loan>,
        account: &Handle,
    ) -> Result<Outcome, Refusal> {
        self.object(object)?;
        let commit = self.journal.current(&object.key).ok_or(Refusal::Missing)?;
        if &commit.metadata.version != version {
            return Err(Refusal::Conflict);
        }
        if offset
            .checked_add(length)
            .is_none_or(|end| end > commit.metadata.bytes)
            || destination.map(Loan::len).unwrap_or(0) != length
        {
            return Err(Refusal::Invalid);
        }
        let metadata = commit.metadata.clone();
        if length == 0 {
            return Ok(Outcome::Read {
                metadata,
                offset,
                length,
            });
        }
        let a = self.journal.geometry().alignment as u64;
        let start = match &commit.location {
            Location::Extent { offset, .. } => *offset,
            _ => return Err(Refusal::Integrity),
        };
        let mut data = PrivateMemory::create_in(a, account).map_err(|_| Refusal::Capacity)?;
        let mut node = PrivateMemory::create_in(a, account).map_err(|_| Refusal::Capacity)?;
        for block in offset / a..(offset + length).div_ceil(a) {
            let mut proof = BlockProof::new(commit, self.journal.geometry(), block)
                .map_err(|_| Refusal::Integrity)?;
            while let Some(node_offset) = proof.next_node().map(|step| step.offset) {
                node.with_bytes(|bytes| {
                    read_block(
                        &mut self.device,
                        account,
                        node_offset,
                        &mut bytes[..a as usize],
                    )?;
                    proof
                        .accept_node(&bytes[..a as usize])
                        .map_err(|_| Refusal::Integrity)
                })
                .map_err(|_| Refusal::Unavailable)??;
            }
            let copied = data
                .with_bytes(|bytes| {
                    read_block(
                        &mut self.device,
                        account,
                        start + block * a,
                        &mut bytes[..a as usize],
                    )?;
                    proof
                        .verify_data(&bytes[..a as usize])
                        .map_err(|_| Refusal::Integrity)?;
                    let lo = offset.max(block * a);
                    let hi = (offset + length).min((block + 1) * a);
                    destination
                        .as_ref()
                        .ok_or(Refusal::Invalid)?
                        .write(
                            lo - offset,
                            &bytes[(lo - block * a) as usize..(hi - block * a) as usize],
                        )
                        .map_err(|_| Refusal::Unavailable)?;
                    Ok::<_, Refusal>(hi - lo)
                })
                .map_err(|_| Refusal::Unavailable)??;
            self.copied = self.copied.checked_add(copied).ok_or(Refusal::Capacity)?;
        }
        Ok(Outcome::Read {
            metadata,
            offset,
            length,
        })
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn device_refusal(cause: crate::device::Error) -> Refusal {
    match cause {
        crate::device::Error::Busy => Refusal::Busy,
        _ => Refusal::Unavailable,
    }
}
fn read_block(
    device: &mut Device,
    account: &Handle,
    offset: u64,
    destination: &mut [u8],
) -> Result<(), Refusal> {
    let sector = device.info().sector_size as u64;
    if !offset.is_multiple_of(sector) || !(destination.len() as u64).is_multiple_of(sector) {
        return Err(Refusal::Invalid);
    }
    let memory = dma(destination.len() as u64, account).map_err(|_| Refusal::Capacity)?;
    let memory = device
        .read(
            offset / sector,
            u32::try_from(destination.len() as u64 / sector).map_err(|_| Refusal::Capacity)?,
            memory,
        )
        .map_err(device_refusal)?;
    Loan::new(memory, 0, destination.len() as u64, false)
        .map_err(|_| Refusal::Unavailable)?
        .read(0, destination)
        .map_err(|_| Refusal::Unavailable)
}
fn read_aligned(
    device: &mut Device,
    account: &Handle,
    offset: u64,
    destination: &mut [u8],
) -> Result<(), Refusal> {
    let max = device.info().max_sectors as usize * device.info().sector_size as usize;
    for (index, chunk) in destination.chunks_mut(max).enumerate() {
        read_block(device, account, offset + (index * max) as u64, chunk)?;
    }
    Ok(())
}
fn write_aligned(
    device: &mut Device,
    account: &Handle,
    offset: u64,
    source: &[u8],
) -> Result<(), Refusal> {
    let sector = device.info().sector_size as usize;
    let max = device.info().max_sectors as usize * sector;
    if !offset.is_multiple_of(sector as u64) || !source.len().is_multiple_of(sector) {
        return Err(Refusal::Invalid);
    }
    for (index, chunk) in source.chunks(max).enumerate() {
        let memory = dma(chunk.len() as u64, account).map_err(|_| Refusal::Capacity)?;
        let loan =
            Loan::new(memory, 0, chunk.len() as u64, true).map_err(|_| Refusal::Unavailable)?;
        loan.write(0, chunk).map_err(|_| Refusal::Unavailable)?;
        device
            .write(
                offset / sector as u64 + (index * max / sector) as u64,
                (chunk.len() / sector) as u32,
                loan.duplicate(
                    moruna_svc::Rights::READ
                        | moruna_svc::Rights::WRITE
                        | moruna_svc::Rights::MAP
                        | moruna_svc::Rights::TRANSFER,
                )
                .map_err(|_| Refusal::Unavailable)?,
            )
            .map_err(device_refusal)?;
    }
    Ok(())
}
impl Backend for Volume {
    fn role(&self) -> SessionRole {
        match self.journal.geometry().role {
            Role::SingleNodeHome => SessionRole::SingleNodeHome,
            Role::CacheScratch => SessionRole::CacheScratch,
            _ => SessionRole::CheckpointMetadata,
        }
    }
    fn execute(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
        _verifier: &dyn moruna_storage_format::auth::Verifier,
    ) -> Result<Outcome, Refusal> {
        if self.journal.geometry().role == Role::CacheScratch {
            return Err(Refusal::Unsupported);
        }
        if self.failed || self.async_failed.get() {
            return Err(Refusal::Unavailable);
        }
        let mut payload = payload;
        let result = (|| {
            self.object(&request.object)?;
            match &request.operation {
                Operation::CacheRead { .. } | Operation::CacheFill { .. } => {
                    Err(Refusal::Unsupported)
                }
                Operation::Current => Ok(Outcome::Head(
                    self.journal
                        .current(&request.object.key)
                        .ok_or(Refusal::Missing)?
                        .metadata
                        .clone(),
                )),
                Operation::Head { version } => {
                    let commit = self
                        .journal
                        .current(&request.object.key)
                        .ok_or(Refusal::Missing)?;
                    if &commit.metadata.version != version {
                        return Err(Refusal::Conflict);
                    }
                    Ok(Outcome::Head(commit.metadata.clone()))
                }
                Operation::Read {
                    version,
                    offset,
                    length,
                } => self.read(
                    &request.object,
                    version,
                    *offset,
                    *length,
                    payload.as_ref(),
                    account,
                ),
                Operation::Create { length } => self
                    .put(
                        &request.object,
                        &Condition::Absent,
                        payload.take(),
                        *length,
                        account,
                    )
                    .map(Outcome::Written),
                Operation::Replace { condition, length } => self
                    .put(&request.object, condition, payload.take(), *length, account)
                    .map(Outcome::Written),
                Operation::List { limit, after } => {
                    let mut entries = Vec::new();
                    let mut more = false;
                    for commit in self.journal.entries() {
                        let key = &commit.metadata.object.key;
                        if !key.starts_with(&request.object.key)
                            || after.as_ref().is_some_and(|after| key <= after)
                        {
                            continue;
                        }
                        if entries.len() == *limit as usize {
                            more = true;
                            break;
                        }
                        entries.push(commit.metadata.clone());
                    }
                    let next = if more {
                        entries.last().map(|metadata| metadata.object.key.clone())
                    } else {
                        None
                    };
                    Ok(Outcome::Listed { entries, next })
                }
            }
        })();
        if result.is_err() && matches!(request.operation, Operation::Read { .. }) {
            if let Some(destination) = payload.as_ref() {
                let zero = [0; 256];
                let mut at = 0;
                while at < destination.len() {
                    let count = (destination.len() - at).min(zero.len() as u64) as usize;
                    destination
                        .write(at, &zero[..count])
                        .map_err(|_| Refusal::Unavailable)?;
                    at += count as u64;
                }
            }
        }
        if matches!(result, Err(Refusal::Unavailable | Refusal::Integrity)) {
            self.failed = true;
        }
        result
    }
}
