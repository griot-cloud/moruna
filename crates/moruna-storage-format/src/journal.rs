//! The sole portable raw-volume replay/publication authority. IO adapters retain charged
//! memory and exclusive original-operation ownership and supply actual synced bytes.
use crate::{
    Commit, Error, Frame, Location, Metadata, Object, Result, Superblock, frame_size,
    wire::Condition,
};
use alloc::{collections::BTreeMap, string::String};
/// Minimum record-cell bytes, rounded upward to actual device alignment.
pub const RECORD_CELL_BYTES: u64 = 16 * 1024;
/// Pure validated raw journal state, usable by Linux userspace and no_std service alike.
pub struct Journal {
    geometry: Superblock,
    cell: u64,
    log: u64,
    data: u64,
    entries: BTreeMap<String, Commit>,
}
impl Journal {
    /// Validate the exact immutable superblock and establish an empty replay cursor.
    pub fn new(geometry: Superblock) -> Result<Self> {
        geometry.validate()?;
        let alignment = u64::from(geometry.alignment);
        let cell = RECORD_CELL_BYTES
            .checked_add(alignment - 1)
            .ok_or(Error::Limit)?
            / alignment
            * alignment;
        if !geometry
            .log_bytes
            .is_multiple_of(cell.checked_mul(2).ok_or(Error::Limit)?)
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            cell,
            log: geometry.log_offset,
            data: geometry.data_offset,
            geometry,
            entries: BTreeMap::new(),
        })
    }
    /// Validated common geometry; adapter additionally compares actual device capacity/role.
    pub fn geometry(&self) -> &Superblock {
        &self.geometry
    }
    /// Aligned bytes for each commit cell and its separate receipt cell.
    pub fn cell_bytes(&self) -> u64 {
        self.cell
    }
    /// Next commit position; this exact value is the record's monotonic sequence.
    pub fn sequence(&self) -> u64 {
        self.log
    }
    /// Next contiguous extent start, never reused before exclusive IO drain and replay.
    pub fn data_end(&self) -> u64 {
        self.data
    }
    /// True only if another complete commit+receipt pair fits in the explicit bounded log.
    pub fn has_capacity(&self) -> bool {
        self.log
            .checked_add(2 * self.cell)
            .is_some_and(|end| end <= self.geometry.data_offset)
    }
    /// Exact current committed facts; old immutable extents remain retained until an explicit
    /// future reclaim policy is implemented. No implicit retention period is promised.
    pub fn current(&self, key: &str) -> Option<&Commit> {
        self.entries.get(key)
    }
    /// Current committed records in exact key order, not a read authorization grant.
    pub fn entries(&self) -> impl Iterator<Item = &Commit> {
        self.entries.values()
    }
    /// Apply a live atomic condition while the caller owns exclusive transaction admission.
    pub fn condition(&self, object: &Object, condition: &Condition) -> Result<bool> {
        self.object(object)?;
        Ok(match (condition, self.current(&object.key)) {
            (Condition::Absent, None) => true,
            (Condition::Match(expected), Some(current)) => {
                expected.validate()?;
                *expected == current.metadata.version
            }
            (Condition::Match(expected), None) => {
                expected.validate()?;
                false
            }
            _ => false,
        })
    }
    /// Plan one immutable contiguous payload/tree without changing visibility.
    pub fn plan(
        &self,
        object: &Object,
        condition: &Condition,
        bytes: u64,
    ) -> Result<Option<Location>> {
        if !self.condition(object, condition)? {
            return Ok(None);
        }
        if !self.has_capacity() {
            return Err(Error::Limit);
        }
        Ok(Some(self.geometry.extent(self.data, bytes)?))
    }
    fn object(&self, object: &Object) -> Result<()> {
        object.validate()?;
        if object.key.is_empty()
            || object.workspace != self.geometry.workspace
            || object.bucket != self.geometry.bucket
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Validate and apply one actual receipt-backed pair. A zero receipt returns false and
    /// changes nothing (unpublished tail). Nonzero corruption, gaps/overlap and a mismatched
    /// receipt are refused, never silently skipped. Padding must be all zero under Frame.
    pub fn replay(&mut self, record: &[u8], receipt: &[u8]) -> Result<bool> {
        if !self.has_capacity()
            || record.len() as u64 != self.cell
            || receipt.len() as u64 != self.cell
        {
            return Err(Error::Invalid);
        }
        if receipt.iter().all(|b| *b == 0) {
            return Ok(false);
        }
        let hash = match Frame::decode(receipt)? {
            Frame::Receipt {
                sequence,
                commit_hash,
            } if sequence == self.log => commit_hash,
            _ => return Err(Error::Integrity),
        };
        let length = frame_size(record)?;
        if length > record.len() || blake3::hash(&record[..length]).as_bytes() != &hash {
            return Err(Error::Integrity);
        }
        let commit = match Frame::decode(record)? {
            Frame::Commit(commit) => commit,
            _ => return Err(Error::Integrity),
        };
        self.validate_commit(&commit)?;
        let end = self.extent_end(&commit.location)?;
        self.entries
            .insert(commit.metadata.object.key.clone(), commit);
        self.log = self.log.checked_add(2 * self.cell).ok_or(Error::Limit)?;
        self.data = end;
        Ok(true)
    }
    /// Common publication guard before any commit/receipt writes. IO adapter owns actual
    /// ordering: complete payload/tree sync, complete commit sync, then receipt sync.
    pub fn validate_commit(&self, commit: &Commit) -> Result<()> {
        if !self.has_capacity() || commit.sequence != self.log {
            return Err(Error::Invalid);
        }
        commit.validate_in(&self.geometry)?;
        if !matches!(commit.location,Location::Extent{offset,..} if offset==self.data) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Exact immutable extent end under shared geometry/tree validation.
    pub fn extent_end(&self, location: &Location) -> Result<u64> {
        match location {
            Location::Extent {
                offset,
                padded_bytes,
                tree,
                ..
            } => {
                if let Some(last) = tree.last() {
                    last.nodes
                        .checked_mul(u64::from(self.geometry.alignment))
                        .and_then(|bytes| last.offset.checked_add(bytes))
                        .ok_or(Error::Limit)
                } else {
                    offset.checked_add(*padded_bytes).ok_or(Error::Limit)
                }
            }
            _ => Err(Error::Invalid),
        }
    }
    /// Consume the portable index when an adapter needs owned metadata for bounded listing.
    pub fn into_metadata(self) -> impl Iterator<Item = Metadata> {
        self.entries.into_values().map(|c| c.metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, Version};
    use alloc::{string::ToString, vec};
    fn journal() -> Journal {
        Journal::new(Superblock {
            capacity: 1 << 20,
            alignment: 4096,
            log_offset: 4096,
            log_bytes: 4 * 32768,
            data_offset: 4096 + 4 * 32768,
            role: Role::SingleNodeHome,
            workspace: "owner".to_string(),
            bucket: "home".to_string(),
        })
        .unwrap()
    }
    fn object() -> Object {
        Object {
            workspace: "owner".to_string(),
            bucket: "home".to_string(),
            key: "column".to_string(),
        }
    }
    fn commit(journal: &Journal) -> Commit {
        let mut location = journal
            .plan(&object(), &Condition::Absent, 13)
            .unwrap()
            .unwrap();
        if let Location::Extent { root, .. } = &mut location {
            *root = [7; 32];
        }
        Commit {
            sequence: journal.sequence(),
            metadata: Metadata {
                object: object(),
                version: Version {
                    etag: Some("etag".to_string()),
                    generation: Some("v1".to_string()),
                },
                bytes: 13,
            },
            content_hash: *blake3::hash(b"13-byte-value").as_bytes(),
            location,
        }
    }
    fn cells(journal: &Journal, commit: Commit) -> (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) {
        let mut record = Frame::Commit(commit).encode().unwrap();
        let mut receipt = Frame::Receipt {
            sequence: journal.sequence(),
            commit_hash: *blake3::hash(&record).as_bytes(),
        }
        .encode()
        .unwrap();
        record.resize(journal.cell_bytes() as usize, 0);
        receipt.resize(journal.cell_bytes() as usize, 0);
        (record, receipt)
    }
    #[test]
    fn journal_receipt_is_required_and_only_validated_pair_advances_visibility() {
        let mut journal = journal();
        let start = journal.sequence();
        let record = commit(&journal);
        let end = journal.extent_end(&record.location).unwrap();
        let (data, receipt) = cells(&journal, record.clone());
        assert!(!journal.replay(&data, &vec![0; receipt.len()]).unwrap());
        assert_eq!(journal.sequence(), start);
        assert!(journal.current("column").is_none());
        let mut corrupt = data.clone();
        corrupt[70] ^= 1;
        assert!(journal.replay(&corrupt, &receipt).is_err());
        assert_eq!(journal.sequence(), start);
        assert!(journal.replay(&data, &receipt).unwrap());
        assert_eq!(journal.data_end(), end);
        assert_eq!(journal.sequence(), start + 2 * journal.cell_bytes());
        assert_eq!(journal.current("column").unwrap(), &record);
        assert!(!journal.condition(&object(), &Condition::Absent).unwrap());
        assert!(
            journal
                .condition(
                    &object(),
                    &Condition::Match(record.metadata.version.clone())
                )
                .unwrap()
        );
        assert!(
            !journal
                .condition(
                    &object(),
                    &Condition::Match(Version {
                        etag: Some("stale".to_string()),
                        generation: None
                    })
                )
                .unwrap()
        );
        assert!(journal.replay(&data, &receipt).is_err()); // stale sequence cannot replay twice
    }
    #[test]
    fn journal_rejects_extent_reuse_gaps_and_changed_record_identity() {
        let mut journal = journal();
        let record = commit(&journal);
        let (data, receipt) = cells(&journal, record.clone());
        journal.replay(&data, &receipt).unwrap();
        let mut overlap = record.clone();
        overlap.sequence = journal.sequence();
        assert!(journal.validate_commit(&overlap).is_err());
        let target = Object {
            key: "next".to_string(),
            ..object()
        };
        let mut next = record;
        next.sequence = journal.sequence();
        next.metadata.object = target.clone();
        next.location = journal
            .plan(&target, &Condition::Absent, 13)
            .unwrap()
            .unwrap();
        if let Location::Extent { offset, .. } = &mut next.location {
            *offset += 4096;
        }
        assert!(journal.validate_commit(&next).is_err());
        let mut wrong_owner = object();
        wrong_owner.workspace = "other".to_string();
        assert!(journal.plan(&wrong_owner, &Condition::Absent, 0).is_err());
    }
}
