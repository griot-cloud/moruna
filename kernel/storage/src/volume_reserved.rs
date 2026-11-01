//! Privileged physical-owner metadata walk with pre-reserved backing/mappings. No
//! caller-supplied root list or tenant read authority is manufactured by this reader.
use super::async_io::{Transaction, owner};
use super::*;
/// One bounded, preallocated full-object metadata reader. Valid bytes become accessible
/// only after the pinned original blocks and full content hash have been verified.
pub(crate) struct ReservedRead {
    identity: Rc<()>,
    capacity: usize,
    valid: Option<usize>,
    remaining_io: Option<u32>,
    payload: PrivateMemory,
    data: PrivateMemory,
    node: PrivateMemory,
    io: crate::device::ReservedIo,
}
impl ReservedRead {
    /// Set the finite physical read-submission ceiling BEFORE registry freeze.
    pub(crate) fn set_io_budget(&mut self, requests: u32) -> Result<(), Refusal> {
        if requests == 0 || self.valid.is_some() || self.remaining_io.is_some() {
            return Err(Refusal::Invalid);
        }
        self.remaining_io = Some(requests);
        Ok(())
    }
    fn charge_io(&mut self, requests: u32) -> Result<(), Refusal> {
        let remaining = self.remaining_io.as_mut().ok_or(Refusal::Invalid)?;
        *remaining = remaining.checked_sub(requests).ok_or(Refusal::Capacity)?;
        Ok(())
    }
    /// Borrow only the actually verified logical bytes; an error invalidates previous
    /// results, so a failed read cannot accidentally parse an old metadata page.
    pub(crate) fn with_bytes<T>(&mut self, f: impl FnOnce(&[u8]) -> T) -> Result<T, Refusal> {
        let length = self.valid.ok_or(Refusal::Unavailable)?;
        self.payload
            .with_bytes(|bytes| f(&bytes[..length]))
            .map_err(|_| Refusal::Unavailable)
    }
}
impl Volume {
    /// Reserve actual metadata, proof, DMA/control backing and mapping BEFORE freezing
    /// namespace admission. This internal maintenance reader grants no application IO.
    pub(crate) fn prepare_read(
        &self,
        account: &Handle,
        max_metadata_bytes: usize,
    ) -> Result<ReservedRead, Refusal> {
        if !self.admission_open() || max_metadata_bytes == 0 {
            return Err(Refusal::Invalid);
        }
        let a = self.journal.geometry().alignment as usize;
        Ok(ReservedRead {
            identity: self.owner_identity(),
            capacity: max_metadata_bytes,
            valid: None,
            remaining_io: None,
            payload: PrivateMemory::create_in(max_metadata_bytes as u64, account)
                .map_err(|_| Refusal::Capacity)?,
            data: PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?,
            node: PrivateMemory::create_in(a as u64, account).map_err(|_| Refusal::Capacity)?,
            io: crate::device::ReservedIo::new(&self.device, owner(account)?, a)
                .map_err(device_refusal)?,
        })
    }
    /// Read the exact current pinned FULL metadata object through reserved original IO.
    /// Caller is the trusted frozen liveness walker, never an unverified object port.
    pub(crate) async fn read_reserved_async(
        &mut self,
        object: &Object,
        version: &Version,
        reader: &mut ReservedRead,
    ) -> Result<(), Refusal> {
        reader.valid = None;
        if !Rc::ptr_eq(&reader.identity, &self.owner_identity) {
            return Err(Refusal::Denied);
        }
        self.object(object)?;
        let commit = self.journal.current(&object.key).ok_or(Refusal::Missing)?;
        if &commit.metadata.version != version {
            return Err(Refusal::Conflict);
        }
        let bytes = usize::try_from(commit.metadata.bytes).map_err(|_| Refusal::Capacity)?;
        if bytes > reader.capacity {
            return Err(Refusal::Capacity);
        }
        let start = match &commit.location {
            Location::Extent { offset, .. } => *offset,
            _ => return Err(Refusal::Integrity),
        };
        let a = self.journal.geometry().alignment as usize;
        let max_bytes = u64::from(self.device.info().sector_size)
            .checked_mul(u64::from(self.device.info().max_sectors))
            .ok_or(Refusal::Invalid)?;
        if max_bytes == 0 {
            return Err(Refusal::Invalid);
        }
        let requests =
            u32::try_from((a as u64).div_ceil(max_bytes)).map_err(|_| Refusal::Capacity)?;
        let mut transaction = Transaction {
            failed: &self.async_failed,
            complete: false,
        };
        let mut hash = blake3::Hasher::new();
        for block in 0..bytes.div_ceil(a) {
            let mut proof = BlockProof::new(commit, self.journal.geometry(), block as u64)
                .map_err(|_| Refusal::Integrity)?;
            while let Some(step) = proof.next_node() {
                if let Err(cause) = reader.charge_io(requests) {
                    transaction.complete = true;
                    return Err(cause);
                }
                reader
                    .io
                    .read(&self.device, step.offset, &mut reader.node, a)
                    .await
                    .map_err(device_refusal)?;
                reader
                    .node
                    .with_bytes(|data| {
                        proof
                            .accept_node(&data[..a])
                            .map_err(|_| Refusal::Integrity)
                    })
                    .map_err(|_| Refusal::Unavailable)??;
            }
            if let Err(cause) = reader.charge_io(requests) {
                transaction.complete = true;
                return Err(cause);
            }
            reader
                .io
                .read(
                    &self.device,
                    start + block as u64 * a as u64,
                    &mut reader.data,
                    a,
                )
                .await
                .map_err(device_refusal)?;
            reader
                .data
                .with_bytes(|data| {
                    proof
                        .verify_data(&data[..a])
                        .map_err(|_| Refusal::Integrity)?;
                    let count = (bytes - block * a).min(a);
                    hash.update(&data[..count]);
                    reader
                        .payload
                        .with_bytes(|payload| {
                            payload[block * a..block * a + count].copy_from_slice(&data[..count])
                        })
                        .map_err(|_| Refusal::Unavailable)
                })
                .map_err(|_| Refusal::Unavailable)??;
        }
        if hash.finalize().as_bytes() != &commit.content_hash {
            return Err(Refusal::Integrity);
        }
        reader.valid = Some(bytes);
        transaction.complete = true;
        Ok(())
    }
}
