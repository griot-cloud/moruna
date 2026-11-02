//! Disk-format major 3: alternating durable roots and two bounded copy banks.
//! This envelope is the sole host/native root codec; old single-superblock media is
//! deliberately not accepted. Inner Frame/Journal bytes keep one existing authority.
use crate::{Error, Frame, Result, Role, Superblock};
use alloc::{string::String, vec::Vec};
const MAGIC: &[u8; 8] = b"MORUBNK3";
const HEADER: usize = 68;
/// Raw-media format major, independent of object transport or artifact encoding.
pub const VERSION: u32 = 3;
/// Validated active-bank root. Fields are private so arbitrary wire geometry cannot
/// bypass the physical-capacity, reserve or generation checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Root {
    physical: u64,
    generation: u64,
    bank: u8,
    geometry: Superblock,
}
/// Validated recovery decision. When both roots are valid the older one must be
/// durably retired before any adapter admits later acknowledged mutation.
pub struct Recovery {
    root: Root,
    retire: Option<u64>,
}
impl Recovery {
    /// Highest validated compatible generation; no invalid nonzero cell was ignored.
    pub fn root(&self) -> &Root {
        &self.root
    }
    /// Older root cell requiring zero+sync after replay validation and before serving.
    pub fn retirement(&self) -> Option<u64> {
        self.retire
    }
}
impl Root {
    /// Plan first provisioning. Both root cells must separately be proved zero by IO.
    /// Exactly half of the aligned post-root capacity is reserved for the other bank.
    pub fn initial(
        physical: u64,
        alignment: u32,
        log_bytes: u64,
        role: Role,
        workspace: String,
        bucket: String,
    ) -> Result<Self> {
        let a = u64::from(alignment);
        if a < 512 || !a.is_power_of_two() || !physical.is_multiple_of(a) {
            return Err(Error::Invalid);
        }
        let start = a.checked_mul(2).ok_or(Error::Limit)?;
        let bank_bytes = physical.checked_sub(start).ok_or(Error::Invalid)? / 2 / a * a;
        let end = start.checked_add(bank_bytes).ok_or(Error::Limit)?;
        let root = Self {
            physical,
            generation: 1,
            bank: 0,
            geometry: Superblock {
                capacity: end,
                alignment,
                log_offset: start,
                log_bytes,
                data_offset: start.checked_add(log_bytes).ok_or(Error::Limit)?,
                role,
                workspace,
                bucket,
            },
        };
        root.validate()?;
        Ok(root)
    }
    /// Exact actual backing capacity, including the copy reserve and both root cells.
    pub fn physical_capacity(&self) -> u64 {
        self.physical
    }
    /// Monotonically increasing durable root generation; never wraps.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Active bank (zero or one), selected only by a durable root.
    pub fn bank(&self) -> u8 {
        self.bank
    }
    /// Existing shared Journal geometry bounded to the selected bank end.
    pub fn geometry(&self) -> &Superblock {
        &self.geometry
    }
    /// Exact alternating root-cell byte offset for this generation.
    pub fn slot(&self) -> u64 {
        (self.generation - 1) % 2 * u64::from(self.geometry.alignment)
    }
    /// Actual payload bytes available after this bank's bounded journal. The other bank
    /// is not usable capacity and cannot be borrowed when this one becomes full.
    pub fn payload_capacity(&self) -> u64 {
        self.geometry.capacity - self.geometry.data_offset
    }
    /// Plan an inactive bank without changing authority. Caller must pre-reserve copy
    /// resources, freeze internal liveness and prove original IO/pin drain before reuse.
    pub fn next(&self) -> Result<Self> {
        self.validate()?;
        let generation = self.generation.checked_add(1).ok_or(Error::Limit)?;
        let mut next = Self::initial(
            self.physical,
            self.geometry.alignment,
            self.geometry.log_bytes,
            self.geometry.role,
            self.geometry.workspace.clone(),
            self.geometry.bucket.clone(),
        )?;
        next.generation = generation;
        next.bank = 1 - self.bank;
        let a = u64::from(self.geometry.alignment);
        let bank_bytes = (self.physical - 2 * a) / 2 / a * a;
        let start = 2 * a + u64::from(next.bank) * bank_bytes;
        next.geometry.log_offset = start;
        next.geometry.data_offset = start
            .checked_add(next.geometry.log_bytes)
            .ok_or(Error::Limit)?;
        next.geometry.capacity = start.checked_add(bank_bytes).ok_or(Error::Limit)?;
        next.validate()?;
        Ok(next)
    }
    /// Validate all capacity, alignment, reserve, bank and generation facts. No adapter
    /// may reinterpret capacity as total media when checking extent bounds.
    pub fn validate(&self) -> Result<()> {
        self.geometry.validate()?;
        let a = u64::from(self.geometry.alignment);
        if self.generation == 0
            || self.bank > 1
            || self.bank != ((self.generation - 1) % 2) as u8
            || !self.physical.is_multiple_of(a)
        {
            return Err(Error::Invalid);
        }
        let bank_bytes = self.physical.checked_sub(2 * a).ok_or(Error::Invalid)? / 2 / a * a;
        let start = 2 * a + u64::from(self.bank) * bank_bytes;
        if self.geometry.log_offset != start
            || self.geometry.capacity != start.checked_add(bank_bytes).ok_or(Error::Limit)?
            || self.geometry.data_offset >= self.geometry.capacity
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Encode one checksummed root; IO zero-pads the remainder of its alignment cell.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let inner = Frame::Superblock(self.geometry.clone()).encode()?;
        let length = HEADER.checked_add(inner.len()).ok_or(Error::Limit)?;
        if length > self.geometry.alignment as usize {
            return Err(Error::Limit);
        }
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        bytes.extend_from_slice(&self.physical.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.push(self.bank);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&inner);
        let mut hash = blake3::Hasher::new();
        hash.update(&bytes[..36]);
        hash.update(&bytes[HEADER..]);
        bytes[36..HEADER].copy_from_slice(hash.finalize().as_bytes());
        Ok(bytes)
    }
    /// Decode one exact or zero-padded root cell. Unknown major, nonzero padding,
    /// corrupt checksum and geometry disagreement are explicit refusals.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER {
            return Err(Error::Invalid);
        }
        if &bytes[..8] != MAGIC {
            return Err(if &bytes[..7] == b"MORUBNK" {
                Error::Version
            } else {
                Error::Invalid
            });
        }
        let u32_at = |at: usize| u32::from_le_bytes(core::array::from_fn(|i| bytes[at + i]));
        let u64_at = |at: usize| u64::from_le_bytes(core::array::from_fn(|i| bytes[at + i]));
        if u32_at(8) != VERSION {
            return Err(Error::Version);
        }
        let length = u32_at(12) as usize;
        if length < HEADER
            || length > bytes.len()
            || bytes[33..36] != [0; 3]
            || bytes[length..].iter().any(|b| *b != 0)
        {
            return Err(Error::Invalid);
        }
        let mut hash = blake3::Hasher::new();
        hash.update(&bytes[..36]);
        hash.update(&bytes[HEADER..length]);
        if hash.finalize().as_bytes() != &bytes[36..HEADER] {
            return Err(Error::Integrity);
        }
        let geometry = match Frame::decode(&bytes[HEADER..length])? {
            Frame::Superblock(s) => s,
            _ => return Err(Error::Invalid),
        };
        let root = Self {
            physical: u64_at(16),
            generation: u64_at(24),
            bank: bytes[32],
            geometry,
        };
        root.validate()?;
        if bytes.len() > root.geometry.alignment as usize {
            return Err(Error::Invalid);
        }
        Ok(root)
    }
    /// Recover the two actual padded root cells. Only an entirely zero cell is absent.
    /// Invalid nonzero bytes are ambiguous authority/corruption, never a fallback hint.
    /// Adapters retire the previous root durably before admitting post-switch writes.
    pub fn recover(first: &[u8], second: &[u8]) -> Result<Recovery> {
        if first.is_empty() || first.len() != second.len() {
            return Err(Error::Integrity);
        }
        fn cell(bytes: &[u8], slot: u64) -> Result<Option<Root>> {
            if bytes.is_empty() {
                return Err(Error::Integrity);
            }
            if bytes.iter().all(|byte| *byte == 0) {
                return Ok(None);
            }
            let root = Root::decode(bytes).map_err(|_| Error::Integrity)?;
            if bytes.len() != root.geometry.alignment as usize
                || root.slot() != slot * u64::from(root.geometry.alignment)
            {
                return Err(Error::Integrity);
            }
            Ok(Some(root))
        }
        let first = cell(first, 0)?;
        let second = cell(second, 1)?;
        let retire = match (&first, &second) {
            (Some(a), Some(b)) => Some(if a.generation < b.generation {
                a.slot()
            } else {
                b.slot()
            }),
            _ => None,
        };
        let root = Self::select(first, second)?;
        Ok(Recovery { root, retire })
    }
    /// Select actual independently decoded root cells. Invalid nonzero cells are reported
    /// by recover; adapters must not turn decoding failures into absent cells.
    fn select(first: Option<Self>, second: Option<Self>) -> Result<Self> {
        match (first, second) {
            (Some(a), Some(b)) => {
                if a.physical != b.physical
                    || a.geometry.alignment != b.geometry.alignment
                    || a.geometry.log_bytes != b.geometry.log_bytes
                    || a.geometry.workspace != b.geometry.workspace
                    || a.geometry.bucket != b.geometry.bucket
                    || a.geometry.role != b.geometry.role
                    || a.generation.abs_diff(b.generation) != 1
                {
                    return Err(Error::Integrity);
                }
                Ok(if a.generation > b.generation { a } else { b })
            }
            (Some(a), None) | (None, Some(a)) => Ok(a),
            (None, None) => Err(Error::Integrity),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    fn root() -> Root {
        Root::initial(
            8 << 20,
            4096,
            32768 * 4,
            Role::SingleNodeHome,
            "owner".to_string(),
            "home".to_string(),
        )
        .unwrap()
    }
    #[test]
    fn banks_are_disjoint_bounded_and_roots_alternate() {
        let a = root();
        let b = a.next().unwrap();
        let c = b.next().unwrap();
        assert_eq!(a.slot(), 0);
        assert_eq!(b.slot(), 4096);
        assert_eq!(c.slot(), 0);
        assert_eq!(a.geometry.capacity, b.geometry.log_offset);
        assert_eq!(a.payload_capacity(), b.payload_capacity());
        assert_eq!(Root::select(Some(a), Some(b.clone())).unwrap(), b);
    }
    #[test]
    fn corrupt_roots_unknown_major_and_nonzero_padding_refuse() {
        let root = root();
        let mut encoded = root.encode().unwrap();
        assert_eq!(Root::decode(&encoded).unwrap(), root);
        encoded[16] ^= 1;
        assert!(Root::decode(&encoded).is_err());
        let mut encoded = root.encode().unwrap();
        encoded[8] = 1;
        assert!(Root::decode(&encoded).is_err());
        let mut encoded = root.encode().unwrap();
        encoded.push(1);
        assert!(Root::decode(&encoded).is_err());
    }
    #[test]
    fn recovery_distinguishes_zero_retirement_from_ambiguous_nonzero_corruption() {
        let first = root();
        let second = first.next().unwrap();
        let mut a = first.encode().unwrap();
        a.resize(4096, 0);
        let mut b = second.encode().unwrap();
        b.resize(4096, 0);
        let recovery = Root::recover(&a, &b).unwrap();
        assert_eq!(recovery.root(), &second);
        assert_eq!(recovery.retirement(), Some(first.slot()));
        a.fill(0);
        let recovery = Root::recover(&a, &b).unwrap();
        assert_eq!(recovery.root(), &second);
        assert_eq!(recovery.retirement(), None);
        a[17] = 1;
        assert!(Root::recover(&a, &b).is_err());
        a.fill(0);
        b.fill(0);
        assert!(Root::recover(&a, &b).is_err());
    }
    #[test]
    fn old_v2_media_is_explicitly_refused_not_upgraded() {
        let mut old = root().encode().unwrap();
        old[..8].copy_from_slice(b"MORUBNK2");
        old[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(Root::decode(&old), Err(Error::Version)));
        let zero = [0; 4096];
        assert!(Root::recover(&old, &zero).is_err());
    }
    #[test]
    fn overflow_and_cross_media_roots_refuse() {
        let mut a = root();
        a.generation = u64::MAX;
        a.bank = 0;
        assert!(a.next().is_err());
        let a = root();
        let mut b = a.next().unwrap();
        b.physical += 4096;
        assert!(Root::select(Some(a), Some(b)).is_err());
    }
}
