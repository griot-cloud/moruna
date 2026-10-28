//! The local-directory adapter of the observation store (section 7; SDD 24 e.5, f.4).
//!
//! ```text
//! <root>/observations/<fnv-1a 64 of the key, 16 hex>/<seq, 16 hex>.seg   records, append-only
//! <root>/observations/<...>/summary.json                                 retired records, summarised
//! <root>/observations/<...>/compact.lock                                 held while retiring
//! <root>/models/<name>@<version>.state                                   fitted state
//! ```
//!
//! A key's records go to its newest segment; a segment is closed after a quarter of the
//! retention, and once a key holds a retention and a segment, its oldest whole segments are
//! folded into its summary and deleted. Appends from several processes land in one segment
//! through `O_APPEND`, one write per record; a record cut short by a crash is skipped and
//! counted, and the records after it are read.

use std::collections::btree_map::Entry as TreeEntry;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::frame;
use super::{History, Observation, ObservationSource, ObservationStore, Retention, Summary};
use crate::error::{CostError, Result};
use crate::inputs::INPUTS_VERSION;
use crate::key::CostKey;
use crate::model::ModelId;

const OBSERVATIONS: &str = "observations";
const MODELS: &str = "models";
const SEGMENT: &str = "seg";
const SUMMARY: &str = "summary.json";
const LOCK: &str = "compact.lock";
/// Segments a retention is split into: a key holds at most a retention and one segment.
const SEGMENTS_PER_RETENTION: usize = 4;
/// A compaction lock older than this was left by a process that died holding it.
const LOCK_STALE: Duration = Duration::from_secs(60);
/// How often a scan starts again when a segment it listed was retired before it read it.
const SCAN_ATTEMPTS: usize = 3;

/// Distinguishes the temporary files of concurrent writers in one process.
static TEMPS: AtomicU64 = AtomicU64::new(0);

/// An [`ObservationStore`] in a local directory: today's `profiles_dir`, generalised.
pub struct LocalStore {
    root: PathBuf,
    retention: Retention,
    live: Mutex<HashMap<PathBuf, Live>>,
}

/// What this process knows of one key directory's live segments.
struct Live {
    retired_through: u64,
    /// Records per live segment, by sequence number.
    segments: BTreeMap<u64, usize>,
}

/// The summary file: every key's retired records, and the newest segment retired, so a crash
/// between writing it and deleting the segment counts nothing twice.
#[derive(Default, Serialize, Deserialize)]
struct SummaryFile {
    retired_through: u64,
    summaries: Vec<Summary>,
}

impl LocalStore {
    /// A store in `root` with the default retention, creating the directories it needs.
    pub fn open(root: impl Into<PathBuf>) -> Result<LocalStore> {
        LocalStore::with_retention(root, Retention::default())
    }

