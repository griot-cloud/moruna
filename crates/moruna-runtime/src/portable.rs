//! Engine checkpoint dependency inventory and bounded materialization.
//! The sole KS1 checkpoint publisher owns authorization, immutable versions and portability
//! acknowledgement. This module only translates actual placement state and segment bytes.
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use moruna_kernel::{AllocStats, Allocator, Buffer, MorunaError, RunId, Tier};
use moruna_placement::manifest::{DiskRef, Manifest, SegmentRow};
use moruna_storage::{
    Access, Condition, Metadata, Object,
    checkpoint::{Checkpoints, Portable},
};
use serde::{Deserialize, Serialize};

const FIELD: &str = "moruna.portable_staging";
const ENGINE_VERSION: u32 = 1;
pub(crate) const HISTORY_FIELD: &str = "moruna.historical_rows";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Piece {
    offset: u64,
    bytes: u64,
    dependency: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    row: SegmentRow,
    pieces: Vec<Piece>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    row: usize,
    disk: DiskRef,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Staging {
    version: u32,
    segments: Vec<Segment>,
    positions: Vec<Position>,
}

fn invalid(reason: &'static str) -> moruna_storage::Error {
    moruna_storage::Error::Invalid(reason)
}
fn local_io(_: std::io::Error) -> moruna_storage::Error {
    moruna_storage::Error::Unavailable("checkpoint local state IO")
}
fn object_under(root: &Object, suffix: &str) -> Object {
    Object {
        key: format!("{}.dependencies/{suffix}", root.key),
        ..root.clone()
    }
}
fn open_read(path: &Path) -> moruna_storage::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(local_io)
}

/// One already charged arena reservation. Allocation only splits that reservation and
/// never asks the live run's allocator for more memory while its workers are paused.
struct ReservedAllocator {
    source: Arc<dyn Allocator>,
    pool: Mutex<Option<Buffer>>,
}
impl Allocator for ReservedAllocator {
    fn alloc(&self, bytes: usize, tier: Tier) -> moruna_kernel::Result<Buffer> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let available = pool.as_ref().map_or(0, Buffer::len);
        let page = self.page_bytes();
        let rounded = bytes.checked_add(page - 1).map(|n| n / page * page);
        let Some(rounded) = rounded.filter(|n| *n <= available) else {
            return Err(MorunaError::Alloc {
                bytes: bytes as u64,
                tier,
                budget: available as u64,
                in_use: 0,
            });
        };
        let current = pool
            .take()
            .ok_or_else(|| MorunaError::Resume("checkpoint reservation exhausted".into()))?;
        if current.tier() != tier {
            *pool = Some(current);
            return Err(MorunaError::Resume(
                "checkpoint reservation tier mismatch".into(),
            ));
        }
        let (region, remaining) = current.split_at(rounded);
        *pool = Some(remaining);
        let (result, padding) = region.split_at(bytes);
        drop(padding);
        Ok(result)
    }
    fn page_bytes(&self) -> usize {
        self.source.page_bytes()
    }
    fn stats(&self) -> AllocStats {
        self.source.stats()
    }
    fn contains(&self, ptr: *const u8) -> bool {
        self.source.contains(ptr)
    }
    fn tier_of(&self, ptr: *const u8) -> Option<Tier> {
        self.source.tier_of(ptr)
    }
    fn is_pinned(&self) -> bool {
        self.source.is_pinned()
    }
    fn note_payload_copy(&self, bytes: u64) {
        self.source.note_payload_copy(bytes);
    }
    fn note_boundary_copy(&self, bytes: u64) {
        self.source.note_boundary_copy(bytes);
    }
}

/// Real buffers acquired before a scheduler checkpoint barrier. Consumed once; no hidden
/// fallback allocation or assumption of available physical bucket capacity.
pub struct Prepared {
    history: Option<Vec<Metadata>>,
    encoding: ReservedAllocator,
    chunk: Option<Buffer>,
    metadata: Buffer,
}

