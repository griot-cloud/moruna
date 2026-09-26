//! The run manifest (e.5), its atomic write (f.12) and the resume it feeds (f.13).
//!
//! The manifest is the one file the engine writes itself, with `std::fs`, because it is
//! small and needs `fsync` and `rename` in order on the calling thread (PL-I15). It is
//! written to a temporary name and renamed into place, so a reader sees either the previous
//! manifest or the new one and never a partial one (PL-I12).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use moruna_kernel::{
    CheckpointExtras, Fingerprint, MorunaError, NodeId, Origin, PayloadKind, ResumePoint,
    ResumePolicy, RunId, SegmentRef, Seq, SourceCursor, Split, StageId,
};
use serde::{Deserialize, Serialize};

use crate::PlacementEngine;
use crate::staging::Segment;
use crate::state::Entry;

/// The manifest version this build writes and the only one it reads (e.5).
pub const MANIFEST_VERSION: u32 = 1;

/// Only the identity fields of a manifest, so the facade can build a `PlacementConfig` with
/// the manifest's run id before `restore` (d.1).
#[derive(Clone, Debug)]
pub struct ManifestHeader {
    /// The manifest format version.
    pub version: u32,
    /// The run the manifest belongs to.
    pub run_id: RunId,
    /// The node that wrote it.
    pub node: NodeId,
    /// That node's hostname.
    pub hostname: String,
    /// Wall clock at the write, nanoseconds since the epoch.
    pub written_ns: u64,
}

/// Where a record's bytes are in a segment (e.5).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskRef {
    /// Segment number.
    pub segment: u32,
    /// Body offset.
    pub offset: u64,
    /// Body length.
    pub len: u64,
}

/// One uncommitted morsel (e.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineageRow {
    /// The morsel.
    pub seq: u64,
    /// Its origin split.
    pub split: u32,
    /// First row, inclusive.
    pub row_start: u64,
    /// Last row, exclusive.
    pub row_end: u64,
    /// The node that read the split.
    pub node: u16,
    /// The stage whose output the morsel currently is.
    pub stage: u16,
    /// `table` or `tensor`.
    pub kind: String,
    /// Payload bytes.
    pub bytes: u64,
    /// The record holding a copy, when there is one.
    pub disk: Option<DiskRef>,
}

/// One segment the lineage references (e.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentRow {
    /// The stage whose records it holds.
    pub stage: u16,
    /// Its global number.
    pub segment: u32,
    /// Bytes written so far.
    pub bytes: u64,
}

/// Where the source drive is (e.5).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
pub struct CursorRow {
    /// Index into the plan.
    pub split_index: u32,
    /// Next row within that split.
    pub row_offset: u64,
    /// Next sequence number to assign.
    pub next_seq: u64,
}

/// One read the source drive had issued and not yet pushed to Q0 (e.5, MH 4.7).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssuedRow {
    /// The sequence number the read was given.
    pub seq: u64,
    /// Its split.
    pub split: u32,
    /// First row, inclusive.
    pub row_start: u64,
    /// Last row, exclusive.
    pub row_end: u64,
    /// The node that issued it.
    pub node: u16,
}

/// One stateful kernel instance's checkpoint (e.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KernelStateRow {
    /// The stage.
    pub stage: u16,
    /// The instance within the stage.
    pub instance: u64,
    /// Base64 of `KernelState::checkpoint`.
    pub state: String,
}

/// The manifest, exactly the table of e.5 and nothing else.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version; an unknown version is refused, not skipped.
    pub version: u32,
    /// 32 hex characters.
    pub run_id: String,
    /// Wall clock at the write, nanoseconds since the epoch.
    pub written_ns: u64,
    /// The writer's node.
    pub node: u16,
    /// The writer's hostname.
    pub hostname: String,
    /// Every node of the run; `[0]` in v1.
    pub nodes: Vec<u16>,
    /// Whether the staging directory survives the node.
    pub durable_staging: bool,
    /// BLAKE3 hex over the source plan.
    pub plan_digest: String,
    /// Kernel fingerprints per stage 1..n, hex.
    pub kernels: Vec<String>,
    /// `reinit`, `checkpoint` or `forbid` per stage 1..n.
    pub resume_policy: Vec<String>,
    /// Number of queues.
    pub stages: u16,
    /// The watermark at the write.
    pub committed_seq: Option<u64>,
    /// Where the source drive is.
    pub source_cursor: CursorRow,
    /// Reads issued behind the cursor that had not reached Q0, ascending `seq`. Absent from a
    /// manifest written before F8.4, which reads as empty: such a manifest loses a read in
    /// flight exactly as it always did, and nothing else.
    #[serde(default)]
    pub issued: Vec<IssuedRow>,
    /// Base64 of `Sink::checkpoint`.
    pub sink_state: Option<String>,
    /// `Checkpoint` kernels only.
    pub kernel_states: Vec<KernelStateRow>,
    /// One row per uncommitted morsel, ascending `seq`.
    pub lineage: Vec<LineageRow>,
    /// Every segment file the lineage references.
    pub segments: Vec<SegmentRow>,
    /// The resolved configuration table at the write.
    pub config: serde_json::Value,
}