    /// A store in `root` keeping `retention`.
    pub fn with_retention(root: impl Into<PathBuf>, retention: Retention) -> Result<LocalStore> {
        let root = root.into();
        for sub in [OBSERVATIONS, MODELS] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).map_err(|e| CostError::io(&dir, e))?;
        }
        Ok(LocalStore {
            root,
            retention: Retention {
                per_key: retention.per_key.max(1),
            },
            live: Mutex::new(HashMap::new()),
        })
    }

    /// The directory the store is in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The retention it keeps.
    pub fn retention(&self) -> Retention {
        self.retention
    }

    fn key_dir(&self, key: &CostKey) -> PathBuf {
        self.root
            .join(OBSERVATIONS)
            .join(format!("{:016x}", fnv1a(key.canonical().as_bytes())))
    }

    fn segment_cap(&self) -> usize {
        (self.retention.per_key / SEGMENTS_PER_RETENTION).max(1)
    }

    fn state_path(&self, model: &ModelId) -> Result<PathBuf> {
        if !moruna_job::is_kind_name(&model.name) {
            return Err(CostError::BadName {
                name: model.name.clone(),
            });
        }
        Ok(self
            .root
            .join(MODELS)
            .join(format!("{}@{}.state", model.name, model.version)))
    }

    /// Retire a directory's oldest whole segments into its summary while it holds more than a
    /// retention, under the directory's lock. Another process holding the lock retires them
    /// instead; a scan never returns more than the retention either way.
    fn compact(&self, dir: &Path, live: &mut Live) -> Result<()> {
        let Some(_lock) = Lock::take(dir)? else {
            return Ok(());
        };
        let mut file = read_summary(dir)?;
        let mut counts = BTreeMap::new();
        for (seq, path) in live_segments(dir, file.retired_through)? {
            let bytes = match read(&path)? {
                Some(bytes) => bytes,
                None => continue,
            };
            counts.insert(seq, (path, frame::decode(&bytes).records.len()));
        }
        let newest = counts.keys().next_back().copied();
        let mut total: usize = counts.values().map(|(_, n)| n).sum();
        let mut retire = Vec::new();
        for (seq, (path, n)) in &counts {
            if Some(*seq) == newest || total - n < self.retention.per_key {
                break;
            }
            total -= n;
            retire.push((*seq, path.clone()));
        }
        for (seq, path) in &retire {
            let (observations, _) = read_observations(path)?;
            for observation in &observations {
                match file.summaries.iter_mut().find(|s| s.key == observation.key) {
                    Some(summary) => summary.fold(observation),
                    None => {
                        let mut summary = Summary::new(observation.key.clone());
                        summary.fold(observation);
                        file.summaries.push(summary);
                    }
                }
            }
            file.retired_through = *seq;
            counts.remove(seq);
        }
        if !retire.is_empty() {
            let text = serde_json::to_vec(&file).map_err(|e| CostError::Unreadable {
                path: dir.join(SUMMARY),
                offset: 0,
                reason: format!("summary not written: {e}"),
            })?;
            write_atomic(&dir.join(SUMMARY), &text)?;
            for (_, path) in &retire {
                remove(path)?;
            }
        }
        live.retired_through = file.retired_through;
        live.segments = counts.into_iter().map(|(seq, (_, n))| (seq, n)).collect();
        Ok(())
    }

    /// One directory's summary file and its live records, oldest first, with the torn count.
    /// A segment retired by another process between listing and reading starts the scan again.
    fn scan_dir(&self, dir: &Path) -> Result<(SummaryFile, Vec<Observation>, u64)> {
        let mut attempt = 0;
        'again: loop {
            attempt += 1;
            let file = read_summary(dir)?;
            let mut observations = Vec::new();
            let mut torn = 0;
            for (_, path) in live_segments(dir, file.retired_through)? {
                match read(&path)? {
                    Some(bytes) => {
                        let (found, t) = decode_observations(&path, &bytes)?;
                        observations.extend(found);
                        torn += t;
                    }
                    None if attempt < SCAN_ATTEMPTS => continue 'again,
                    None => {
                        return Err(CostError::io(
                            &path,
                            std::io::Error::new(
                                ErrorKind::NotFound,
                                "segments kept being retired while this scan read them",
                            ),
                        ));
                    }
                }
            }
            return Ok((file, observations, torn));
        }
    }

    /// Keep the last `per_key` records and fold the older ones into the summary.
    fn trim(&self, key: &CostKey, summary: &mut Option<Summary>, kept: &mut Vec<Observation>) {
        if kept.len() > self.retention.per_key {
            let excess = kept.len() - self.retention.per_key;
            let folded = summary.get_or_insert_with(|| Summary::new(key.clone()));
            for observation in kept.drain(..excess) {
                folded.fold(&observation);
            }
        }
    }
}

impl ObservationSource for LocalStore {
    fn scan(&self, key: &CostKey) -> Result<History> {
        let dir = self.key_dir(key);
        let (file, all, torn) = self.scan_dir(&dir)?;
        let mut summary = file.summaries.into_iter().find(|s| s.key == *key);
        let mut observations: Vec<Observation> =
            all.into_iter().filter(|o| o.key == *key).collect();
        self.trim(key, &mut summary, &mut observations);
        Ok(History {
            key: key.clone(),
            summary,
            observations,
            torn,
        })
    }

    fn scan_all(&self, each: &mut dyn FnMut(History) -> Result<()>) -> Result<()> {
        let root = self.root.join(OBSERVATIONS);
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(&root).map_err(|e| CostError::io(&root, e))? {
            let entry = entry.map_err(|e| CostError::io(&root, e))?;
            if entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        for dir in dirs {
            let (file, all, mut torn) = self.scan_dir(&dir)?;
            let mut by_key: BTreeMap<CostKey, (Option<Summary>, Vec<Observation>)> =
                BTreeMap::new();
            for summary in file.summaries {
                by_key.insert(summary.key.clone(), (Some(summary), Vec::new()));
            }
            for observation in all {
                match by_key.entry(observation.key.clone()) {
                    TreeEntry::Occupied(mut e) => e.get_mut().1.push(observation),
                    TreeEntry::Vacant(e) => {
                        e.insert((None, vec![observation]));
                    }
                }
            }
            for (key, (mut summary, mut observations)) in by_key {
                self.trim(&key, &mut summary, &mut observations);
                each(History {
                    key,
                    summary,
                    observations,
                    torn,
                })?;
                torn = 0;
            }
        }
        Ok(())
    }
}

impl ObservationStore for LocalStore {
    fn append(&self, observation: &Observation) -> Result<()> {
        observation.check().map_err(CostError::Invalid)?;
        let body = serde_json::to_vec(observation)
            .map_err(|e| CostError::Invalid(format!("not serialisable: {e}")))?;
        let record = frame::encode(&body)
            .ok_or_else(|| CostError::Invalid("a record longer than 4 GiB".into()))?;
        let dir = self.key_dir(&observation.key);
        std::fs::create_dir_all(&dir).map_err(|e| CostError::io(&dir, e))?;
        let mut held = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let live = match held.entry(dir.clone()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(load_live(&dir)?),
        };
        let cap = self.segment_cap();
        let (mut seq, count) = live
            .segments
            .last_key_value()
            .map(|(seq, n)| (*seq, *n))
            .unwrap_or((live.retired_through + 1, 0));
        if count >= cap {
            seq += 1;
        }
        let path = dir.join(segment_name(seq));
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| CostError::io(&path, e))?;
        out.write_all(&record)
            .map_err(|e| CostError::io(&path, e))?;
        *live.segments.entry(seq).or_insert(0) += 1;
        let total: usize = live.segments.values().sum();
        if total >= self.retention.per_key + cap {
            self.compact(&dir, live)?;
        }
        Ok(())
    }