/// Charged engine adapter around the one governed checkpoint authority.
pub struct EngineCheckpoints {
    authority: Arc<Checkpoints>,
    alloc: Arc<dyn Allocator>,
    chunk_bytes: usize,
    metadata_bytes: usize,
}
impl EngineCheckpoints {
    /// Explicit per-piece and metadata admission; no unbounded whole-shard allocation.
    pub fn new(
        authority: Arc<Checkpoints>,
        alloc: Arc<dyn Allocator>,
        chunk_bytes: usize,
        metadata_bytes: usize,
    ) -> moruna_storage::Result<Self> {
        if chunk_bytes == 0 || metadata_bytes == 0 {
            return Err(invalid("checkpoint transfer budgets"));
        }
        Ok(Self {
            authority,
            alloc,
            chunk_bytes,
            metadata_bytes,
        })
    }
    /// Reserve actual host IO, input metadata, and codec working-set bytes before pausing.
    /// `encoding_bytes` is a ceiling, not a prediction of the Arrow framing size.
    pub fn prepare(&self, encoding_bytes: usize) -> moruna_storage::Result<Prepared> {
        if encoding_bytes == 0 || self.alloc.page_bytes() == 0 {
            return Err(invalid("checkpoint encoding admission"));
        }
        let tier = if self.alloc.is_pinned() {
            Tier::PinnedHost
        } else {
            Tier::Host
        };
        let allocate = |bytes| {
            self.alloc
                .alloc(bytes, tier)
                .map_err(|_| invalid("checkpoint charged admission"))
        };
        Ok(Prepared {
            history: None,
            encoding: ReservedAllocator {
                source: self.alloc.clone(),
                pool: Mutex::new(Some(allocate(encoding_bytes)?)),
            },
            chunk: Some(allocate(self.chunk_bytes)?),
            metadata: allocate(self.metadata_bytes)?,
        })
    }

