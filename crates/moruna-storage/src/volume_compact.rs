//! Real bounded bank relocation under the same exclusive original-IO transaction.
use super::*;
impl Volume {
    /// Reclaim obsolete versions while retaining EVERY currently published object.
    /// This maintenance capability is not an application authorization grant. The owner
    /// must separately authorize invoking it, just as explicit provisioning. It never
    /// takes a client-provided liveness list or prunes namespace-referenced objects.
    pub async fn compact(&self, scope: &Scope) -> Result<Superblock> {
        let _admission = self.admission.lock().await;
        let arena = self.arena(scope)?;
        let tx = Arc::clone(&self.blocks).begin()?;
        let replay = self.replay(tx.as_ref(), arena.as_ref(), scope).await?;
        let root = replay.root.next()?;
        let mut journal = Journal::new(root.geometry().clone())?;
        let a = self.blocks.alignment();
        let cell = journal.cell_bytes() as usize;
        let _charge = self.staging.reserve(scope, root.geometry().log_bytes)?;
        // Full capacity/metadata preflight precedes the first inactive-bank write.
        let mut data_end = journal.data_end();
        let mut count = 0u64;
        for entry in replay.journal.entries() {
            let location = root.geometry().extent(data_end, entry.metadata.bytes)?;
            data_end = journal.extent_end(&location)?;
            count = count
                .checked_add(1)
                .ok_or(Error::Invalid("compaction count"))?;
        }
        if count
            .checked_mul(2 * journal.cell_bytes())
            .is_none_or(|n| n > root.geometry().log_bytes)
        {
            return Err(Error::Invalid("compaction journal capacity"));
        }
        let mut scratch = self.buffer(arena.as_ref(), a)?;
        let mut node = self.buffer(arena.as_ref(), a)?;
        let mut record_buffer = self.buffer(arena.as_ref(), cell)?;
        let mut receipt_buffer = self.buffer(arena.as_ref(), cell)?;
        scratch.as_mut().fill(0);
        // An old valid root must never authorize a bank after reuse has begun.
        let retired = Arc::new(scratch);
        tx.write(root.slot(), retired.view()).await?;
        tx.sync().await?;
        scratch = Arc::try_unwrap(retired)
            .map_err(|_| Error::Unavailable("retired root buffer retained"))?;
        for at in (root.geometry().log_offset..root.geometry().data_offset).step_by(a) {
            let shared = Arc::new(scratch);
            tx.write(at, shared.view()).await?;
            scratch = Arc::try_unwrap(shared)
                .map_err(|_| Error::Unavailable("compaction zero buffer retained"))?;
        }
        tx.sync().await?;
        for old in replay.journal.entries() {
            let mut location = journal
                .plan(
                    &old.metadata.object,
                    &wire::Condition::Absent,
                    old.metadata.bytes,
                )?
                .ok_or(Error::Integrity)?;
            let (old_offset, padded, old_tree, digest) = match &old.location {
                Location::Extent {
                    offset,
                    padded_bytes,
                    tree,
                    root,
                } => (*offset, *padded_bytes, tree, *root),
                _ => return Err(Error::Integrity),
            };
            let (new_offset, new_tree) = match &mut location {
                Location::Extent {
                    offset, tree, root, ..
                } => {
                    *root = digest;
                    (*offset, tree)
                }
                _ => return Err(Error::Integrity),
            };
            for block in 0..padded / a as u64 {
                let mut proof = BlockProof::new(old, replay.journal.geometry(), block)?;
                while let Some(step) = proof.next_node() {
                    node = tx.read(step.offset, node).await?;
                    proof.accept_node(node.as_ref())?;
                }
                scratch = tx.read(old_offset + block * a as u64, scratch).await?;
                proof.verify_data(scratch.as_ref())?;
                let shared = Arc::new(scratch);
                tx.write(new_offset + block * a as u64, shared.view())
                    .await?;
                scratch = Arc::try_unwrap(shared)
                    .map_err(|_| Error::Unavailable("compaction data buffer retained"))?;
            }
            if old_tree.len() != new_tree.len() {
                return Err(Error::Integrity);
            }
            for (old_level, new_level) in old_tree.iter().zip(new_tree.iter()) {
                if old_level.nodes != new_level.nodes {
                    return Err(Error::Integrity);
                }
                for page in 0..old_level.nodes {
                    scratch = tx.read(old_level.offset + page * a as u64, scratch).await?;
                    let shared = Arc::new(scratch);
                    tx.write(new_level.offset + page * a as u64, shared.view())
                        .await?;
                    scratch = Arc::try_unwrap(shared)
                        .map_err(|_| Error::Unavailable("compaction tree buffer retained"))?;
                }
            }
            let commit = Commit {
                sequence: journal.sequence(),
                metadata: old.metadata.clone(),
                content_hash: old.content_hash,
                location,
            };
            journal.validate_commit(&commit)?;
            let record = Frame::Commit(commit).encode()?;
            let receipt = Frame::Receipt {
                sequence: journal.sequence(),
                commit_hash: *blake3::hash(&record).as_bytes(),
            }
            .encode()?;
            if record.len() > cell || receipt.len() > cell {
                return Err(Error::Invalid("compaction metadata cell"));
            }
            record_buffer.as_mut().fill(0);
            record_buffer.as_mut()[..record.len()].copy_from_slice(&record);
            receipt_buffer.as_mut().fill(0);
            receipt_buffer.as_mut()[..receipt.len()].copy_from_slice(&receipt);
            tx.sync().await?;
            let record_buffer_shared = Arc::new(record_buffer);
            tx.write(journal.sequence(), record_buffer_shared.view())
                .await?;
            tx.sync().await?;
            let receipt_buffer_shared = Arc::new(receipt_buffer);
            tx.write(
                journal.sequence() + cell as u64,
                receipt_buffer_shared.view(),
            )
            .await?;
            tx.sync().await?;
            if !journal.replay(
                record_buffer_shared.as_ref().as_ref(),
                receipt_buffer_shared.as_ref().as_ref(),
            )? {
                return Err(Error::Integrity);
            }
            record_buffer = Arc::try_unwrap(record_buffer_shared)
                .map_err(|_| Error::Unavailable("compaction record buffer retained"))?;
            receipt_buffer = Arc::try_unwrap(receipt_buffer_shared)
                .map_err(|_| Error::Unavailable("compaction receipt buffer retained"))?;
        }
        let encoded = root.encode()?;
        scratch.as_mut().fill(0);
        scratch.as_mut()[..encoded.len()].copy_from_slice(&encoded);
        let published = Arc::new(scratch);
        tx.write(root.slot(), published.view()).await?;
        tx.sync().await?;
        let mut scratch = Arc::try_unwrap(published)
            .map_err(|_| Error::Unavailable("published root buffer retained"))?;
        scratch.as_mut().fill(0);
        // Do not admit later writes while an older root could roll them back if the
        // newest cell were lost. A failed/torn retirement is explicit uncertainty.
        tx.write(replay.root.slot(), Arc::new(scratch).view())
            .await?;
        tx.sync().await?;
        Ok(root.geometry().clone())
    }
}