/// BLAKE3 over the source plan: for each split in order, `id` and `rows` as little-endian
/// u64s. `uncompressed_bytes` is excluded because it may be an estimate that varies between
/// plans (e.5).
pub fn plan_digest(plan: &[Split]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for split in plan {
        hasher.update(&u64::from(split.id).to_le_bytes());
        hasher.update(&split.rows.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn policy_name(policy: ResumePolicy) -> &'static str {
    match policy {
        ResumePolicy::Reinit => "reinit",
        ResumePolicy::Checkpoint => "checkpoint",
        ResumePolicy::Forbid => "forbid",
    }
}

fn kind_name(kind: PayloadKind) -> &'static str {
    match kind {
        PayloadKind::Table | PayloadKind::Either => "table",
        PayloadKind::Tensor => "tensor",
    }
}

fn kind_of_name(name: &str) -> Option<PayloadKind> {
    match name {
        "table" => Some(PayloadKind::Table),
        "tensor" => Some(PayloadKind::Tensor),
        _ => None,
    }
}

fn resume(path: &Path, what: impl std::fmt::Display) -> MorunaError {
    MorunaError::Resume(format!("{}: {what}", path.display()))
}

/// Read a manifest from disk and parse it (f.13).
pub fn read_manifest(path: &Path) -> moruna_kernel::Result<Manifest> {
    let text = std::fs::read_to_string(path).map_err(|e| resume(path, e))?;
    serde_json::from_str(&text).map_err(|e| resume(path, e))
}

/// Read only the identity fields of a manifest (d.1).
pub fn read_manifest_header(path: &Path) -> moruna_kernel::Result<ManifestHeader> {
    let manifest = read_manifest(path)?;
    let Some(run_id) = RunId::from_hex(&manifest.run_id) else {
        return Err(resume(path, "run id is not 32 hex characters"));
    };
    Ok(ManifestHeader {
        version: manifest.version,
        run_id,
        node: NodeId(manifest.node),
        hostname: manifest.hostname,
        written_ns: manifest.written_ns,
    })
}

/// Locate the newest manifest (by `written_ns`) under `staging_dir` for `run_id`, or, with
/// `None`, the newest manifest of any run in that directory (d.1).
pub fn find_manifest(
    staging_dir: &Path,
    run_id: Option<RunId>,
) -> moruna_kernel::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MorunaError::Io {
                op: "read_dir",
                target: staging_dir.display().to_string(),
                msg: e.to_string(),
            });
        }
    };
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let candidate = entry.path().join("manifest.json");
        if !candidate.is_file() {
            continue;
        }
        let Ok(header) = read_manifest_header(&candidate) else {
            continue;
        };
        if let Some(wanted) = run_id
            && header.run_id != wanted
        {
            continue;
        }
        let newer = best
            .as_ref()
            .is_none_or(|(written, _)| header.written_ns > *written);
        if newer {
            best = Some((header.written_ns, candidate));
        }
    }
    Ok(best.map(|(_, path)| path))
}

/// What a manifest must agree with for `resume = "auto"` to pick it (MH 4.7): the same kernel
/// fingerprints, stage by stage, and, when the caller knows them, the same plan digest and the
/// same job document digest (`config["spec.digest"]`, written by a run built from a job
/// document). A field left `None` is not compared, and `restore` still checks every one of them.
#[derive(Clone, Debug, Default)]
pub struct ManifestMatch<'a> {
    /// The run's kernel fingerprints, stages 1..n.
    pub fingerprints: &'a [Fingerprint],
    /// The run's plan digest (e.5), when its source is already built.
    pub plan_digest: Option<[u8; 32]>,
    /// The run's job document digest, when it was built from one.
    pub spec_digest: Option<&'a str>,
}