    /// Publish actual immutable segment chunks and inline kernel/sink state. Return only the
    /// bucket acknowledgement; a local-only publication remains typed PortablePending.
    #[expect(
        clippy::too_many_arguments,
        reason = "Public checkpoint publication explicitly binds authority, immutable targets, CAS condition and pre-admitted buffers; preserve its integration contract"
    )]
    pub async fn publish(
        &self,
        access: &Access,
        manifest_path: &Path,
        sequence: u64,
        local_target: &Object,
        bucket_target: &Object,
        condition: Condition,
        prepared: &mut Prepared,
        engine: Option<&moruna_placement::PlacementEngine>,
    ) -> moruna_storage::Result<Metadata> {
        let mut file = open_read(manifest_path)?;
        if file.metadata().map_err(local_io)?.len() > self.metadata_bytes as u64 {
            return Err(invalid("engine checkpoint metadata ceiling"));
        }
        let length = usize::try_from(file.metadata().map_err(local_io)?.len())
            .map_err(|_| invalid("checkpoint metadata length"))?;
        if length > prepared.metadata.len() || length > self.metadata_bytes {
            return Err(invalid("checkpoint metadata admission"));
        }
        file.read_exact(&mut prepared.metadata.as_mut()[..length])
            .map_err(local_io)?;
        let mut manifest: Manifest = serde_json::from_slice(&prepared.metadata.as_ref()[..length])
            .map_err(|_| invalid("engine checkpoint manifest"))?;
        if manifest.version != moruna_placement::manifest::MANIFEST_VERSION
            || manifest.run_id != access.run
            || RunId::from_hex(&manifest.run_id).is_none()
            || !manifest.issued.is_empty()
        {
            return Err(invalid("engine checkpoint identity/version"));
        }
        let directory = manifest_path
            .parent()
            .ok_or_else(|| invalid("engine checkpoint directory"))?;
        let mut positions = Vec::new();
        let mut referenced = HashSet::new();
        let mut resident = Vec::new();
        let mut next_segment = manifest
            .segments
            .iter()
            .map(|row| row.segment)
            .max()
            .unwrap_or(0);
        for (row, lineage) in manifest.lineage.iter_mut().enumerate() {
            let disk = match lineage.disk.take() {
                Some(disk) => {
                    referenced.insert(disk.segment);
                    disk
                }
                None => {
                    let engine = engine.ok_or_else(|| {
                        invalid("portable checkpoint lacks resident snapshot authority")
                    })?;
                    let payload = engine.checkpoint_payload(lineage).map_err(|cause| {
                        moruna_storage::Error::Io {
                            operation: "pin portable checkpoint payload",
                            source: std::io::Error::other(format!(
                                "stage={} seq={} source={} root={} origin={}:{}:{}..{} kind={} bytes={}: {cause:?}",
                                lineage.stage, lineage.seq, lineage.source, lineage.root,
                                lineage.node, lineage.split, lineage.row_start, lineage.row_end,
                                lineage.kind, lineage.bytes,
                            )),
                        }
                    })?;
                    next_segment = next_segment.checked_add(1).ok_or_else(|| {
                        invalid("checkpoint synthetic segment namespace exhausted")
                    })?;
                    let record = moruna_placement::portable_record::encode(
                        &payload,
                        lineage.stage,
                        lineage.seq,
                        next_segment,
                        &prepared.encoding,
                        self.alloc.page_bytes() as u64,
                    )
                    .map_err(|_| {
                        invalid(
                            "checkpoint resident encoding exceeded admission or lacks host bytes",
                        )
                    })?;
                    let disk = record.disk().clone();
                    resident.push((lineage.stage, next_segment, record));
                    disk
                }
            };
            disk.offset
                .checked_add(disk.len)
                .ok_or_else(|| invalid("checkpoint disk range overflow"))?;
            positions.push(Position { row, disk });
        }
        let rows = std::mem::take(&mut manifest.segments);
        let mut ids = HashSet::new();
        for row in &rows {
            if !ids.insert(row.segment) || !referenced.contains(&row.segment) {
                return Err(invalid("checkpoint segment inventory"));
            }
        }
        if ids != referenced {
            return Err(invalid("checkpoint dependency inventory is incomplete"));
        }
        let mut dependencies = Vec::new();
        let mut segments = Vec::new();
        for row in rows {
            if positions.iter().any(|position| {
                position.disk.segment == row.segment
                    && position
                        .disk
                        .offset
                        .checked_add(position.disk.len)
                        .is_none_or(|end| end > row.bytes)
            }) {
                return Err(invalid("checkpoint lineage reads outside segment"));
            }
            let path = moruna_placement::manifest::segment_file(directory, row.segment);
            let mut input = open_read(&path)?;
            if input.metadata().map_err(local_io)?.len() < row.bytes {
                return Err(invalid("checkpoint segment truncated"));
            }
            let mut pieces = Vec::new();
            let mut offset = 0;
            while offset < row.bytes {
                let len = (row.bytes - offset).min(self.chunk_bytes as u64) as usize;
                let mut buffer = prepared
                    .chunk
                    .take()
                    .ok_or_else(|| invalid("checkpoint upload buffer retained"))?;
                input
                    .read_exact(&mut buffer.as_mut()[..len])
                    .map_err(local_io)?;
                let target = object_under(
                    bucket_target,
                    &format!("{}/{sequence}/{}/{offset}", manifest.run_id, row.segment),
                );
                let buffer = Arc::new(buffer);
                let dependency = self
                    .authority
                    .export_bytes(access, &target, buffer.view().slice(0, len))
                    .await?;
                prepared.chunk = Some(
                    Arc::try_unwrap(buffer)
                        .map_err(|_| invalid("checkpoint backend retained upload buffer"))?,
                );
                if dependency.metadata.bytes != len as u64 {
                    return Err(moruna_storage::Error::Integrity);
                }
                pieces.push(Piece {
                    offset,
                    bytes: len as u64,
                    dependency: dependencies.len(),
                });
                dependencies.push(dependency);
                offset += len as u64;
            }
            segments.push(Segment { row, pieces });
        }
        // Real resident bytes use the SAME native segment codec as normal placement.
        // Synthetic IDs are unique within this immutable snapshot, never local file aliases.
        for (stage, segment, record) in resident {
            let mut pieces = Vec::new();
            let mut offset = 0;
            while offset < record.segment_bytes() {
                let len = (record.segment_bytes() - offset).min(self.chunk_bytes as u64) as usize;
                let mut buffer = prepared
                    .chunk
                    .take()
                    .ok_or_else(|| invalid("checkpoint upload buffer retained"))?;
                let materialized = record
                    .materialize_prefix(offset, len, &mut buffer)
                    .map_err(|_| invalid("checkpoint resident materialization"))?;
                self.alloc.note_payload_copy(materialized.copied_bytes);
                let target = object_under(
                    bucket_target,
                    &format!("{}/{sequence}/{segment}/{offset}", manifest.run_id),
                );
                let buffer = Arc::new(buffer);
                let dependency = self
                    .authority
                    .export_bytes(access, &target, buffer.view().slice(0, len))
                    .await?;
                prepared.chunk = Some(
                    Arc::try_unwrap(buffer)
                        .map_err(|_| invalid("checkpoint backend retained upload buffer"))?,
                );
                if dependency.metadata.bytes != len as u64 {
                    return Err(moruna_storage::Error::Integrity);
                }
                pieces.push(Piece {
                    offset,
                    bytes: len as u64,
                    dependency: dependencies.len(),
                });
                dependencies.push(dependency);
                offset += len as u64;
            }
            segments.push(Segment {
                row: SegmentRow {
                    stage,
                    segment,
                    bytes: record.segment_bytes(),
                },
                pieces,
            });
        }
        // Every old disk location is now represented by an explicit verified dependency map.
        // This is not the legacy clearing-and-requery shortcut: restore reconstructs the bytes.
        manifest.durable_staging = true;
        let mut value = serde_json::to_value(manifest)
            .map_err(|_| invalid("engine checkpoint serialization"))?;
        value
            .as_object_mut()
            .ok_or_else(|| invalid("engine checkpoint object"))?
            .insert(
                FIELD.into(),
                serde_json::to_value(Staging {
                    version: ENGINE_VERSION,
                    segments,
                    positions,
                })
                .map_err(|_| invalid("checkpoint dependency map"))?,
            );
        if let Some(prefixes) = &prepared.history {
            value
                .as_object_mut()
                .ok_or_else(|| invalid("engine checkpoint object"))?
                .insert(
                    HISTORY_FIELD.into(),
                    serde_json::to_value(prefixes)
                        .map_err(|_| invalid("capture prefix metadata"))?,
                );
        }
        let portable = Portable {
            version: moruna_storage::VERSION,
            workspace: access.workspace.clone(),
            run: access.run.clone(),
            sequence,
            manifest: value,
            dependencies,
        };
        self.authority
            .publish(access, local_target, bucket_target, condition, &portable)
            .await
    }
    /// Restore only from an actual pinned bucket acknowledgement, under this requester's
    /// live authority. Materialize exact segments in a NEW own run directory before publishing
    /// the native manifest; no layer re-execution or node-local reference relabelling.
    pub async fn restore(
        &self,
        access: &Access,
        pinned: &Metadata,
        staging_root: &Path,
    ) -> moruna_storage::Result<PathBuf> {
        let portable = self.authority.restore(access, pinned).await?;
        let mut value = portable.manifest;
        let map = value
            .as_object_mut()
            .ok_or_else(|| invalid("portable engine manifest"))?;
        let staging: Staging = serde_json::from_value(
            map.remove(FIELD)
                .ok_or_else(|| invalid("portable engine dependency map absent"))?,
        )
        .map_err(|_| invalid("portable engine dependency map"))?;
        let history = map.remove(HISTORY_FIELD);
        let mut manifest: Manifest =
            serde_json::from_value(value).map_err(|_| invalid("portable engine manifest"))?;
        if staging.version != ENGINE_VERSION
            || manifest.version != moruna_placement::manifest::MANIFEST_VERSION
            || manifest.run_id != portable.run
            || RunId::from_hex(&manifest.run_id).is_none()
            || !manifest.segments.is_empty()
            || !manifest.issued.is_empty()
            || manifest.lineage.iter().any(|row| row.disk.is_some())
        {
            return Err(invalid("portable engine version/identity/unresolved state"));
        }
        let mut ids = HashMap::new();
        let mut used = HashSet::new();
        for segment in &staging.segments {
            if ids.insert(segment.row.segment, segment.row.bytes).is_some() {
                return Err(invalid("portable segment repeated"));
            }
            let mut offset = 0;
            for piece in &segment.pieces {
                let dependency = portable
                    .dependencies
                    .get(piece.dependency)
                    .ok_or_else(|| invalid("portable dependency index"))?;
                if piece.offset != offset
                    || piece.bytes == 0
                    || piece.bytes != dependency.metadata.bytes
                    || piece.bytes > self.chunk_bytes as u64
                    || !used.insert(piece.dependency)
                {
                    return Err(invalid("portable dependency span/order/budget"));
                }
                offset = offset
                    .checked_add(piece.bytes)
                    .ok_or_else(|| invalid("portable dependency range overflow"))?;
            }
            if offset != segment.row.bytes {
                return Err(invalid("portable segment is incomplete"));
            }
        }
        if used.len() != portable.dependencies.len() {
            return Err(invalid("portable dependencies were not fully inventoried"));
        }
        let mut positions = HashSet::new();
        for position in &staging.positions {
            if !positions.insert(position.row)
                || position.row >= manifest.lineage.len()
                || position
                    .disk
                    .offset
                    .checked_add(position.disk.len)
                    .is_none_or(|end| {
                        ids.get(&position.disk.segment)
                            .is_none_or(|bytes| end > *bytes)
                    })
            {
                return Err(invalid("portable lineage location"));
            }
        }
        if positions.len() != manifest.lineage.len() {
            return Err(invalid(
                "portable lineage lacks a complete actual byte inventory",
            ));
        }
        if std::fs::symlink_metadata(staging_root)
            .map_err(local_io)?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("portable staging root is a link"));
        }
        let directory = staging_root.join(format!("moruna-{}", manifest.run_id));
        std::fs::create_dir(&directory).map_err(local_io)?; // never overwrite an existing run
        for segment in staging.segments {
            let path = moruna_placement::manifest::segment_file(&directory, segment.row.segment);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(local_io)?;
            for piece in segment.pieces {
                let buffer = self
                    .alloc
                    .alloc(
                        piece.bytes as usize,
                        if self.alloc.is_pinned() {
                            Tier::PinnedHost
                        } else {
                            Tier::Host
                        },
                    )
                    .map_err(|_| invalid("checkpoint restore arena"))?;
                let data = self
                    .authority
                    .dependency(access, &portable.dependencies[piece.dependency], buffer)
                    .await?;
                file.write_all(data.as_ref()).map_err(local_io)?;
            }
            file.sync_all().map_err(local_io)?;
            manifest.segments.push(segment.row);
        }
        for position in staging.positions {
            manifest.lineage[position.row].disk = Some(position.disk);
        }
        let mut restored =
            serde_json::to_value(&manifest).map_err(|_| invalid("restored engine manifest"))?;
        if let Some(history) = history {
            restored
                .as_object_mut()
                .ok_or_else(|| invalid("restored engine manifest"))?
                .insert(HISTORY_FIELD.into(), history);
        }
        let bytes =
            serde_json::to_vec(&restored).map_err(|_| invalid("restored engine manifest"))?;
        if bytes.len() > self.metadata_bytes {
            return Err(invalid("restored engine metadata ceiling"));
        }
        let temporary = directory.join("manifest.restoring");
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(local_io)?;
        output.write_all(&bytes).map_err(local_io)?;
        output.sync_all().map_err(local_io)?;
        let path = directory.join("manifest.json");
        std::fs::rename(temporary, &path).map_err(local_io)?;
        File::open(&directory)
            .map_err(local_io)?
            .sync_all()
            .map_err(local_io)?;
        Ok(path)
    }
}

