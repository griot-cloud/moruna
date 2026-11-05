//! Durable local launch evidence. Missing, partial or corrupt records never prove
//! absence. This is not a regional fence or permission to redispatch a run.
use moruna_kernel::RunId;
use moruna_vmm::error::{Result, VmmError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
fn bad(s: impl Into<String>) -> VmmError {
    VmmError::config("launch witness", s)
}
/// Caller-owned fencing identity, distinct from host OS/process identity.
pub struct Admission<'a> {
    /// Canonical assigned run (hex in persisted keys).
    pub run_id: RunId,
    /// Caller fencing identity, never an OS/process identity.
    pub node_boot: RunId,
    /// Caller-authorized attempt number.
    pub attempt: u32,
    /// Explicit existing absolute durable state directory.
    pub directory: &'a Path,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Exact canonical launch correlation key; no regional scheduling authority.
pub struct Key {
    /// Canonical assigned run (hex in persisted keys).
    pub run_id: String,
    /// Existing canonical job_digest result.
    pub spec_digest: String,
    /// Canonical Plan::content_id, not a second hash.
    pub host_plan_digest: String,
    /// Caller fencing identity, never an OS/process identity.
    pub node_boot: String,
    /// Caller-authorized attempt number.
    pub attempt: u32,
}
impl Key {
    pub(crate) fn checked(admission: &Admission<'_>, spec: String, plan: String) -> Self {
        Self {
            run_id: admission.run_id.to_hex(),
            spec_digest: spec,
            host_plan_digest: plan,
            node_boot: admission.node_boot.to_hex(),
            attempt: admission.attempt,
        }
    }
    pub(crate) fn matches_admission(&self, admission: &Admission<'_>) -> bool {
        self.run_id == admission.run_id.to_hex()
            && self.node_boot == admission.node_boot.to_hex()
            && self.attempt == admission.attempt
    }
}
/// Validate the real directory before preparation or an exclusive claim.
pub(crate) fn checked_directory(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(bad("explicit absolute durable directory required"));
    }
    let named = fs::symlink_metadata(path).map_err(|e| bad(e.to_string()))?;
    if !named.is_dir() || named.file_type().is_symlink() {
        return Err(bad("durable directory must be an existing real directory"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| bad(e.to_string()))?;
    let actual = file.metadata().map_err(|e| bad(e.to_string()))?;
    if !actual.is_dir() || actual.dev() != named.dev() || actual.ino() != named.ino() {
        return Err(bad("durable directory changed while opening"));
    }
    Ok(file)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Last durable observation; intermediate states are never launch finality.
pub enum Phase {
    /// Durable intent before any potential process spawn.
    PotentialDispatch,
    /// Actual launcher returned owned running processes.
    Launched,
    /// Real guest protocol Hello accepted.
    Hello,
    /// Actual guest exit received; node cleanup remains owed.
    GuestExit(i32),
    /// Guest exit plus actual owned child reaps durably recorded.
    Exited(i32),
    /// Explicit cancellation and actual owned reaps durably recorded.
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Versioned launch evidence bound to one canonical key.
pub struct Record {
    /// Witness format major; currently one.
    pub version: u32,
    /// Exact run/spec/plan/boot/attempt binding.
    pub key: Key,
    /// Last observed lifecycle evidence.
    pub phase: Phase,
    /// Actual observed Linux host boot identity; never used to signal a PID.
    pub host_boot: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    record: Record,
    previous: [u8; 32],
    checksum: [u8; 32],
}
fn checksum(record: &Record, previous: &[u8; 32]) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(record).map_err(|e| bad(e.to_string()))?;
    let mut hash = Sha256::new();
    hash.update(b"moruna.launch.witness.1\0");
    hash.update(previous);
    hash.update(bytes);
    let mut result = [0; 32];
    result.copy_from_slice(&hash.finalize());
    Ok(result)
}
#[derive(Debug)]
/// Conservative durable readback after possible ownership loss.
pub enum Probe {
    /// Includes last evidence, but never asserts present ownership or absence.
    Unknown(Option<Record>),
    /// Verified final exit record.
    Exited(Record),
    /// Verified final cancellation record.
    Cancelled(Record),
}
/// Durable record owner. Only a live NativeRun with actual owned Child handles
/// may append lifecycle evidence; disk readback cannot reacquire process ownership.
pub(crate) struct Witness {
    file: File,
    directory: File,
    directory_path: PathBuf,
    path: PathBuf,
    record: Record,
    poisoned: bool,
    checksum: [u8; 32],
}
impl Witness {
    #[cfg(test)]
    pub(crate) fn reserve(admission: &Admission<'_>, spec: String, plan: String) -> Result<Self> {
        let directory = checked_directory(admission.directory)?;
        Self::reserve_prepared(admission, Key::checked(admission, spec, plan), directory)
    }
    pub(crate) fn reserve_prepared(
        admission: &Admission<'_>,
        key: Key,
        directory: File,
    ) -> Result<Self> {
        if !key.matches_admission(admission) {
            return Err(bad("prepared key differs from admission"));
        }
        let named = checked_directory(admission.directory)?;
        let actual = directory.metadata().map_err(|e| bad(e.to_string()))?;
        let current = named.metadata().map_err(|e| bad(e.to_string()))?;
        if actual.dev() != current.dev() || actual.ino() != current.ino() {
            return Err(bad("prepared durable directory was replaced"));
        }
        let path = path(admission.directory, admission.run_id);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| {
                bad(format!(
                    "exclusive run claim refused (existing/unknown claims never redispatch): {e}"
                ))
            })?;
        file.try_lock()
            .map_err(|e| bad(format!("claim lock: {e}")))?;
        let record = Record {
            version: 1,
            key,
            phase: Phase::PotentialDispatch,
            host_boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|s| s.trim().into()),
        };
        let checksum = append(&mut file, &record, [0; 32])?;
        directory
            .sync_all()
            .map_err(|e| bad(format!("claim directory sync: {e}")))?;
        let witness = Self {
            file,
            directory,
            directory_path: admission.directory.into(),
            path,
            record,
            poisoned: false,
            checksum,
        };
        if !witness.healthy() {
            return Err(bad("new durable witness publication lost identity"));
        }
        Ok(witness)
    }
    pub(crate) fn advance(&mut self, phase: Phase) -> Result<()> {
        if !self.healthy() {
            self.poisoned = true;
            return Err(bad(
                "prior durable write failed; ownership evidence is uncertain",
            ));
        }
        let allowed = transition(&self.record.phase, &phase);
        if !allowed {
            return Err(bad("invalid evidence transition"));
        }
        let mut record = self.record.clone();
        record.phase = phase;
        match append(&mut self.file, &record, self.checksum) {
            Ok(checksum) => self.checksum = checksum,
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        }
        self.record = record;
        if !self.healthy() {
            self.poisoned = true;
            return Err(bad(
                "durable witness path or journal changed during publication",
            ));
        }
        Ok(())
    }
    pub(crate) fn healthy(&self) -> bool {
        if self.poisoned {
            return false;
        }
        let identities = || -> std::io::Result<bool> {
            let file = self.file.metadata()?;
            let named = fs::symlink_metadata(&self.path)?;
            let directory = self.directory.metadata()?;
            let named_directory = fs::symlink_metadata(&self.directory_path)?;
            Ok(file.is_file()
                && named.is_file()
                && file.nlink() == 1
                && file.dev() == named.dev()
                && file.ino() == named.ino()
                && named_directory.is_dir()
                && directory.dev() == named_directory.dev()
                && directory.ino() == named_directory.ino())
        };
        if !matches!(identities(), Ok(true)) {
            return false;
        }
        match probe(&self.directory_path, &self.record.key) {
            Probe::Unknown(Some(record)) | Probe::Exited(record) | Probe::Cancelled(record) => {
                record == self.record
            }
            Probe::Unknown(None) => false,
        }
    }
    pub(crate) fn record(&self) -> Record {
        self.record.clone()
    }
    pub(crate) fn key(&self) -> &Key {
        &self.record.key
    }
}
fn path(directory: &Path, run: RunId) -> PathBuf {
    directory.join(format!("{}.launch", run.to_hex()))
}
fn append(file: &mut File, record: &Record, previous: [u8; 32]) -> Result<[u8; 32]> {
    let checksum = checksum(record, &previous)?;
    let entry = Entry {
        record: record.clone(),
        previous,
        checksum,
    };
    let mut bytes = serde_json::to_vec(&entry).map_err(|e| bad(e.to_string()))?;
    bytes.push(b'\n');
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| bad(format!("durable evidence write: {e}")))?;
    Ok(checksum)
}
/// Later probes never interpret missing/corrupt evidence as NotDispatched.
/// Only terminal evidence written after actual owned-process reaps is final.
pub fn probe(directory: &Path, expected: &Key) -> Probe {
    let Some(run) = RunId::from_hex(&expected.run_id) else {
        return Probe::Unknown(None);
    };
    let read = || -> Option<Record> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path(directory, run))
            .ok()?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(65537)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.is_empty() || bytes.len() > 65536 || bytes.last() != Some(&b'\n') {
            return None;
        }
        let mut last: Option<Record> = None;
        let mut previous = [0; 32];
        for line in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
            let entry: Entry = serde_json::from_slice(line).ok()?;
            if entry.previous != previous
                || checksum(&entry.record, &previous).ok()? != entry.checksum
            {
                return None;
            }
            previous = entry.checksum;
            let record = entry.record;
            if record.version != 1
                || &record.key != expected
                || last
                    .as_ref()
                    .map_or(record.phase != Phase::PotentialDispatch, |old| {
                        !transition(&old.phase, &record.phase)
                    })
            {
                return None;
            }
            last = Some(record);
        }
        last
    };
    match read() {
        Some(r) if matches!(r.phase, Phase::Exited(_)) => Probe::Exited(r),
        Some(r) if r.phase == Phase::Cancelled => Probe::Cancelled(r),
        other => Probe::Unknown(other),
    }
}