impl ManifestMatch<'_> {
    /// Why `manifest` cannot be this run's, or `None` when it can.
    fn refuses(&self, manifest: &Manifest, here: &str) -> Option<String> {
        if manifest.version != MANIFEST_VERSION {
            return Some(format!("version {}", manifest.version));
        }
        let given: Vec<String> = self.fingerprints.iter().map(Fingerprint::to_hex).collect();
        if manifest.kernels != given {
            return Some("kernel fingerprints differ".into());
        }
        if let Some(digest) = &self.plan_digest
            && manifest.plan_digest != hex(digest)
        {
            return Some("plan digest differs".into());
        }
        if let Some(digest) = self.spec_digest {
            let theirs = manifest.config.get("spec.digest").and_then(|v| v.as_str());
            if theirs != Some(digest) {
                return Some("job document digest differs".into());
            }
        }
        if manifest
            .resume_policy
            .iter()
            .any(|policy| policy == "forbid")
        {
            return Some("a stage forbids resume".into());
        }
        if !manifest.durable_staging && manifest.hostname != here {
            return Some(format!(
                "written on {} and its staging is not durable",
                manifest.hostname
            ));
        }
        None
    }
}

/// `resume = "auto"` (MH 4.7): among `staging_dir/moruna-*/manifest.json`, the newest by
/// `written_ns` that `want` accepts. A directory that does not exist, or holds nothing that
/// matches, is `Ok(None)`: the caller starts fresh. Each manifest passed over is logged with
/// the reason, so a host that expected a resume can see why it did not get one.
pub fn find_resumable_manifest(
    staging_dir: &Path,
    want: &ManifestMatch<'_>,
) -> moruna_kernel::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MorunaError::Io {
                op: "read_dir",
                target: staging_dir.display().to_string(),
                msg: e.to_string(),
            });
        }
    };
    let here = hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let is_run_dir = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("moruna-"));
        let candidate = entry.path().join("manifest.json");
        if !is_run_dir || !candidate.is_file() {
            continue;
        }
        let manifest = match read_manifest(&candidate) {
            Ok(manifest) => manifest,
            Err(e) => {
                tracing::info!(target: "placement.resume_auto", path = %candidate.display(), reason = %e, "passed over");
                continue;
            }
        };
        if let Some(reason) = want.refuses(&manifest, &here) {
            tracing::info!(target: "placement.resume_auto", path = %candidate.display(), %reason, "passed over");
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(written, _)| manifest.written_ns > *written)
        {
            best = Some((manifest.written_ns, candidate));
        }
    }
    Ok(best.map(|(_, path)| path))
}

