//! Governed observation-store adapter. The cost crate owns record/model codecs and retention;
//! storage owns authorization, immutable data and conditional visibility. One owned IO thread
//! bridges the synchronous model port without blocking a hot scheduler worker or nesting Tokio.
use moruna_cost::{
    CostError, CostKey, History, ModelId, Observation, ObservationSource, ObservationStore,
    Retention,
};
use moruna_kernel::{Allocator, Tier};
use moruna_storage::{Access, Condition, Governed, Metadata, Object};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};

/// Explicit finite admission for the native cost store, not a backend capacity promise.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum immutable records one scan will admit, before retaining/summarising them.
    pub records: usize,
    /// Maximum complete observation or fitted model state bytes.
    pub object_bytes: usize,
    /// Maximum total encoded history bytes read by one fit/scan.
    pub scan_bytes: u64,
    /// Maximum requests queued behind the one owned IO operation.
    pub queue: usize,
    /// Finite whole-operation deadline, independently enforced by native transport too.
    pub timeout: Duration,
    /// Existing raw-record retention semantics.
    pub retention: Retention,
}
fn failed(reason: impl Into<String>) -> CostError {
    CostError::Invalid(format!("governed cost store: {}", reason.into()))
}
fn storage(error: moruna_storage::Error) -> CostError {
    failed(error.to_string())
}
#[derive(Clone)]
enum Work {
    Append(Box<Observation>),
    Save(ModelId, Vec<u8>),
    Load(ModelId),
    Scan(Option<CostKey>),
}
enum Answer {
    Done,
    State(Option<Vec<u8>>),
    History(Vec<History>),
}
struct Request {
    work: Work,
    reply: mpsc::SyncSender<moruna_cost::Result<Answer>>,
}
/// The sole governed cost history configured for registration and live learning.
pub struct Store {
    home: Arc<Governed>,
    access: Arc<Access>,
    alloc: Arc<dyn Allocator>,
    root: Object,
    tx: mpsc::SyncSender<Request>,
    limits: Limits,
}
struct Owner {
    home: Arc<Governed>,
    access: Arc<Access>,
    alloc: Arc<dyn Allocator>,
    root: Object,
    limits: Limits,
    expected: BTreeMap<String, Option<Metadata>>,
}
impl Store {
    /// Bind real authority and start its one serialized deadline-bound IO owner. No directory,
    /// home, token, model or fallback store is selected implicitly.
    pub fn open(
        home: Arc<Governed>,
        access: Arc<Access>,
        alloc: Arc<dyn Allocator>,
        root: Object,
        limits: Limits,
    ) -> moruna_cost::Result<Self> {
        root.validate().map_err(|error| failed(error.to_string()))?;
        if root.key.is_empty()
            || limits.records == 0
            || limits.object_bytes == 0
            || limits.scan_bytes == 0
            || limits.queue == 0
            || limits.timeout.is_zero()
            || limits.retention.per_key == 0
        {
            return Err(failed(
                "positive explicit namespace/admission limits required",
            ));
        }
        let (tx, rx) = mpsc::sync_channel::<Request>(limits.queue);
        let mut owner = Owner {
            home: home.clone(),
            access: access.clone(),
            alloc: alloc.clone(),
            root: root.clone(),
            limits: limits.clone(),
            expected: BTreeMap::new(),
        };
        let (ready, started) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("moruna-governed-cost".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let runtime = match runtime {
                    Ok(runtime) => {
                        let _ = ready.send(Ok(()));
                        runtime
                    }
                    Err(error) => {
                        let _ = ready.send(Err(failed(error.to_string())));
                        return;
                    }
                };
                while let Ok(request) = rx.recv() {
                    let result = runtime.block_on(async {
                        tokio::time::timeout(owner.limits.timeout, owner.execute(request.work))
                            .await
                            .map_err(|_| {
                                failed(
                                    "operation deadline elapsed; publication is not acknowledged",
                                )
                            })?
                    });
                    let _ = request.reply.send(result);
                }
            })
            .map_err(|error| failed(error.to_string()))?;
        started
            .recv_timeout(limits.timeout)
            .map_err(|_| failed("IO owner did not initialize within deadline"))??;
        Ok(Self {
            tx,
            limits,
            home,
            access,
            alloc,
            root,
        })
    }
    fn call(&self, work: Work) -> moruna_cost::Result<Answer> {
        let (reply, result) = mpsc::sync_channel(1);
        self.tx
            .try_send(Request { work, reply })
            .map_err(|_| failed("IO queue unavailable/full; no request admission"))?;
        // No worker holds a queue, model or placement mutex while this adapter performs IO.
        // A timed-out result is unacknowledged, not proof that the remote write never landed.
        result
            .recv_timeout(self.limits.timeout)
            .map_err(|_| failed("IO acknowledgement deadline elapsed"))?
    }
}
impl ObservationStore for Store {
    fn fitting_store(&self) -> moruna_cost::Result<Option<Arc<dyn ObservationStore>>> {
        Ok(Some(Arc::new(Self::open(
            self.home.clone(),
            self.access.clone(),
            self.alloc.clone(),
            self.root.clone(),
            self.limits.clone(),
        )?)))
    }
    fn append(&self, observation: &Observation) -> moruna_cost::Result<()> {
        observation.check().map_err(CostError::Invalid)?;
        match self.call(Work::Append(Box::new(observation.clone())))? {
            Answer::Done => Ok(()),
            _ => Err(failed("append response type")),
        }
    }
    fn save_state(&self, model: &ModelId, state: &[u8]) -> moruna_cost::Result<()> {
        if state.len() > self.limits.object_bytes {
            return Err(failed("model state exceeds admitted object bytes"));
        }
        match self.call(Work::Save(model.clone(), state.to_vec()))? {
            Answer::Done => Ok(()),
            _ => Err(failed("state response type")),
        }
    }
    fn load_state(&self, model: &ModelId) -> moruna_cost::Result<Option<Vec<u8>>> {
        match self.call(Work::Load(model.clone()))? {
            Answer::State(state) => Ok(state),
            _ => Err(failed("load response type")),
        }
    }
}
impl ObservationSource for Store {
    fn scan(&self, key: &CostKey) -> moruna_cost::Result<History> {
        match self.call(Work::Scan(Some(key.clone())))? {
            Answer::History(mut history) => {
                history.pop().ok_or_else(|| failed("scan response absent"))
            }
            _ => Err(failed("scan response type")),
        }
    }
    fn scan_all(
        &self,
        each: &mut dyn FnMut(History) -> moruna_cost::Result<()>,
    ) -> moruna_cost::Result<()> {
        let Answer::History(history) = self.call(Work::Scan(None))? else {
            return Err(failed("scan response type"));
        };
        for item in history {
            each(item)?;
        }
        Ok(())
    }
}
impl Owner {
    fn object(&self, suffix: &str) -> Object {
        Object {
            key: format!("{}/{suffix}", self.root.key.trim_end_matches('/')),
            ..self.root.clone()
        }
    }
    fn tier(&self) -> Tier {
        if self.alloc.is_pinned() {
            Tier::PinnedHost
        } else {
            Tier::Host
        }
    }
    fn buffer(&self, bytes: &[u8]) -> moruna_cost::Result<moruna_kernel::Buffer> {
        if bytes.len() > self.limits.object_bytes {
            return Err(failed("encoded object exceeds admission"));
        }
        let mut buffer = self
            .alloc
            .alloc(bytes.len(), self.tier())
            .map_err(|error| failed(error.to_string()))?;
        buffer.as_mut().copy_from_slice(bytes);
        Ok(buffer)
    }
    async fn latest(&self, object: &Object) -> moruna_cost::Result<Option<Metadata>> {
        match self.home.current(&self.access, object).await {
            Ok(metadata) => Ok(Some(metadata)),
            Err(moruna_storage::Error::Missing) => Ok(None),
            Err(error) => Err(storage(error)),
        }
    }