fn transition(old: &Phase, next: &Phase) -> bool {
    match (old, next) {
        (Phase::PotentialDispatch, Phase::Launched)
        | (Phase::Launched, Phase::Hello)
        | (Phase::Hello, Phase::GuestExit(_)) => true,
        (Phase::GuestExit(a), Phase::Exited(b)) => a == b,
        (
            Phase::PotentialDispatch | Phase::Launched | Phase::Hello | Phase::GuestExit(_),
            Phase::Cancelled,
        ) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "moruna-launch-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
        fn admission(&self) -> Admission<'_> {
            Admission {
                run_id: RunId([1; 16]),
                node_boot: RunId([2; 16]),
                attempt: 3,
                directory: &self.0,
            }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn crash_windows_never_authorize_redispatch_or_infer_absence() {
        let dir = Scratch::new();
        let admission = dir.admission();
        let mut owner = Witness::reserve(&admission, "spec".into(), "plan".into()).unwrap();
        let key = owner.key().clone();
        assert!(matches!(
            probe(&dir.0, &key),
            Probe::Unknown(Some(Record {
                phase: Phase::PotentialDispatch,
                ..
            }))
        ));
        assert!(Witness::reserve(&admission, "spec".into(), "plan".into()).is_err());
        owner.advance(Phase::Launched).unwrap();
        drop(owner);
        assert!(matches!(
            probe(&dir.0, &key),
            Probe::Unknown(Some(Record {
                phase: Phase::Launched,
                ..
            }))
        ));
        assert!(Witness::reserve(&admission, "spec".into(), "plan".into()).is_err());
        let mut foreign = key.clone();
        foreign.node_boot = RunId([3; 16]).to_hex();
        assert!(matches!(probe(&dir.0, &foreign), Probe::Unknown(None)));
        fs::remove_file(path(&dir.0, admission.run_id)).unwrap();
        assert!(matches!(probe(&dir.0, &key), Probe::Unknown(None)));
    }
    #[test]
    fn partial_corrupt_or_unearned_terminal_evidence_is_unknown() {
        let dir = Scratch::new();
        let admission = dir.admission();
        let mut owner = Witness::reserve(&admission, "spec".into(), "plan".into()).unwrap();
        let key = owner.key().clone();
        assert!(owner.advance(Phase::Exited(0)).is_err());
        owner.advance(Phase::Launched).unwrap();
        owner.advance(Phase::Hello).unwrap();
        owner.advance(Phase::GuestExit(7)).unwrap();
        assert!(owner.advance(Phase::Exited(0)).is_err());
        owner.advance(Phase::Exited(7)).unwrap();
        drop(owner);
        assert!(matches!(
            probe(&dir.0, &key),
            Probe::Exited(Record {
                phase: Phase::Exited(7),
                ..
            })
        ));
        let file = path(&dir.0, admission.run_id);
        let original = fs::read(&file).unwrap();
        let split = original.iter().position(|b| *b == b'\n').unwrap();
        let mut changed: serde_json::Value = serde_json::from_slice(&original[..split]).unwrap();
        changed["record"]["host_boot"] = serde_json::json!("valid-json-corruption");
        let mut mutated = serde_json::to_vec(&changed).unwrap();
        mutated.extend_from_slice(&original[split..]);
        fs::write(&file, mutated).unwrap();
        assert!(matches!(probe(&dir.0, &key), Probe::Unknown(None)));
        fs::write(&file, original).unwrap();
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"{\"version\":").unwrap();
        f.sync_all().unwrap();
        assert!(matches!(probe(&dir.0, &key), Probe::Unknown(None)));
        fs::write(file, b"corrupt\n").unwrap();
        assert!(matches!(probe(&dir.0, &key), Probe::Unknown(None)));
    }
    #[test]
    fn original_validation_refusal_has_no_dispatch_claim_or_spawn() {
        let dir = Scratch::new();
        let result = super::super::native::NativeRun::start(
            dir.admission(),
            Path::new("no-descriptor"),
            moruna_vmm::Budget {
                memory_bytes: 1,
                memory_max_bytes: 1,
                cpus: 1,
                cpus_max: 1,
            },
            &dir.0.join("never-created-runtime"),
            super::super::native::Tools {
                qemu: Path::new("no-qemu"),
                vsock: Path::new("no-helper"),
                cgroup: &moruna_vmm::jail::Cgroup::Undelegated,
            },
            super::super::protocol::Inbound::Cancel,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        match result {
            Err(error) => assert_eq!(
                error.disposition,
                super::super::native::StartDisposition::NotDispatched
            ),
            Ok(_) => panic!("invalid Spec cannot launch"),
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }
    #[test]
    fn simultaneous_original_calls_have_only_one_exclusive_claim() {
        let dir = Scratch::new();
        let admission = dir.admission();
        let winners = std::thread::scope(|scope| {
            let one =
                scope.spawn(|| Witness::reserve(&admission, "spec".into(), "plan".into()).is_ok());
            let two =
                scope.spawn(|| Witness::reserve(&admission, "spec".into(), "plan".into()).is_ok());
            usize::from(one.join().unwrap()) + usize::from(two.join().unwrap())
        });
        assert_eq!(winners, 1);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }
    #[test]
    fn active_owner_cannot_publish_through_replaced_or_corrupt_claim() {
        for replace in [true, false] {
            let dir = Scratch::new();
            let admission = dir.admission();
            let mut owner = Witness::reserve(&admission, "spec".into(), "plan".into()).unwrap();
            let named = path(&dir.0, admission.run_id);
            let original = fs::read(&named).unwrap();
            assert!(owner.healthy());
            if replace {
                fs::remove_file(&named).unwrap();
                fs::write(&named, &original).unwrap();
            } else {
                fs::write(&named, b"corrupt\n").unwrap();
            }
            assert!(!owner.healthy());
            assert!(owner.advance(Phase::Launched).is_err());
            // Restoring bytes cannot unpoison ownership after an integrity failure.
            fs::write(&named, &original).unwrap();
            assert!(!owner.healthy());
            assert!(owner.advance(Phase::Launched).is_err());
        }
    }
}