/// A live scoped native checkpoint writer. Targets and Access come from the embedding's
/// actual storage policy; no URL credentials or implicit cloud/local fallback are inferred.
pub struct Publisher {
    /// Sole governed checkpoint authority.
    pub authority: Arc<Checkpoints>,
    /// Live producer authority, retained without cloning credential bytes.
    pub access: Arc<Access>,
    /// Local durable acknowledgement target.
    pub local_target: Object,
    /// Bucket acknowledgement target.
    pub bucket_target: Object,
    /// Explicit upload chunk capacity.
    pub chunk_bytes: usize,
    /// Explicit encoded manifest ceiling.
    pub metadata_bytes: usize,
    /// Actual pre-barrier codec reservation, including framing and boundary-owned buffers.
    pub encoding_bytes: usize,
    /// Whole publication deadline, including immutable dependency verification.
    pub timeout: std::time::Duration,
    state: Mutex<WriterState>,
}
#[derive(Default)]
struct WriterState {
    sequence: u64,
    receipt: Option<Metadata>,
    local_pending: Option<Metadata>,
}
impl Publisher {
    /// Create a single-writer native port with no pre-existing bucket receipt. To resume an
    /// existing writer, call `resume_at` using the freshly authorized actual bucket readback.
    #[expect(
        clippy::too_many_arguments,
        reason = "Public publisher admission keeps explicit authority, two backend targets, separate buffer budgets and deadline; no implicit defaults"
    )]
    pub fn new(
        authority: Arc<Checkpoints>,
        access: Arc<Access>,
        local_target: Object,
        bucket_target: Object,
        chunk_bytes: usize,
        metadata_bytes: usize,
        encoding_bytes: usize,
        timeout: std::time::Duration,
    ) -> moruna_storage::Result<Self> {
        if chunk_bytes == 0 || metadata_bytes == 0 || encoding_bytes == 0 || timeout.is_zero() {
            return Err(invalid("checkpoint publisher admission"));
        }
        Ok(Self {
            authority,
            access,
            local_target,
            bucket_target,
            chunk_bytes,
            metadata_bytes,
            encoding_bytes,
            timeout,
            state: Mutex::new(WriterState::default()),
        })
    }
    /// Freshly authorize and validate the actual immutable bucket checkpoint before allowing
    /// conditional continuation. Never accepts a caller-invented sequence or version alone.
    pub async fn resume_at(&self, receipt: &Metadata) -> moruna_storage::Result<()> {
        if receipt.object != self.bucket_target {
            return Err(invalid("checkpoint writer receipt target"));
        }
        let checkpoint = self.authority.restore(&self.access, receipt).await?;
        if checkpoint.run != self.access.run || checkpoint.workspace != self.access.workspace {
            return Err(moruna_storage::Error::Denied);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.sequence != 0 || state.receipt.is_some() {
            return Err(invalid("checkpoint writer already started"));
        }
        state.sequence = checkpoint.sequence;
        state.receipt = Some(receipt.clone());
        Ok(())
    }
    /// Last genuine complete bucket acknowledgement, never the local-only pending receipt.
    pub fn receipt(&self) -> Option<Metadata> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipt
            .clone()
    }
    /// Actual acknowledged local pointer when the latest bucket publication refused.
    /// This is diagnostic recovery evidence, not a portable bucket acknowledgement.
    pub fn local_pending(&self) -> Option<Metadata> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .local_pending
            .clone()
    }

    /// Build scheduler integration against its actual allocator/placement. Preparation runs
    /// before the pause. The returned publication is one-shot and holds no mutex over IO.
    pub(crate) fn preparation(
        self: &Arc<Self>,
        alloc: Arc<dyn Allocator>,
        engine: Arc<moruna_placement::PlacementEngine>,
        history: Option<crate::history::Checkpoint>,
    ) -> moruna_scheduler::ManifestPreparation {
        let publisher = self.clone();
        Arc::new(move || {
            let bridge = EngineCheckpoints::new(
                publisher.authority.clone(),
                alloc.clone(),
                publisher.chunk_bytes,
                publisher.metadata_bytes,
            )
            .map_err(storage_error)?;
            let prepared = bridge
                .prepare(publisher.encoding_bytes)
                .map_err(storage_error)?;
            let publisher = publisher.clone();
            let engine = engine.clone();
            let admission = Mutex::new(Some(prepared));
            let history = history.clone();
            Ok(Arc::new(move |path: &Path| {
                let mut prepared = admission
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .ok_or_else(|| {
                        MorunaError::Resume("checkpoint admission already consumed".into())
                    })?;
                let (sequence, condition) = {
                    let mut state = publisher.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.sequence = state.sequence.checked_add(1).ok_or_else(|| {
                        MorunaError::Resume("checkpoint sequence exhausted".into())
                    })?;
                    (
                        state.sequence,
                        state.receipt.as_ref().map_or(Condition::Absent, |receipt| {
                            Condition::Match(receipt.version.clone())
                        }),
                    )
                };
                if let Some(history) = &history {
                    prepared.history = Some(history.snapshot(sequence)?);
                }
                let runtime = moruna_reactor::configure_runtime(
                    &mut tokio::runtime::Builder::new_current_thread(),
                )
                .build()
                .map_err(|_| MorunaError::Resume("checkpoint IO runtime unavailable".into()))?;
                let receipt = runtime.block_on(async {
                    tokio::time::timeout(publisher.timeout, bridge.publish(&publisher.access,
                        path, sequence, &publisher.local_target, &publisher.bucket_target,
                        condition, &mut prepared, Some(&engine))).await
                        .map_err(|_| MorunaError::Resume("checkpoint bucket publication deadline elapsed; no portable acknowledgement".into()))?
                        .map_err(|error| {
                            if let moruna_storage::Error::PortablePending {local, ..} = &error {
                                publisher.state.lock().unwrap_or_else(|e| e.into_inner()).local_pending = Some((**local).clone());
                            }
                            storage_error(error)
                        })
                })?;
                let mut state = publisher.state.lock().unwrap_or_else(|e| e.into_inner());
                state.receipt = Some(receipt);
                state.local_pending = None;
                Ok(())
            }) as moruna_scheduler::ManifestHook)
        })
    }
}
fn storage_error(error: moruna_storage::Error) -> MorunaError {
    MorunaError::Resume(format!("governed checkpoint refused: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_testkit::FakeAllocator;

    #[test]
    fn a_reservation_answers_for_its_source_and_refuses_what_it_cannot_cover() {
        let source = Arc::new(FakeAllocator::new());
        let pool = source.alloc(8192, Tier::Host).unwrap();
        let inside = pool.host_ptr().unwrap() as *const u8;
        let reservation = ReservedAllocator {
            source: source.clone(),
            pool: Mutex::new(Some(pool)),
        };
        assert_eq!(
            reservation.page_bytes(),
            Allocator::page_bytes(source.as_ref())
        );
        assert_eq!(
            reservation.is_pinned(),
            Allocator::is_pinned(source.as_ref())
        );
        assert_eq!(
            reservation.contains(inside),
            Allocator::contains(source.as_ref(), inside)
        );
        assert_eq!(
            reservation.tier_of(inside),
            Allocator::tier_of(source.as_ref(), inside)
        );
        reservation.note_payload_copy(3);
        reservation.note_boundary_copy(5);
        assert_eq!(
            reservation.stats().allocations_total,
            Allocator::stats(source.as_ref()).allocations_total
        );
        // More than the reservation holds is an allocation refusal, never a new source alloc.
        assert!(matches!(
            reservation.alloc(1 << 20, Tier::Host),
            Err(MorunaError::Alloc { .. })
        ));
        // A different tier than the reservation's is refused and the pool is kept.
        assert!(matches!(
            reservation.alloc(16, Tier::PinnedHost),
            Err(MorunaError::Resume(_))
        ));
        assert!(
            reservation.alloc(16, Tier::Host).is_ok(),
            "the pool was kept"
        );
    }

    #[test]
    fn checkpoint_codec_admission_never_allocates_from_the_live_run() {
        let source = Arc::new(FakeAllocator::new());
        let pool = source.alloc(8192, Tier::Host).unwrap();
        let reservation = ReservedAllocator {
            source: source.clone(),
            pool: Mutex::new(Some(pool)),
        };
        let allocations = source.stats().allocations_total;
        let first = reservation.alloc(17, Tier::Host).unwrap();
        let second = reservation.alloc(4096, Tier::Host).unwrap();
        assert_eq!(first.len(), 17);
        assert_eq!(second.len(), 4096);
        assert_eq!(second.host_ptr().unwrap() as usize % 4096, 0);
        assert!(matches!(
            reservation.alloc(1, Tier::Host),
            Err(MorunaError::Alloc { .. })
        ));
        // Freed pieces return to their true original owner, never quietly refill this
        // checkpoint's exhausted ceiling or turn into a new live allocation.
        drop(first);
        drop(second);
        assert!(reservation.alloc(1, Tier::Host).is_err());
        assert_eq!(source.stats().allocations_total, allocations);
    }

    #[test]
    fn wrong_tier_refusal_preserves_the_actual_checkpoint_reservation() {
        let source = Arc::new(FakeAllocator::new());
        let pool = source.alloc(4096, Tier::Host).unwrap();
        let reservation = ReservedAllocator {
            source,
            pool: Mutex::new(Some(pool)),
        };
        assert!(reservation.alloc(4096, Tier::PinnedHost).is_err());
        assert_eq!(
            reservation.alloc(4096, Tier::Host).unwrap().tier(),
            Tier::Host
        );
    }
}

#[cfg(test)]
mod dependency_roundtrip_tests {
    use super::*;
    use moruna_kernel::arrow::{
        array::Int64Array,
        datatypes::{DataType, Field, Schema},
        record_batch::RecordBatch,
    };
    use moruna_placement::state::PayloadRef;
    use moruna_storage::{
        Action, Authorizer, Credential, Governed, Operation, Role, local::FileStore,
    };
    use moruna_testkit::FakeAllocator;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Verifier(AtomicBool);
    impl Authorizer for Verifier {
        fn verify<'a>(
            &'a self,
            access: &'a Access,
            object: &'a Object,
            _: Action,
            _: Option<&'a moruna_storage::Version>,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.0.load(Ordering::SeqCst)
                    && access.workspace == "fixture"
                    && object.workspace == "fixture"
                    && access.credential.application() == Some(&b"TEST-ONLY"[..])
                {
                    Ok(())
                } else {
                    Err(moruna_storage::Error::Denied)
                }
            })
        }
    }
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn fixture() -> Scratch {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "moruna-checkpoint-fixture-{}",
            RunId(nonce).to_hex()
        ));
        std::fs::create_dir(&path).unwrap();
        Scratch(path)
    }
    fn object(key: &str) -> Object {
        Object {
            workspace: "fixture".into(),
            bucket: "home".into(),
            key: key.into(),
        }
    }
    fn governed(path: PathBuf, role: Role, verifier: Arc<Verifier>) -> Arc<Governed> {
        Arc::new(
            Governed::new(
                Arc::new(FileStore::open(path, "fixture".into(), "home".into(), role).unwrap()),
                verifier,
            )
            .unwrap(),
        )
    }

    /// Deterministic governed durable-format test, not a cloud/raw-device performance proof.
    /// The home stores actual immutable bytes and survives destruction of the writer's staging.
    #[test]
    fn actual_segment_rows_and_state_restore_after_writer_staging_is_destroyed() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let scratch = fixture();
            let alloc:Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
            let verifier = Arc::new(Verifier(AtomicBool::new(true)));
            let home = governed(scratch.0.join("durable-format-home"),Role::FormatProof,verifier.clone());
            let writer_local = governed(scratch.0.join("writer-metadata"),Role::CheckpointMetadata,verifier.clone());
            let access = Access{workspace:"fixture".into(),run:RunId([8;16]).to_hex(),credential:Credential::new(b"TEST-ONLY".to_vec()).unwrap()};
            let batch = RecordBatch::try_new(Arc::new(Schema::new(vec![Field::new("id",DataType::Int64,false)])),
                vec![Arc::new(Int64Array::from(vec![19,23,41]))]).unwrap();
            let record = moruna_placement::portable_record::encode(&PayloadRef::Table(batch),0,0,7,alloc.as_ref(),4096).unwrap();
            let writer_staging = scratch.0.join("writer-staging"); std::fs::create_dir(&writer_staging).unwrap();
            let segment = moruna_placement::manifest::segment_file(&writer_staging,7);
            let mut bytes = alloc.alloc(record.segment_bytes() as usize,Tier::Host).unwrap();
            record.materialize(0,&mut bytes).unwrap(); std::fs::write(&segment,bytes.as_ref()).unwrap();
            let manifest:Manifest = serde_json::from_value(serde_json::json!({
                "version":moruna_placement::manifest::MANIFEST_VERSION,"run_id":access.run,
                "written_ns":1,"node":0,"hostname":"writer-node","nodes":[0],"durable_staging":false,
                "plan_digests":[],"kernels":[],"resume_policy":[],"queues":1,"committed":[null],
                "source_cursors":[],"issued":[],"sink_states":["AQID"],
                "kernel_states":[{"stage":1,"instance":0,"state":"AwAAAA=="}],
                "lineage":[{"source":0,"root":0,"seq":0,"split":0,"row_start":0,"row_end":3,
                    "node":0,"stage":0,"kind":"table","bytes":24,"disk":record.disk()}],
                "segments":[{"stage":0,"segment":7,"bytes":record.segment_bytes()}],"config":{}
            })).unwrap();
            let path = writer_staging.join("manifest.json"); std::fs::write(&path,serde_json::to_vec(&manifest).unwrap()).unwrap();
            let authority = Arc::new(Checkpoints::new(writer_local,home.clone(),alloc.clone(),65536).unwrap());
            let bridge = EngineCheckpoints::new(authority,alloc.clone(),4096,65536).unwrap();
            let receipt = bridge.publish(&access,&path,1,&object("local"),&object("checkpoint"),Condition::Absent,
                &mut bridge.prepare(65536).unwrap(),None).await.unwrap();
            std::fs::remove_dir_all(&writer_staging).unwrap();
            assert!(!segment.exists());
            let reader_local = governed(scratch.0.join("other-node-metadata"),Role::CheckpointMetadata,verifier.clone());
            let restored_authority = Arc::new(Checkpoints::new(reader_local,home,alloc.clone(),65536).unwrap());
            let other = EngineCheckpoints::new(restored_authority,alloc.clone(),4096,65536).unwrap();
            let other_staging = scratch.0.join("other-node-staging");std::fs::create_dir(&other_staging).unwrap();
            let restored_path = other.restore(&access,&receipt,&other_staging).await.unwrap();
            let restored:Manifest = serde_json::from_slice(&std::fs::read(&restored_path).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&restored.kernel_states).unwrap(),serde_json::to_value(&manifest.kernel_states).unwrap());
            assert_eq!(restored.sink_states,manifest.sink_states);
            let location = restored.lineage[0].disk.as_ref().unwrap();
            let full = std::fs::read(moruna_placement::manifest::segment_file(restored_path.parent().unwrap(),location.segment)).unwrap();
            let mut body = alloc.alloc(location.len as usize,Tier::Host).unwrap();
            body.as_mut().copy_from_slice(&full[location.offset as usize..(location.offset+location.len) as usize]);
            let decoded = moruna_kernel::ipc::decode(body.into_arrow_buffer().unwrap(),4096).unwrap();
            assert_eq!(decoded.num_rows(),3);
            let values = decoded.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
            assert_eq!(values.values().as_ref(), &[19,23,41]);
            verifier.0.store(false,Ordering::SeqCst);
            let refused_staging = scratch.0.join("revoked-node");std::fs::create_dir(&refused_staging).unwrap();
            assert!(matches!(other.restore(&access,&receipt,&refused_staging).await,Err(moruna_storage::Error::Denied)));
            assert_eq!(std::fs::read_dir(refused_staging).unwrap().count(),0);
        });
    }
}