    async fn read(&self, metadata: &Metadata) -> moruna_cost::Result<Vec<u8>> {
        let len = usize::try_from(metadata.bytes).map_err(|_| failed("object length width"))?;
        if len > self.limits.object_bytes {
            return Err(failed("object length exceeds admission"));
        }
        let actual = self
            .home
            .head(&self.access, &metadata.object, &metadata.version)
            .await
            .map_err(storage)?;
        if actual != *metadata {
            return Err(failed("immutable object metadata changed"));
        }
        let destination = self
            .alloc
            .alloc(len, self.tier())
            .map_err(|error| failed(error.to_string()))?;
        let bytes = self
            .home
            .read(
                &self.access,
                &metadata.object,
                &metadata.version,
                0,
                destination,
            )
            .await
            .map_err(storage)?;
        Ok(bytes.as_ref().to_vec())
    }
    async fn execute(&mut self, work: Work) -> moruna_cost::Result<Answer> {
        match work {
            Work::Append(observation) => {
                let bytes = moruna_cost::encode_observation(&observation)?;
                let prefix = self.object(&format!(
                    "observations/{}/",
                    moruna_cost::key_partition(&observation.key)
                ));
                let current = self
                    .home
                    .list(&self.access, &prefix)
                    .await
                    .map_err(storage)?;
                if current.len() >= self.limits.records {
                    return Err(failed(
                        "immutable record ceiling reached; explicit compaction/lifecycle required",
                    ));
                }
                let mut nonce = [0; 16];
                getrandom::fill(&mut nonce).map_err(|error| failed(error.to_string()))?;
                let object = self.object(&format!(
                    "observations/{}/{:016x}-{}.seg",
                    moruna_cost::key_partition(&observation.key),
                    observation.at_ms,
                    moruna_kernel::RunId(nonce).to_hex()
                ));
                let bytes = Arc::new(self.buffer(&bytes)?);
                self.home
                    .write(&self.access, &object, bytes.view())
                    .await
                    .map_err(storage)?;
                Ok(Answer::Done)
            }
            Work::Save(model, state) => {
                let object = self.object(&format!(
                    "models/{}",
                    moruna_cost::model_state_name(&model)?
                ));
                let key = moruna_cost::model_state_name(&model)?;
                let expected = self.expected.get(&key).ok_or_else(|| {
                    failed("model state must be loaded before conditional publication")
                })?;
                let condition = expected.as_ref().map_or(Condition::Absent, |metadata| {
                    Condition::Match(metadata.version.clone())
                });
                let bytes = Arc::new(self.buffer(&state)?);
                let acknowledged = self
                    .home
                    .replace(&self.access, &object, condition, bytes.view())
                    .await
                    .map_err(storage)?;
                self.expected.insert(key, Some(acknowledged));
                Ok(Answer::Done)
            }
            Work::Load(model) => {
                let object = self.object(&format!(
                    "models/{}",
                    moruna_cost::model_state_name(&model)?
                ));
                let metadata = self.latest(&object).await?;
                let state = match &metadata {
                    Some(metadata) => Some(self.read(metadata).await?),
                    None => None,
                };
                self.expected
                    .insert(moruna_cost::model_state_name(&model)?, metadata);
                Ok(Answer::State(state))
            }
            Work::Scan(key) => {
                let prefix = self.object(&match &key {
                    Some(key) => format!("observations/{}/", moruna_cost::key_partition(key)),
                    None => "observations/".into(),
                });
                let entries = self
                    .home
                    .list(&self.access, &prefix)
                    .await
                    .map_err(storage)?;
                if entries.len() > self.limits.records {
                    return Err(failed("scan immutable record ceiling"));
                }
                let total = entries
                    .iter()
                    .try_fold(0u64, |total, entry| total.checked_add(entry.bytes))
                    .ok_or_else(|| failed("scan byte overflow"))?;
                if total > self.limits.scan_bytes {
                    return Err(failed("scan encoded-byte ceiling"));
                }
                let mut grouped: BTreeMap<CostKey, Vec<(String, Observation)>> = BTreeMap::new();
                let mut torn = 0;
                for entry in entries {
                    let bytes = self.read(&entry).await?;
                    let (observations, damaged) =
                        moruna_cost::decode_observations(Path::new(&entry.object.key), &bytes)?;
                    // An immutable publication is whole. A torn object is corruption, not a
                    // tolerated in-progress append as in the local O_APPEND adapter.
                    if damaged != 0 {
                        return Err(failed("immutable observation frame was torn"));
                    }
                    torn += damaged;
                    for observation in observations {
                        if key.as_ref().is_some_and(|key| key != &observation.key) {
                            continue;
                        }
                        grouped
                            .entry(observation.key.clone())
                            .or_default()
                            .push((entry.object.key.clone(), observation));
                    }
                }
                if let Some(key) = key {
                    grouped.entry(key).or_default();
                }
                let mut histories = Vec::with_capacity(grouped.len());
                for (key, mut records) in grouped {
                    records.sort_by(|(an, a), (bn, b)| a.at_ms.cmp(&b.at_ms).then(an.cmp(bn)));
                    let mut observations = records
                        .into_iter()
                        .map(|(_, observation)| observation)
                        .collect();
                    let mut summary = None;
                    moruna_cost::trim_history(
                        &key,
                        &mut summary,
                        &mut observations,
                        self.limits.retention,
                    );
                    histories.push(History {
                        key,
                        summary,
                        observations,
                        torn,
                    });
                    torn = 0;
                }
                Ok(Answer::History(histories))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_cost::{Digest, Inputs, Origin, Outcome, Target};
    use moruna_storage::{Action, Authorizer, Credential, Operation, Role, local::FileStore};
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
                    && access.workspace == "cost-fixture"
                    && object.workspace == "cost-fixture"
                    && access.credential.application() == Some(&b"TEST-ONLY"[..])
                {
                    Ok(())
                } else {
                    Err(moruna_storage::Error::Denied)
                }
            })
        }
    }
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn fixture() -> (Scratch, Arc<Governed>, Arc<Access>, Arc<Verifier>, Object) {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().join(format!(
            "moruna-governed-cost-{}",
            moruna_kernel::RunId(nonce).to_hex()
        ));
        let verifier = Arc::new(Verifier(AtomicBool::new(true)));
        // Actual durable-format/reopen conformance only, no physical bucket/device claim.
        let home = Arc::new(
            Governed::new(
                Arc::new(
                    FileStore::open(
                        root.clone(),
                        "cost-fixture".into(),
                        "home".into(),
                        Role::FormatProof,
                    )
                    .unwrap(),
                ),
                verifier.clone(),
            )
            .unwrap(),
        );
        let access = Arc::new(Access {
            workspace: "cost-fixture".into(),
            run: moruna_kernel::RunId([4; 16]).to_hex(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        });
        let namespace = Object {
            workspace: "cost-fixture".into(),
            bucket: "home".into(),
            key: "cost".into(),
        };
        (Scratch(root), home, access, verifier, namespace)
    }
    fn limits() -> Limits {
        Limits {
            records: 8,
            object_bytes: 65536,
            scan_bytes: 1 << 20,
            queue: 2,
            timeout: Duration::from_secs(2),
            retention: Retention { per_key: 1 },
        }
    }
    fn observation(at_ms: u64) -> Observation {
        let key = CostKey {
            role: moruna_job::Role::Kernel,
            kind: "std".into(),
            fingerprint: Digest([1; 32]),
            settings: Digest([2; 32]),
            schema: Digest([3; 32]),
        };
        let mut outcome = Outcome::new(Origin::WarmUp);
        outcome.set(Target::Duration, at_ms as f64 / 1000.0);
        Observation::new(key, Inputs::default(), outcome, at_ms)
    }
    #[test]
    fn model_publication_rejects_a_fitter_whose_loaded_version_is_stale() {
        let (_root, home, access, _verifier, namespace) = fixture();
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let first = Arc::new(
            Store::open(
                home.clone(),
                access.clone(),
                alloc.clone(),
                namespace.clone(),
                limits(),
            )
            .unwrap(),
        );
        let fitted = crate::cost::CostSettings::embedder(first.clone())
            .open()
            .unwrap();
        let model = fitted.model_id();
        let first = first.fitting_store().unwrap().unwrap();
        let stale = first.fitting_store().unwrap().unwrap();
        first.load_state(&model).unwrap();
        stale.load_state(&model).unwrap();
        first
            .save_state(&model, b"first acknowledged replacement")
            .unwrap();
        assert!(stale.save_state(&model, b"stale fitter overwrite").is_err());
        assert_eq!(
            first.load_state(&model).unwrap(),
            Some(b"first acknowledged replacement".to_vec())
        );
        assert_eq!(
            stale.load_state(&model).unwrap(),
            Some(b"first acknowledged replacement".to_vec())
        );
        stale
            .save_state(&model, b"explicitly reloaded replacement")
            .unwrap();
        assert_eq!(
            first.load_state(&model).unwrap(),
            Some(b"explicitly reloaded replacement".to_vec())
        );
    }
    #[test]
    fn same_native_record_codec_persists_history_and_model_state_without_local_shadow() {
        let (_root, home, access, verifier, namespace) = fixture();
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let writer = Arc::new(
            Store::open(
                home.clone(),
                access.clone(),
                alloc.clone(),
                namespace.clone(),
                limits(),
            )
            .unwrap(),
        );
        let first = observation(10);
        let second = observation(20);
        writer.append(&first).unwrap();
        writer.append(&second).unwrap();
        let history = writer.scan(&first.key).unwrap();
        assert_eq!(history.observations, vec![second.clone()]);
        assert!(
            history.summary.is_some(),
            "older actual measurement is summarised, not lost"
        );
        let settings = crate::cost::CostSettings::embedder(writer.clone());
        let fitted = settings.open().unwrap();
        let model = fitted.model_id();
        let state = writer
            .load_state(&model)
            .unwrap()
            .expect("actual fitted state acknowledged");
        let reader = Store::open(home, access, alloc, namespace, limits()).unwrap();
        assert_eq!(reader.load_state(&model).unwrap(), Some(state));
        assert_eq!(
            reader.scan(&first.key).unwrap(),
            history,
            "another adapter reopens the same authoritative bytes"
        );
        verifier.0.store(false, Ordering::SeqCst);
        assert!(
            reader.scan(&first.key).is_err(),
            "read denial is never empty history"
        );
        assert!(
            reader.load_state(&model).is_err(),
            "read denial is never absent state"
        );
        assert!(
            reader.append(&observation(30)).is_err(),
            "revoked write cannot be acknowledged"
        );
    }
    #[test]
    fn governed_cost_calls_are_safe_inside_an_existing_tokio_runtime_and_refuse_real_admission() {
        let (_root, home, access, _, namespace) = fixture();
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new().with_limit(Tier::Host, 16));
        let store = Store::open(home, access, alloc, namespace, limits()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(
                store.append(&observation(10)).is_err(),
                "actual arena refusal propagates; no temporary fallback"
            );
        });
    }
}