    fn save_state(&self, model: &ModelId, state: &[u8]) -> Result<()> {
        write_atomic(&self.state_path(model)?, state)
    }

    fn load_state(&self, model: &ModelId) -> Result<Option<Vec<u8>>> {
        read(&self.state_path(model)?)
    }
}

/// The compaction lock of one directory, released when dropped.
struct Lock(PathBuf);

impl Lock {
    fn take(dir: &Path) -> Result<Option<Lock>> {
        let path = dir.join(LOCK);
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Some(Lock(path))),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| SystemTime::now().duration_since(at).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if !(stale && attempt == 0) {
                        return Ok(None);
                    }
                    remove(&path)?;
                }
                Err(e) => return Err(CostError::io(&path, e)),
            }
        }
        Ok(None)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Nothing to do with a failure here: the lock goes stale and the next compaction
        // takes it.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// FNV-1a, 64 bits: names a key's directory. Two keys that collide share a directory, and every
/// record carries its key, so a scan still returns only its own.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn segment_name(seq: u64) -> String {
    format!("{seq:016x}.{SEGMENT}")
}

fn load_live(dir: &Path) -> Result<Live> {
    let file = read_summary(dir)?;
    let mut segments = BTreeMap::new();
    for (seq, path) in live_segments(dir, file.retired_through)? {
        if let Some(bytes) = read(&path)? {
            segments.insert(seq, frame::decode(&bytes).records.len());
        }
    }
    Ok(Live {
        retired_through: file.retired_through,
        segments,
    })
}

/// The segments of a directory above `retired_through`, by sequence number; none when the
/// directory is not there.
fn live_segments(dir: &Path, retired_through: u64) -> Result<BTreeMap<u64, PathBuf>> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(CostError::io(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| CostError::io(dir, e))?;
        let name = entry.file_name();
        let Some(seq) = name
            .to_str()
            .and_then(|n| n.strip_suffix(&format!(".{SEGMENT}")))
            .and_then(|stem| u64::from_str_radix(stem, 16).ok())
        else {
            continue;
        };
        if seq > retired_through {
            out.insert(seq, entry.path());
        }
    }
    Ok(out)
}

fn read_summary(dir: &Path) -> Result<SummaryFile> {
    let path = dir.join(SUMMARY);
    match read(&path)? {
        None => Ok(SummaryFile::default()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| CostError::Unreadable {
            path,
            offset: 0,
            reason: format!("not a summary: {e}"),
        }),
    }
}

fn read_observations(path: &Path) -> Result<(Vec<Observation>, u64)> {
    match read(path)? {
        Some(bytes) => decode_observations(path, &bytes),
        None => Ok((Vec::new(), 0)),
    }
}

/// Every whole record of a segment as an observation. A record whose frame checks but whose
/// body is not an observation of this build is not a torn write: it is refused, loudly.
fn decode_observations(path: &Path, bytes: &[u8]) -> Result<(Vec<Observation>, u64)> {
    let decoded = frame::decode(bytes);
    let mut out = Vec::with_capacity(decoded.records.len());
    for (offset, body) in decoded.records {
        let unreadable = |reason: String| CostError::Unreadable {
            path: path.to_path_buf(),
            offset: offset as u64,
            reason,
        };
        let observation: Observation = serde_json::from_slice(body)
            .map_err(|e| unreadable(format!("not an observation: {e}")))?;
        if observation.inputs.version != INPUTS_VERSION {
            return Err(unreadable(format!(
                "inputs version {}; this build reads version {INPUTS_VERSION}",
                observation.inputs.version
            )));
        }
        out.push(observation);
    }
    Ok((out, decoded.torn))
}

/// A file's bytes, or `None` when it is not there.
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CostError::io(path, e)),
    }
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CostError::io(path, e)),
    }
}

/// Write a file whole: a temporary file named for this process, then a rename, so a reader
/// never sees half of one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let temp = path.with_file_name(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        TEMPS.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, bytes).map_err(|e| CostError::io(&temp, e))?;
    std::fs::rename(&temp, path).map_err(|e| CostError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_reference_values() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn a_stale_lock_is_taken_and_a_fresh_one_is_not() {
        let dir = std::env::temp_dir().join(format!("moruna-cost-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let first = Lock::take(&dir).expect("io").expect("free");
        assert!(Lock::take(&dir).expect("io").is_none(), "held");
        drop(first);
        let again = Lock::take(&dir).expect("io");
        assert!(again.is_some(), "released on drop");
        drop(again);
        std::fs::remove_dir_all(&dir).ok();
    }
}