impl PlacementEngine {
    /// `checkpoint` (f.12). Runs on the caller's thread, never under a queue lock, and the
    /// lineage lock is held only for the snapshot.
    pub(crate) fn write_manifest(
        &self,
        extras: &CheckpointExtras,
    ) -> moruna_kernel::Result<PathBuf> {
        let started = Instant::now();
        let Some(dir) = self.run_dir.clone() else {
            return Err(MorunaError::Resume("no staging directory".into()));
        };
        let committed_seq = extras.committed_seq.or_else(|| self.committed_seq());
        let snapshot = self.lineage_snapshot(committed_seq);
        let mut referenced: HashSet<u32> = HashSet::new();
        let mut lineage = Vec::with_capacity(snapshot.len());
        for (seq, record) in snapshot {
            if let Some(disk) = record.disk {
                referenced.insert(disk.segment);
            }
            lineage.push(LineageRow {
                seq,
                split: record.origin.split,
                row_start: record.origin.row_start,
                row_end: record.origin.row_end,
                node: record.origin.node.0,
                stage: record.stage,
                kind: kind_name(record.kind).to_string(),
                bytes: record.bytes,
                disk: record.disk.map(|disk| DiskRef {
                    segment: disk.segment,
                    offset: disk.offset,
                    len: disk.len,
                }),
            });
        }
        let segments: Vec<SegmentRow> = {
            let staging = self.lock_staging();
            let mut rows: Vec<SegmentRow> = staging
                .segments
                .iter()
                .filter(|(number, _)| referenced.contains(number))
                .map(|(number, segment)| SegmentRow {
                    stage: segment.stage,
                    segment: *number,
                    bytes: segment.bytes_written,
                })
                .collect();
            rows.sort_by_key(|row| row.segment);
            rows
        };
        let written_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos() as u64)
            .unwrap_or(0);
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            run_id: self.cfg.run_id.to_hex(),
            written_ns,
            node: self.cfg.node.0,
            hostname: hostname::get()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            nodes: vec![self.cfg.node.0],
            durable_staging: self.cfg.durable_staging,
            plan_digest: hex(&self.cfg.plan_digest),
            kernels: self
                .cfg
                .fingerprints
                .iter()
                .map(Fingerprint::to_hex)
                .collect(),
            resume_policy: self
                .cfg
                .resume_policy
                .iter()
                .map(|policy| policy_name(*policy).to_string())
                .collect(),
            stages: self.cfg.stages,
            committed_seq,
            source_cursor: CursorRow {
                split_index: extras.source_cursor.split_index,
                row_offset: extras.source_cursor.row_offset,
                next_seq: extras.source_cursor.next_seq,
            },
            issued: {
                let mut rows: Vec<IssuedRow> = extras
                    .issued
                    .iter()
                    .filter(|(seq, _)| committed_seq.is_none_or(|watermark| *seq > watermark))
                    .map(|(seq, origin)| IssuedRow {
                        seq: *seq,
                        split: origin.split,
                        row_start: origin.row_start,
                        row_end: origin.row_end,
                        node: origin.node.0,
                    })
                    .collect();
                rows.sort_by_key(|row| row.seq);
                rows.dedup_by_key(|row| row.seq);
                rows
            },
            sink_state: extras
                .sink_state
                .as_ref()
                .map(|state| base64::engine::general_purpose::STANDARD.encode(state)),
            kernel_states: extras
                .kernel_states
                .iter()
                .map(|(stage, instance, state)| KernelStateRow {
                    stage: *stage,
                    instance: *instance as u64,
                    state: base64::engine::general_purpose::STANDARD.encode(state),
                })
                .collect(),
            lineage,
            segments,
            config: self.cfg.config.clone(),
        };
        let text = serde_json::to_string_pretty(&manifest).map_err(|e| MorunaError::Io {
            op: "serialise",
            target: "manifest.json".into(),
            msg: e.to_string(),
        })?;
        let final_path = dir.join("manifest.json");
        let temporary = dir.join("manifest.json.tmp");
        let io = |op: &'static str, target: &Path, e: std::io::Error| MorunaError::Io {
            op,
            target: target.display().to_string(),
            msg: e.to_string(),
        };
        {
            let mut file =
                std::fs::File::create(&temporary).map_err(|e| io("create", &temporary, e))?;
            file.write_all(text.as_bytes())
                .map_err(|e| io("write", &temporary, e))?;
            file.sync_all().map_err(|e| io("fsync", &temporary, e))?;
        }
        std::fs::rename(&temporary, &final_path).map_err(|e| io("rename", &final_path, e))?;
        if let Ok(handle) = std::fs::File::open(&dir) {
            let _ = handle.sync_all();
        }
        {
            let mut refs = self.manifest_refs.lock().unwrap_or_else(|e| e.into_inner());
            *refs = referenced.clone();
        }
        let reclaim = self.reclaimable_segments(&referenced);
        self.unlink_segments(&reclaim);
        self.manifests_written.fetch_add(1, Ordering::Relaxed);
        let micros = started.elapsed().as_micros() as u64;
        self.last_manifest_us.store(micros, Ordering::Relaxed);
        tracing::debug!(
            target: "placement.checkpoint",
            lineage_len = manifest.lineage.len(),
            segments = manifest.segments.len(),
            duration_us = micros
        );
        Ok(final_path)
    }

    /// `restore` (f.13). Must precede any `push`.
    pub(crate) fn restore_from(
        &self,
        path: &Path,
        plan: &[Split],
        fingerprints: &[Fingerprint],
    ) -> moruna_kernel::Result<ResumePoint> {
        let manifest = read_manifest(path)?;
        if manifest.version != MANIFEST_VERSION {
            return Err(resume(
                path,
                format!("unknown manifest version {}", manifest.version),
            ));
        }
        if manifest.run_id != self.cfg.run_id.to_hex() {
            return Err(resume(
                path,
                format!(
                    "run id {} is not this run's {}",
                    manifest.run_id,
                    self.cfg.run_id.to_hex()
                ),
            ));
        }
        let computed = hex(&plan_digest(plan));
        if manifest.plan_digest != computed {
            return Err(resume(
                path,
                format!(
                    "plan digest {} does not match the plan's {computed}",
                    manifest.plan_digest
                ),
            ));
        }
        if hex(&self.cfg.plan_digest) != computed {
            return Err(resume(
                path,
                "the configured plan digest does not match the plan".to_string(),
            ));
        }
        let given: Vec<String> = fingerprints.iter().map(Fingerprint::to_hex).collect();
        if manifest.kernels != given {
            return Err(resume(path, "kernel fingerprints differ"));
        }
        if let Some(stage) = manifest
            .resume_policy
            .iter()
            .position(|policy| policy == "forbid")
        {
            return Err(resume(
                path,
                format!("stage {} declares resume policy forbid", stage + 1),
            ));
        }
        let here = hostname::get()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !manifest.durable_staging && manifest.hostname != here {
            return Err(resume(
                path,
                format!(
                    "the manifest was written on {} and its staging is not durable",
                    manifest.hostname
                ),
            ));
        }
        for key in ["staging.segment_bytes", "page.bytes"] {
            let theirs = config_at(&manifest.config, key);
            let ours = config_at(&self.cfg.config, key);
            if theirs != ours {
                return Err(resume(
                    path,
                    format!("{key} differs: {theirs:?} then, {ours:?} now"),
                ));
            }
        }
        // Every referenced segment must exist and reach past the furthest byte the lineage
        // reads, checked through the reactor so the check sees what a promotion would see
        // (PL-I15).
        let mut furthest: HashMap<u32, u64> = HashMap::new();
        for row in &manifest.lineage {
            if let Some(disk) = &row.disk {
                let end = disk.offset + disk.len;
                let slot = furthest.entry(disk.segment).or_insert(0);
                *slot = (*slot).max(end);
            }
        }
        for row in &manifest.segments {
            let Some(segment_path) = self.segment_path(row.segment) else {
                return Err(resume(path, "no staging directory"));
            };
            let Ok(meta) = std::fs::metadata(&segment_path) else {
                return Err(resume(
                    path,
                    format!(
                        "segment {} is missing at {}",
                        row.segment,
                        segment_path.display()
                    ),
                ));
            };
            self.reactor.register_segment(row.segment, &segment_path)?;
            let Some(end) = furthest.get(&row.segment).copied() else {
                continue;
            };
            if end > row.bytes {
                return Err(resume(
                    path,
                    format!(
                        "segment {} holds {} bytes, the lineage reads to {end}",
                        row.segment, row.bytes
                    ),
                ));
            }
            // f.13 checks the segment's length through the reactor, which would mean
            // waiting on a completion; PL-I14 forbids the engine ever calling
            // `Completion::wait`, and the preamble outranks the component document
            // (preamble section 9), so the file's own length is what is checked here. The
            // manifest arithmetic above already refuses a lineage row that reads past what
            // the manifest recorded. Reported as a finding.
            if meta.len() < end {
                return Err(resume(
                    path,
                    format!(
                        "segment {} is {} bytes on disk, the lineage reads to {end}",
                        row.segment,
                        meta.len()
                    ),
                ));
            }
        }
        if let Some(seq) = manifest.committed_seq {
            self.committed.store(seq, Ordering::Release);
        }
        let mut to_recompute: Vec<(Seq, Origin)> = Vec::new();
        let mut restored: Vec<(StageId, Entry)> = Vec::new();
        let mut per_segment: HashMap<u32, u64> = HashMap::new();
        {
            let mut lineage = self.lock_lineage();
            lineage.clear();
            let covered = |seq: u64| {
                manifest
                    .committed_seq
                    .is_some_and(|watermark| seq <= watermark)
            };
            for row in manifest.lineage.iter().filter(|row| !covered(row.seq)) {
                let Some(kind) = kind_of_name(&row.kind) else {
                    return Err(resume(path, format!("unknown payload kind {}", row.kind)));
                };
                let origin = Origin {
                    split: row.split,
                    row_start: row.row_start,
                    row_end: row.row_end,
                    node: NodeId(row.node),
                };
                let disk = row.disk.as_ref().map(|disk| SegmentRef {
                    segment: disk.segment,
                    offset: disk.offset,
                    len: disk.len,
                });
                lineage.insert(
                    row.seq,
                    crate::lineage::Lineage {
                        origin: origin.clone(),
                        stage: row.stage,
                        kind,
                        bytes: row.bytes,
                        disk,
                        consumed: false,
                        committed: false,
                    },
                );
                match disk {
                    Some(seg) => {
                        *per_segment.entry(seg.segment).or_insert(0) += 1;
                        restored.push((
                            row.stage,
                            Entry::on_disk(row.seq, row.stage, origin, row.bytes, kind, seg),
                        ));
                    }
                    None => to_recompute.push((row.seq, origin)),
                }
            }
        }
        restored.sort_by_key(|(_, entry)| entry.seq);
        // MH 4.7: a read in flight at the write is behind the cursor and in no queue; it is
        // read again unless the lineage caught it too (the drive pushes, then forgets, so a
        // read that landed between the two snapshots is in both).
        let known: HashSet<u64> = manifest.lineage.iter().map(|row| row.seq).collect();
        let issued: Vec<(Seq, Origin)> = manifest
            .issued
            .iter()
            .filter(|row| {
                manifest
                    .committed_seq
                    .is_none_or(|watermark| row.seq > watermark)
            })
            .map(|row| {
                (
                    row.seq,
                    Origin {
                        split: row.split,
                        row_start: row.row_start,
                        row_end: row.row_end,
                        node: NodeId(row.node),
                    },
                )
            })
            .collect();
        to_recompute.extend(
            issued
                .iter()
                .filter(|(seq, _)| !known.contains(seq))
                .cloned(),
        );
        to_recompute.sort_by_key(|(seq, _)| *seq);
        to_recompute.dedup_by_key(|(seq, _)| *seq);
        {
            let mut staging = self.lock_staging();
            staging.segments.clear();
            staging.active.clear();
            for row in &manifest.segments {
                let Some(segment_path) = self.segment_path(row.segment) else {
                    continue;
                };
                staging.segments.insert(
                    row.segment,
                    Segment {
                        stage: row.stage,
                        path: segment_path,
                        bytes_written: row.bytes,
                        records_total: per_segment.get(&row.segment).copied().unwrap_or(0),
                        records_released: 0,
                        active: false,
                    },
                );
            }
        }
        let highest = manifest
            .segments
            .iter()
            .map(|row| row.segment)
            .max()
            .map(|top| top + 1)
            .unwrap_or(0);
        self.next_segment.store(highest, Ordering::Release);
        self.disk_bytes.store(
            self.cfg.segment_bytes * manifest.segments.len() as u64,
            Ordering::Release,
        );
        for (stage, entry) in restored {
            let index = usize::from(stage);
            if index >= self.queues.len() {
                return Err(resume(path, format!("stage {stage} is outside the run")));
            }
            let mut queue = self.lock_queue(index);
            queue.order.push_back(entry);
            self.counters[index].count.fetch_add(1, Ordering::Relaxed);
        }
        self.restored.store(true, Ordering::Release);
        // The restored heads are on disk; nothing else will trigger the first promotion.
        self.plan_every_queue();
        let on_disk = manifest
            .lineage
            .iter()
            .filter(|row| row.disk.is_some())
            .count();
        tracing::info!(
            target: "placement.restore",
            on_disk,
            to_recompute = to_recompute.len(),
            committed_seq = manifest.committed_seq,
            next_seq = manifest.source_cursor.next_seq
        );
        let extras = CheckpointExtras {
            kernel_states: manifest
                .kernel_states
                .iter()
                .filter_map(|row| {
                    base64::engine::general_purpose::STANDARD
                        .decode(&row.state)
                        .ok()
                        .map(|state| (row.stage, row.instance as usize, state))
                })
                .collect(),
            sink_state: manifest
                .sink_state
                .as_ref()
                .and_then(|state| base64::engine::general_purpose::STANDARD.decode(state).ok()),
            committed_seq: manifest.committed_seq,
            source_cursor: SourceCursor {
                split_index: manifest.source_cursor.split_index,
                row_offset: manifest.source_cursor.row_offset,
                next_seq: manifest.source_cursor.next_seq,
            },
            issued,
        };
        Ok(ResumePoint {
            extras,
            to_recompute,
        })
    }
}

/// One dotted key of the resolved configuration table, as the manifest records it (e.5).
fn config_at(config: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    let mut cursor = config;
    for part in key.split('.') {
        cursor = cursor.get(part)?;
    }
    Some(cursor.clone())
}
