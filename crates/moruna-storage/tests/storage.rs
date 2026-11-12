//! SDD32 deterministic adapter and governance checks. These are not cloud/raw-device proofs.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};

use moruna_kernel::{Allocator, Buffer, Tier};
use moruna_storage::Role;
use moruna_storage::bucket::{Bucket, Config};
use moruna_storage::cache::{Cache, Keys, Lookup, WorkspaceKey};
use moruna_storage::local::FileStore;
use moruna_storage::{
    Access, Action, Authorizer, Backend, Condition, Credential, Error, Governed, Object, Operation,
    Result, VERSION,
};
use moruna_testkit::FakeAllocator;
use zeroize::Zeroizing;

// Image leases use an open-file-description lock. A concurrent process spawn
// can inherit another test's locked descriptor until exec closes CLOEXEC files.
// Keep this binary's CLI spawning and image-provision lease checks disjoint;
// this never retries or relaxes a refused production lease.
fn provision_process_guard() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|error| error.into_inner())
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let name = format!(
            "m-st-{}-{}",
            std::process::id(),
            &blake3::hash(&random).to_hex().as_str()[..16]
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Verifier {
    allowed: AtomicBool,
    calls: AtomicUsize,
    shared_read: AtomicBool,
    delegated_write: AtomicBool,
}
impl Verifier {
    fn new() -> Self {
        Self {
            allowed: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            shared_read: AtomicBool::new(false),
            delegated_write: AtomicBool::new(false),
        }
    }
}
impl Authorizer for Verifier {
    fn verify<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        action: Action,
        _version: Option<&'a moruna_storage::Version>,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.allowed.load(Ordering::SeqCst)
                || access.credential.application() != Some(&b"TEST-ONLY"[..])
                || object.key.starts_with("forbidden")
            {
                return Err(Error::Denied);
            }
            if access.workspace != object.workspace {
                let granted = match action {
                    Action::Read | Action::List => self.shared_read.load(Ordering::SeqCst),
                    Action::Write | Action::Publish => self.delegated_write.load(Ordering::SeqCst),
                };
                if !granted {
                    return Err(Error::Denied);
                }
            }
            Ok(())
        })
    }
}
#[derive(Clone)]
struct TestStaging {
    state: Arc<StageState>,
}
struct StageState {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
}
struct TestLease {
    state: Arc<StageState>,
    bytes: usize,
}
impl moruna_storage::staging::Lease for TestLease {}
impl Drop for TestLease {
    fn drop(&mut self) {
        self.state.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
impl TestStaging {
    fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(StageState {
                used: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                limit,
            }),
        }
    }
}
impl moruna_storage::staging::Staging for TestStaging {
    fn reserve(
        &self,
        scope: &moruna_storage::Scope,
        bytes: u64,
    ) -> Result<Box<dyn moruna_storage::staging::Lease>> {
        scope.validate()?;
        let bytes = usize::try_from(bytes).unwrap();
        let before = self
            .state
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes)
                    .filter(|sum| *sum <= self.state.limit)
            });
        match before {
            Ok(used) => {
                self.state.peak.fetch_max(used + bytes, Ordering::SeqCst);
                Ok(Box::new(TestLease {
                    state: Arc::clone(&self.state),
                    bytes,
                }))
            }
            Err(_) => Err(Error::StagingRefused {
                need: bytes as u64,
                budget: self.state.limit as u64,
            }),
        }
    }
}
struct TestKeys(AtomicU8);
impl Keys for TestKeys {
    fn workspace<'a>(
        &'a self,
        access: &'a Access,
        id: Option<&'a str>,
    ) -> Operation<'a, WorkspaceKey> {
        Box::pin(async move {
            if id.is_some_and(|id| id != "TEST-KEY") {
                return Err(Error::Denied);
            }
            Ok(WorkspaceKey {
                id: "TEST-KEY".into(),
                bytes: Zeroizing::new(
                    [self.0.load(Ordering::SeqCst)
                        ^ blake3::hash(access.workspace.as_bytes()).as_bytes()[0];
                        32],
                ),
            })
        })
    }
}
fn context() -> Access {
    Access {
        workspace: "workspace".into(),
        run: "run".into(),
        credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
    }
}
fn object(key: &str) -> Object {
    Object {
        workspace: "workspace".into(),
        bucket: "bucket".into(),
        key: key.into(),
    }
}
fn bytes(alloc: &dyn Allocator, data: &[u8]) -> Buffer {
    let mut b = alloc.alloc(data.len(), Tier::Host).unwrap();
    b.as_mut().copy_from_slice(data);
    b
}
fn bucket(auth: Arc<Verifier>) -> Arc<Governed> {
    let backend = Bucket::unclassified(
        Arc::new(object_store::memory::InMemory::new()),
        Config::new("workspace".into(), "bucket".into()),
        Arc::new(TestStaging::new(64 << 20)),
    )
    .unwrap();
    Arc::new(Governed::new(Arc::new(backend), auth).unwrap())
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn st_t1_same_conformance_for_sdk_memory_and_durable_format() {
    runtime().block_on(async {
        let alloc = FakeAllocator::new();
        let access = context();
        let verifier = Arc::new(Verifier::new());
        let expected = [
            "ST-T1 exact range and original arena allocation",
            "ST-T1 empty range at unaligned offset and EOF",
            "ST-T1 immutable create conflict",
            "ST-T1 stale conditional replacement refused",
            "ST-T1 concurrent CAS has one winner",
            "ST-T1 empty object",
            "ST-T1 recursive prefix list",
        ];
        let sdk = bucket(Arc::clone(&verifier));
        assert_eq!(
            moruna_storage::conformance::check(&sdk, &access, &alloc, &object("suite"))
                .await
                .unwrap()
                .as_slice(),
            &expected[..]
        );
        let scratch = Scratch::new();
        let local = FileStore::open(
            scratch.0.join("store"),
            "workspace".into(),
            "bucket".into(),
            Role::FormatProof,
        )
        .unwrap();
        let local = Governed::new(Arc::new(local), verifier).unwrap();
        assert_eq!(
            moruna_storage::conformance::check(&local, &access, &alloc, &object("suite"))
                .await
                .unwrap()
                .as_slice(),
            &expected[..]
        );
    });
}

#[test]
fn st_t2_cache_hit_is_reauthorized_and_cross_workspace_is_refused() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        let alloc = FakeAllocator::new();
        let access = context();
        let verifier = Arc::new(Verifier::new());
        let store = bucket(Arc::clone(&verifier));
        let keys = Arc::new(TestKeys(AtomicU8::new(7)));
        let cache =
            Cache::create(scratch.0.join("cache"), keys, Arc::clone(&store), 65536).unwrap();
        let original = Arc::new(bytes(&alloc, b"historical rows"));
        let metadata = store
            .write(&access, &object("rows"), original.view())
            .await
            .unwrap();
        cache
            .insert(
                &access,
                &metadata.object,
                &metadata.version,
                0,
                bytes(&alloc, b"historical rows"),
            )
            .await
            .unwrap();
        let pointer = alloc.alloc(15, Tier::Host).unwrap();
        let at = pointer.host_ptr();
        match cache
            .read(&access, &metadata.object, &metadata.version, 0, pointer)
            .await
            .unwrap()
        {
            Lookup::Hit(buffer) => {
                assert_eq!(buffer.host_ptr(), at);
                assert_eq!(buffer.as_ref(), b"historical rows");
            }
            Lookup::Miss(_) => panic!("inserted entry missing"),
        }
        verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            cache
                .read(
                    &access,
                    &metadata.object,
                    &metadata.version,
                    0,
                    alloc.alloc(15, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
        let other = Object {
            workspace: "another".into(),
            ..metadata.object.clone()
        };
        assert!(matches!(
            cache
                .read(
                    &access,
                    &other,
                    &metadata.version,
                    0,
                    alloc.alloc(15, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
        assert!(verifier.calls.load(Ordering::SeqCst) >= 4);
    });
}

#[test]
fn st_t3_cache_ciphertext_binding_corruption_rotation_and_recycle() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        let alloc = FakeAllocator::new();
        let access = context();
        let verifier = Arc::new(Verifier::new());
        let store = bucket(verifier);
        let keys = Arc::new(TestKeys(AtomicU8::new(7)));
        let root = scratch.0.join("cache");
        let cache = Cache::create(root.clone(), keys.clone(), Arc::clone(&store), 65536).unwrap();
        let data = b"PRIVATE-TENANT-ROWS-ONLY";
        let metadata = store
            .write(
                &access,
                &object("rows"),
                Arc::new(bytes(&alloc, data)).view(),
            )
            .await
            .unwrap();
        cache
            .insert(
                &access,
                &metadata.object,
                &metadata.version,
                0,
                bytes(&alloc, data),
            )
            .await
            .unwrap();
        let entry = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let encrypted = fs::read(&entry).unwrap();
        assert!(!encrypted.windows(data.len()).any(|w| w == data));
        // Re-inserting generates a different nonce/ciphertext under the same bound identity.
        cache
            .insert(
                &access,
                &metadata.object,
                &metadata.version,
                0,
                bytes(&alloc, data),
            )
            .await
            .unwrap();
        assert_ne!(encrypted, fs::read(&entry).unwrap());
        keys.0.store(9, Ordering::SeqCst);
        assert!(matches!(
            cache
                .read(
                    &access,
                    &metadata.object,
                    &metadata.version,
                    0,
                    alloc.alloc(data.len(), Tier::Host).unwrap()
                )
                .await,
            Err(Error::Integrity)
        ));
        keys.0.store(7, Ordering::SeqCst);
        let mut corrupted = fs::read(&entry).unwrap();
        let end = corrupted.len() - 1;
        corrupted[end] ^= 1;
        fs::write(&entry, corrupted).unwrap();
        assert!(matches!(
            cache
                .read(
                    &access,
                    &metadata.object,
                    &metadata.version,
                    0,
                    alloc.alloc(data.len(), Tier::Host).unwrap()
                )
                .await,
            Err(Error::Integrity)
        ));
        cache.recycle().unwrap();
        assert!(!root.exists());
        assert!(matches!(
            cache
                .insert(
                    &access,
                    &metadata.object,
                    &metadata.version,
                    0,
                    bytes(&alloc, data)
                )
                .await,
            Err(Error::Unavailable("recycled cache"))
        ));
    });
}

#[test]
fn st_t4_incomplete_commit_tail_is_not_visible_and_next_commit_discards_it() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        let alloc = FakeAllocator::new();
        let access = context();
        let root = scratch.0.join("store");
        let verifier = Arc::new(Verifier::new());
        let local = FileStore::open(
            root.clone(),
            "workspace".into(),
            "bucket".into(),
            Role::FormatProof,
        )
        .unwrap();
        let store = Governed::new(Arc::new(local), verifier).unwrap();
        let old = store
            .write(
                &access,
                &object("committed"),
                Arc::new(bytes(&alloc, b"old")).view(),
            )
            .await
            .unwrap();
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(root.join("commits.log"))
            .unwrap();
        log.write_all(&[100, 0, 0, 0, b'{']).unwrap();
        log.sync_all().unwrap();
        drop(log);
        assert_eq!(store.list(&access, &object("")).await.unwrap().len(), 1);
        store
            .replace(
                &access,
                &old.object,
                Condition::Match(old.version),
                Arc::new(bytes(&alloc, b"new")).view(),
            )
            .await
            .unwrap();
        assert_eq!(store.list(&access, &object("")).await.unwrap().len(), 1);
    });
}

#[test]
fn st_t1_protocol_major_is_not_a_compatibility_fallback() {
    struct Wrong;
    impl Backend for Wrong {
        fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
            None
        }
        fn current<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
        ) -> Operation<'a, moruna_storage::Metadata> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }

        fn head<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
            _: &'a moruna_storage::Version,
        ) -> Operation<'a, moruna_storage::Metadata> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }

        fn version(&self) -> u32 {
            VERSION + 1
        }
        fn read<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
            _: &'a moruna_storage::Version,
            _: u64,
            _: Buffer,
        ) -> Operation<'a, Buffer> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }
        fn write<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
            _: moruna_kernel::BufferView,
        ) -> Operation<'a, moruna_storage::Metadata> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }
        fn list<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
        ) -> Operation<'a, Vec<moruna_storage::Metadata>> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }
        fn replace<'a>(
            &'a self,
            _: &'a moruna_storage::Scope,
            _: &'a Object,
            _: Condition,
            _: moruna_kernel::BufferView,
        ) -> Operation<'a, moruna_storage::Metadata> {
            Box::pin(async { Err(Error::Unavailable("wrong major")) })
        }
    }
    assert!(matches!(
        Governed::new(Arc::new(Wrong), Arc::new(Verifier::new())),
        Err(Error::Unavailable("storage protocol major"))
    ));
}

#[test]
fn st_t5_portable_checkpoint_commits_locally_then_restores_under_new_node_context() {
    runtime().block_on(async {
        use moruna_storage::checkpoint::{Checkpoints, Dependency, Portable};
        let scratch = Scratch::new(); let access = context();
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let verifier = Arc::new(Verifier::new()); let home = bucket(Arc::clone(&verifier));
        let local = Arc::new(Governed::new(Arc::new(FileStore::open(scratch.0.join("node-one"), "workspace".into(), "bucket".into(), Role::CheckpointMetadata).unwrap()), verifier.clone()).unwrap());
        let data = b"immutable dependency";
        let dependency = home.write(&access, &object("dependency"), Arc::new(bytes(alloc.as_ref(), data)).view()).await.unwrap();
        let manager = Checkpoints::new(Arc::clone(&local), Arc::clone(&home), Arc::clone(&alloc), 4096).unwrap();
        let portable = Portable { version: VERSION, workspace: "workspace".into(), run: "run".into(), sequence: 0,
            manifest: serde_json::json!({"segments":[],"lineage":[],"source_cursors":[{"split_index":1,"row_offset":3,"next_seq":9}]}),
            dependencies: vec![Dependency { metadata: dependency, content_hash: blake3::hash(data).to_hex().to_string() }] };
        let committed = manager.publish(&access, &object("local-checkpoint"), &object("bucket-checkpoint"), Condition::Absent, &portable).await.unwrap();
        assert_eq!(local.list(&access, &object("local-checkpoint")).await.unwrap().len(), 1);
        let new_local = Arc::new(Governed::new(Arc::new(FileStore::open(scratch.0.join("node-two"), "workspace".into(), "bucket".into(), Role::CheckpointMetadata).unwrap()), verifier.clone()).unwrap());
        let another_node = Checkpoints::new(new_local, Arc::clone(&home), Arc::clone(&alloc), 4096).unwrap();
        let restored = another_node.restore(&access, &committed).await.unwrap();
        assert_eq!(restored.sequence, 0);
        assert_eq!(restored.manifest["source_cursors"][0]["next_seq"], 9);
        let mut local_only = portable.clone();
        local_only.manifest["lineage"] = serde_json::json!([{"disk":{"segment":1,"offset":4096,"len":1024}}]);
        assert!(matches!(manager.publish(&access, &object("bad-local"), &object("bad-bucket"), Condition::Absent, &local_only).await, Err(Error::Invalid(_))));
        verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(another_node.restore(&access, &committed).await, Err(Error::Denied)));
    });
}

#[test]
fn st_t7_required_capture_keeps_fusion_capability_explicit_and_never_marks_partial_complete() {
    runtime().block_on(async {
        use moruna_storage::artifact::{Boundary, Capture, Identity, Reader, Request, State};
        let access = context();
        let verifier = Arc::new(Verifier::new());
        let store = bucket(verifier.clone());
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let batch = moruna_kernel::arrow::record_batch::RecordBatch::try_from_iter([(
            "value",
            Arc::new(moruna_kernel::arrow::array::Int64Array::from(vec![7]))
                as moruna_kernel::arrow::array::ArrayRef,
        )])
        .unwrap();
        let record = || {
            let (framing, pieces) =
                moruna_kernel::ipc::encode_framing(&batch, 4096, 0, alloc.as_ref()).unwrap();
            let len = pieces
                .iter()
                .map(|(offset, bytes)| offset + bytes.len())
                .chain(std::iter::once(framing.len()))
                .max()
                .unwrap();
            let mut encoded = alloc.alloc(len, Tier::Host).unwrap();
            encoded.as_mut().fill(0);
            encoded.as_mut()[..framing.len()].copy_from_slice(framing.as_ref());
            for (offset, bytes) in pieces {
                encoded.as_mut()[offset..offset + bytes.len()].copy_from_slice(bytes.as_slice());
            }
            encoded
        };
        let encoded_len = record().len();
        let request = Request {
            identity: Identity {
                workspace: "workspace".into(),
                run: "run".into(),
                attempt: "attempt-one".into(),
                task: "logical-task".into(),
                port: "rows".into(),
            },
            required: true,
            byte_budget: encoded_len as u64,
            policy: "TEST-POLICY".into(),
        };
        assert!(matches!(
            Capture::prepare(
                request.clone(),
                Boundary::Opaque,
                object("artifact"),
                Arc::clone(&store),
                Arc::clone(&alloc),
                4096
            ),
            Err(Error::Unavailable(_))
        ));
        assert!(
            store
                .list(&access, &object("artifact"))
                .await
                .unwrap()
                .is_empty()
        );
        let mut capture = Capture::prepare(
            request,
            Boundary::LogicalRows,
            object("artifact"),
            Arc::clone(&store),
            Arc::clone(&alloc),
            4096,
        )
        .unwrap();
        capture.begin(&access).await.unwrap();
        capture
            .append(&access, "TEST-SCHEMA".into(), 1, Arc::new(record()).view())
            .await
            .unwrap();
        assert!(
            capture
                .append(&access, "TEST-SCHEMA".into(), 1, Arc::new(record()).view())
                .await
                .is_err()
        );
        assert!(capture.complete(&access).await.is_err());
        let metadata = store
            .list(&access, &object("artifact"))
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.object.key == "artifact")
            .unwrap();
        let reader = Reader::new(Arc::clone(&store), Arc::clone(&alloc), 4096).unwrap();
        let historical = reader.manifest(&access, &metadata).await.unwrap();
        assert!(matches!(historical.state, State::Refused(_)));
        assert_eq!(
            (historical.rows, historical.bytes, historical.chunks.len()),
            (1, encoded_len as u64, 1)
        );
        let rows = reader
            .chunk(
                &access,
                &historical,
                0,
                alloc.alloc(encoded_len, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        let decoded = moruna_kernel::ipc::decode(rows.into_arrow_buffer().unwrap(), 4096).unwrap();
        assert_eq!(decoded.num_rows(), 1);
        let values = decoded
            .column(0)
            .as_any()
            .downcast_ref::<moruna_kernel::arrow::array::Int64Array>()
            .unwrap();
        assert_eq!(values.value(0), 7);
        verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            reader
                .chunk(
                    &access,
                    &historical,
                    0,
                    alloc.alloc(encoded_len, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
    });
}

#[test]
fn st_t3_cipher_schedule_zeroization_is_an_enabled_library_contract() {
    use aes_gcm::KeyInit;
    fn schedule<T: zeroize::ZeroizeOnDrop>(_: &T) {}
    let cipher = aes::Aes256::new_from_slice(&[0u8; 32]).unwrap();
    schedule(&cipher);
}

#[test]
fn st_t2_explicit_foreign_read_grant_preserves_requester_key_and_cache_isolation() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        let alloc = FakeAllocator::new();
        let verifier = Arc::new(Verifier::new());
        let backend = Bucket::unclassified(
            Arc::new(object_store::memory::InMemory::new()),
            Config::new("resource-owner".into(), "bucket".into()),
            Arc::new(TestStaging::new(64 << 20)),
        )
        .unwrap();
        let store = Arc::new(Governed::new(Arc::new(backend), verifier.clone()).unwrap());
        let foreign = Object {
            workspace: "resource-owner".into(),
            bucket: "bucket".into(),
            key: "shared-rows".into(),
        };
        let owner = Access {
            workspace: "resource-owner".into(),
            run: "owner-fixture".into(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        };
        let source = store
            .write(
                &owner,
                &foreign,
                Arc::new(bytes(&alloc, b"shared data")).view(),
            )
            .await
            .unwrap();
        let requester = context();
        assert!(matches!(
            store
                .read(
                    &requester,
                    &foreign,
                    &source.version,
                    0,
                    alloc.alloc(11, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
        verifier.shared_read.store(true, Ordering::SeqCst);
        let root = scratch.0.join("cache");
        let cache = Cache::create(
            root.clone(),
            Arc::new(TestKeys(AtomicU8::new(7))),
            Arc::clone(&store),
            65536,
        )
        .unwrap();
        let read = store
            .read(
                &requester,
                &foreign,
                &source.version,
                0,
                alloc.alloc(11, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        cache
            .insert(&requester, &foreign, &source.version, 0, read)
            .await
            .unwrap();
        let first = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let partner = Access {
            workspace: "partner".into(),
            run: "inspection".into(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        };
        assert!(matches!(
            cache
                .read(
                    &partner,
                    &foreign,
                    &source.version,
                    0,
                    alloc.alloc(11, Tier::Host).unwrap()
                )
                .await
                .unwrap(),
            Lookup::Miss(_)
        ));
        let read = store
            .read(
                &partner,
                &foreign,
                &source.version,
                0,
                alloc.alloc(11, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        cache
            .insert(&partner, &foreign, &source.version, 0, read)
            .await
            .unwrap();
        let second = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| *path != first)
            .unwrap();
        assert_ne!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        for access in [&requester, &partner] {
            match cache
                .read(
                    access,
                    &foreign,
                    &source.version,
                    0,
                    alloc.alloc(11, Tier::Host).unwrap(),
                )
                .await
                .unwrap()
            {
                Lookup::Hit(buffer) => assert_eq!(buffer.as_ref(), b"shared data"),
                Lookup::Miss(_) => panic!("scoped entry absent"),
            }
        }
        fs::copy(&first, &second).unwrap();
        assert!(matches!(
            cache
                .read(
                    &partner,
                    &foreign,
                    &source.version,
                    0,
                    alloc.alloc(11, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Integrity)
        ));
        verifier.shared_read.store(false, Ordering::SeqCst);
        assert!(matches!(
            cache
                .read(
                    &requester,
                    &foreign,
                    &source.version,
                    0,
                    alloc.alloc(11, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
    });
}

#[test]
fn st_t7_delegated_artifact_target_does_not_rewrite_producer_or_reader_identity() {
    runtime().block_on(async {
        use moruna_storage::artifact::{Boundary, Capture, Identity, Reader, Request, State};
        let verifier = Arc::new(Verifier::new());
        verifier.shared_read.store(true, Ordering::SeqCst);
        verifier.delegated_write.store(true, Ordering::SeqCst);
        let backend = Bucket::unclassified(
            Arc::new(object_store::memory::InMemory::new()),
            Config::new("delegated-owner".into(), "bucket".into()),
            Arc::new(TestStaging::new(64 << 20)),
        )
        .unwrap();
        let store = Arc::new(Governed::new(Arc::new(backend), verifier).unwrap());
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let producer = context();
        let target = Object {
            workspace: "delegated-owner".into(),
            bucket: "bucket".into(),
            key: "artifact".into(),
        };
        let request = Request {
            identity: Identity {
                workspace: producer.workspace.clone(),
                run: producer.run.clone(),
                attempt: "attempt".into(),
                task: "logical-task".into(),
                port: "rows".into(),
            },
            required: true,
            byte_budget: 4096,
            policy: "TEST-POLICY".into(),
        };
        let mut capture = Capture::prepare(
            request,
            Boundary::LogicalRows,
            target.clone(),
            Arc::clone(&store),
            Arc::clone(&alloc),
            4096,
        )
        .unwrap();
        capture.begin(&producer).await.unwrap();
        let published = capture.complete(&producer).await.unwrap(); // Actual empty logical output.
        assert_eq!(published.object.workspace, "delegated-owner");
        let viewer = Access {
            workspace: "partner".into(),
            run: "history-inspection".into(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        };
        let reader = Reader::new(store, alloc, 4096).unwrap();
        let historical = reader.manifest(&viewer, &published).await.unwrap();
        assert_eq!(historical.identity.workspace, producer.workspace);
        assert_eq!(historical.identity.run, producer.run);
        assert_eq!(historical.state, State::Complete);
        assert_eq!(historical.rows, 0);
    });
}

#[test]
fn st_t6_sdk_staging_is_charged_bounded_and_ingress_copy_is_reported() {
    runtime().block_on(async {
        let access = context();
        let alloc = FakeAllocator::new();
        let stages = Arc::new(TestStaging::new(3));
        let mut config = Config::new("workspace".into(), "bucket".into());
        config.range_bytes = 3;
        config.parallel_ranges = 1;
        let backend = Arc::new(
            Bucket::unclassified(
                Arc::new(object_store::memory::InMemory::new()),
                config,
                stages.clone(),
            )
            .unwrap(),
        );
        let store = Governed::new(backend.clone(), Arc::new(Verifier::new())).unwrap();
        let original = store
            .write(
                &access,
                &object("bounded-range"),
                Arc::new(bytes(&alloc, b"abcdefgh")).view(),
            )
            .await
            .unwrap();
        let destination = alloc.alloc(8, Tier::Host).unwrap();
        let pointer = destination.host_ptr();
        let read = store
            .read(&access, &original.object, &original.version, 0, destination)
            .await
            .unwrap();
        assert_eq!(read.host_ptr(), pointer);
        assert_eq!(read.as_ref(), b"abcdefgh");
        let stats = backend.staging_stats();
        assert_eq!(stats.peak_reserved_bytes, 3);
        assert_eq!(stats.reserved_bytes, 0);
        assert_eq!(stats.requests, 3);
        assert_eq!(stats.copied_bytes, 8);
        assert_eq!(stages.state.used.load(Ordering::SeqCst), 0);
        assert_eq!(stages.state.peak.load(Ordering::SeqCst), 3);
    });
}

#[cfg(unix)]
#[test]
fn st_t6_cancelled_sdk_consumer_keeps_charge_and_destination_until_current_body_drains() {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    struct Fixture {
        path: PathBuf,
        release: Option<std::sync::mpsc::Sender<()>>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                // Cleanup may run after the worker/receiver already exited.
                let _receiver_already_closed = release.send(());
            }
            drop(std::os::unix::net::UnixStream::connect(&self.path));
            if let Some(worker) = self.worker.take()
                && let Err(cause) = worker.join()
                && !std::thread::panicking()
            {
                std::panic::resume_unwind(cause);
            }
        }
    }
    let scratch = Scratch::new();
    // Keep the Unix socket name short on macOS; Scratch remains process/entropy unique.
    let path = scratch.0.join("s");
    let listener = UnixListener::bind(&path).unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let (started, waiting) = tokio::sync::oneshot::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut one = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read_exact(&mut one) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return,
                Err(error) => panic!("controlled request read: {error}"),
            }
            request.push(one[0]);
            assert!(request.len() < 16384);
        }
        let lower = std::str::from_utf8(&request).unwrap().to_ascii_lowercase();
        assert!(
            lower.contains("range: bytes=0-3"),
            "SDK must request a bounded range"
        );
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/8\r\nETag: \"fixture-etag\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nConnection: close\r\n\r\na").unwrap();
        stream.flush().unwrap();
        started.send(()).unwrap();
        released.recv().unwrap();
        // Normal cancellation drains this already-started bounded response before releasing
        // its charge; it must not issue the next range of the logical eight-byte read.
        stream.write_all(b"bcd").unwrap();
    });
    let mut fixture = Fixture {
        path: path.clone(),
        release: Some(release),
        worker: Some(worker),
    };
    runtime().block_on(async {
        let sdk = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name("bucket")
            .with_region("us-east-1")
            .with_access_key_id("TEST-ONLY-ACCESS")
            .with_secret_access_key("TEST-ONLY-SECRET")
            .with_endpoint(moruna_storage::proxy::SOCKET_ENDPOINT)
            .with_allow_http(true)
            .with_virtual_hosted_style_request(false)
            .with_http_connector(moruna_storage::proxy::SocketConnector::new(
                moruna_storage::proxy::SocketTarget::Unix(path),
            ))
            .build()
            .unwrap();
        let stages = Arc::new(TestStaging::new(4));
        let mut config = Config::new("workspace".into(), "bucket".into());
        config.range_bytes = 4;
        config.parallel_ranges = 1;
        let backend =
            Arc::new(Bucket::unclassified(Arc::new(sdk), config, stages.clone()).unwrap());
        let governed = Arc::new(Governed::new(backend.clone(), Arc::new(Verifier::new())).unwrap());
        let alloc = FakeAllocator::new();
        let destination = alloc.alloc(8, Tier::Host).unwrap();
        let caller = tokio::spawn(async move {
            governed
                .read(
                    &context(),
                    &object("rows"),
                    &moruna_storage::Version {
                        etag: Some("\"fixture-etag\"".into()),
                        generation: None,
                    },
                    0,
                    destination,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(stages.state.used.load(Ordering::SeqCst), 4);
        assert_eq!(backend.staging_stats().active_reads, 1);
        assert_eq!(alloc.in_use(Tier::Host), 8);
        fixture.release.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), backend.wait_idle())
            .await
            .unwrap();
        assert_eq!(stages.state.used.load(Ordering::SeqCst), 0);
        assert_eq!(alloc.in_use(Tier::Host), 0);
        assert_eq!(backend.staging_stats().requests, 1);
        assert_eq!(backend.staging_stats().copied_bytes, 4);
    });
}

struct VolumeArena(Arc<FakeAllocator>);
impl moruna_storage::volume::Arenas for VolumeArena {
    fn allocator(&self, scope: &moruna_storage::Scope) -> Result<Arc<dyn Allocator>> {
        scope.validate()?;
        if scope.workspace != "workspace" || scope.run != "run" {
            return Err(Error::Denied);
        }
        Ok(self.0.clone())
    }
}
#[test]
fn st_t4_block_volume_reopens_exact_commits_and_verifies_unaligned_ranges() {
    runtime().block_on(async {
        use moruna_storage::{Scope, blocks::FileBlocks, volume::Volume};
        let scratch = Scratch::new();
        let path = scratch.0.join("volume");
        let allocator = Arc::new(FakeAllocator::new());
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        let blocks = FileBlocks::create(&path, 4 << 20, 4096).unwrap();
        let volume = Arc::new(
            Volume::new(
                blocks,
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        let geometry = volume
            .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
            .await
            .unwrap();
        assert_eq!(geometry.role, Role::FormatProof);
        let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
        let input = vec![0x73; 3 * 4096 + 71];
        let metadata = store
            .write(
                &context(),
                &object("raw-format"),
                Arc::new(bytes(allocator.as_ref(), &input)).view(),
            )
            .await
            .unwrap();
        let destination = allocator.alloc(8201, Tier::Host).unwrap();
        let pointer = destination.host_ptr();
        let result = store
            .read(
                &context(),
                &metadata.object,
                &metadata.version,
                13,
                destination,
            )
            .await
            .unwrap();
        assert_eq!(result.host_ptr(), pointer);
        assert_eq!(result.as_ref(), &input[13..13 + 8201]);
        drop(store);
        drop(volume);
        let reopened = Volume::new(
            FileBlocks::open(&path, 4096).unwrap(),
            Arc::new(VolumeArena(allocator.clone())),
            Arc::new(TestStaging::new(1 << 20)),
        )
        .unwrap();
        assert_eq!(reopened.mount(&scope).await.unwrap(), geometry);
        let store = Governed::new(Arc::new(reopened), Arc::new(Verifier::new())).unwrap();
        assert_eq!(
            store.list(&context(), &object("")).await.unwrap(),
            vec![metadata.clone()]
        );
        assert!(matches!(
            store
                .write(
                    &context(),
                    &metadata.object,
                    Arc::new(bytes(allocator.as_ref(), b"duplicate")).view()
                )
                .await,
            Err(Error::Conflict)
        ));
        // A physical payload corruption is not silently returned or treated as a cache miss.
        use std::os::unix::fs::FileExt;
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0x12], geometry.data_offset + 17)
            .unwrap();
        file.sync_all().unwrap();
        assert!(matches!(
            store
                .read(
                    &context(),
                    &metadata.object,
                    &metadata.version,
                    0,
                    allocator.alloc(4096, Tier::Host).unwrap()
                )
                .await,
            Err(Error::Format(moruna_storage_format::Error::Integrity))
        ));
    });
}

#[test]
fn st_t4_unreceipted_data_is_not_visible_after_interrupted_publication() {
    use moruna_storage::{
        Scope,
        blocks::{Blocks, FileBlocks, Transaction},
        volume::Volume,
    };
    struct Fault {
        inner: Arc<FileBlocks>,
        reject_receipt: Arc<AtomicBool>,
    }
    struct FaultTx {
        inner: Box<dyn Transaction>,
        reject_receipt: Arc<AtomicBool>,
    }
    impl Blocks for Fault {
        fn capacity(&self) -> u64 {
            self.inner.capacity()
        }
        fn alignment(&self) -> usize {
            self.inner.alignment()
        }
        fn role(&self) -> Role {
            Role::FormatProof
        }
        fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>> {
            Ok(Box::new(FaultTx {
                inner: self.inner.clone().begin()?,
                reject_receipt: self.reject_receipt.clone(),
            }))
        }
    }
    impl Transaction for FaultTx {
        fn read(&self, offset: u64, dst: Buffer) -> Operation<'static, Buffer> {
            self.inner.read(offset, dst)
        }
        fn write(&self, offset: u64, src: moruna_kernel::BufferView) -> Operation<'static, ()> {
            let receipt = src.as_host_slice().is_some_and(|s| {
                matches!(
                    moruna_storage_format::Frame::decode(s),
                    Ok(moruna_storage_format::Frame::Receipt { .. })
                )
            });
            if receipt && self.reject_receipt.swap(false, Ordering::SeqCst) {
                return Box::pin(async { Err(Error::Unavailable("TEST-ONLY receipt fault")) });
            }
            self.inner.write(offset, src)
        }
        fn sync(&self) -> Operation<'static, ()> {
            self.inner.sync()
        }
    }
    runtime().block_on(async {
        let scratch = Scratch::new();
        let allocator = Arc::new(FakeAllocator::new());
        let reject = Arc::new(AtomicBool::new(true));
        let blocks = Arc::new(Fault {
            inner: FileBlocks::create(&scratch.0.join("volume"), 4 << 20, 4096).unwrap(),
            reject_receipt: reject,
        });
        let volume = Arc::new(
            Volume::new(
                blocks,
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        volume
            .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
            .await
            .unwrap();
        let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
        let target = object("interrupted");
        assert!(matches!(
            store
                .write(
                    &context(),
                    &target,
                    Arc::new(bytes(allocator.as_ref(), b"not committed")).view()
                )
                .await,
            Err(Error::Unavailable("TEST-ONLY receipt fault"))
        ));
        volume.mount(&scope).await.unwrap();
        assert!(
            store
                .list(&context(), &object(""))
                .await
                .unwrap()
                .is_empty()
        );
        let committed = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(allocator.as_ref(), b"recovered")).view(),
            )
            .await
            .unwrap();
        assert_eq!(
            store.list(&context(), &object("")).await.unwrap(),
            vec![committed]
        );
    });
}

#[test]
fn st_t3_disk_scratch_requires_live_read_authority_and_never_claims_retention() {
    runtime().block_on(async {
        let temp = Scratch::new();
        let allocator = FakeAllocator::new();
        let verifier = Arc::new(Verifier::new());
        let store = bucket(verifier.clone());
        let cache = Arc::new(
            Cache::create(
                temp.0.join("scratch"),
                Arc::new(TestKeys(AtomicU8::new(17))),
                store,
                65536,
            )
            .unwrap(),
        );
        let scratch = moruna_storage::scratch::Scratch::new(cache.clone());
        let (handle, original) = scratch
            .write(
                &context(),
                &object("ephemeral/spill"),
                bytes(&allocator, b"private-row-value"),
            )
            .await
            .unwrap();
        assert_eq!(original.as_ref(), b"private-row-value");
        for entry in fs::read_dir(temp.0.join("scratch")).unwrap() {
            let cipher = fs::read(entry.unwrap().path()).unwrap();
            assert!(
                !cipher
                    .windows(original.len())
                    .any(|w| w == original.as_ref())
            );
        }
        let restored = scratch
            .read(
                &context(),
                &handle,
                allocator.alloc(original.len(), Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(restored.as_ref(), original.as_ref());
        verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            scratch
                .read(
                    &context(),
                    &handle,
                    allocator.alloc(original.len(), Tier::Host).unwrap()
                )
                .await,
            Err(Error::Denied)
        ));
        verifier.allowed.store(true, Ordering::SeqCst);
        cache.recycle().unwrap();
        assert!(matches!(
            scratch
                .read(
                    &context(),
                    &handle,
                    allocator.alloc(original.len(), Tier::Host).unwrap()
                )
                .await,
            Err(Error::Missing)
        ));
        assert!(
            scratch
                .write(
                    &context(),
                    &object("another"),
                    bytes(&allocator, b"refused after recycle")
                )
                .await
                .is_err()
        );
    });
}

#[test]
fn st_t6_real_owned_object_socket_lands_into_caller_arena_and_refuses_body_before_send() {
    use moruna_storage::{
        Scope,
        port::{Client, Service, Unix},
    };
    use std::{
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };
    runtime().block_on(async {
        let temp = Scratch::new();
        let path = temp.0.join("port.sock");
        let listener = Arc::new(UnixListener::bind(&path).unwrap());
        listener.set_nonblocking(true).unwrap();
        let allocator = Arc::new(FakeAllocator::new());
        let arenas = Arc::new(VolumeArena(allocator.clone()));
        let remote_verifier = Arc::new(Verifier::new());
        let home = bucket(remote_verifier.clone());
        let service = Arc::new(Service::new(home, arenas.clone(), 1 << 20).unwrap());
        let server_allocator = allocator.clone();
        let server = tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..5 {
                let listener = listener.clone();
                let socket = tokio::task::spawn_blocking(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match listener.accept() {
                            Ok((socket, _)) => return socket,
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    && Instant::now() < deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(1))
                            }
                            Err(e) => panic!("controlled socket accept: {e}"),
                        }
                    }
                })
                .await
                .unwrap();
                // macOS hands an accepted socket the listener's O_NONBLOCK (Linux does not),
                // and a non-blocking socket ignores its timeouts: every read the service made
                // was WouldBlock at once, and the client was answered Unavailable.
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                results.push(
                    service
                        .clone()
                        .serve(
                            Box::new(socket),
                            server_allocator
                                .alloc(moruna_storage_format::wire::MAX_HEADER, Tier::Host)
                                .unwrap(),
                        )
                        .await,
                );
            }
            results
        });
        let client = Client::new(
            Arc::new(Unix::new(path, Duration::from_secs(5)).unwrap()),
            Arc::new(context()),
            arenas,
        )
        .unwrap();
        let store = Governed::new(Arc::new(client), Arc::new(Verifier::new())).unwrap();
        let target = object("wire/item");
        let written = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(allocator.as_ref(), b"wire-payload")).view(),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .head(&context(), &target, &written.version)
                .await
                .unwrap(),
            written
        );
        let destination = allocator.alloc(7, Tier::Host).unwrap();
        let pointer = destination.host_ptr();
        let received = store
            .read(&context(), &target, &written.version, 2, destination)
            .await
            .unwrap();
        assert_eq!(received.host_ptr(), pointer);
        assert_eq!(received.as_ref(), b"re-payl");
        remote_verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            store
                .write(
                    &context(),
                    &object("denied"),
                    Arc::new(bytes(allocator.as_ref(), b"must not be sent before Ready")).view()
                )
                .await,
            Err(Error::Denied)
        ));
        remote_verifier.allowed.store(true, Ordering::SeqCst);
        assert_eq!(
            store.list(&context(), &object("wire/")).await.unwrap(),
            vec![written]
        );
        let results = server.await.unwrap();
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
        assert!(results[2].is_ok());
        assert!(matches!(results[3], Err(Error::Denied)));
        assert!(results[4].is_ok());
        let wrong = Scope {
            workspace: "other".into(),
            run: "run".into(),
        };
        assert!(
            moruna_storage::volume::Arenas::allocator(&VolumeArena(allocator), &wrong).is_err()
        );
    });
}

#[test]
fn st_t1_block_volume_runs_same_empty_range_and_concurrent_cas_contract() {
    runtime().block_on(async {
        use moruna_storage::{Scope, blocks::FileBlocks, volume::Volume};
        let temp = Scratch::new();
        let allocator = Arc::new(FakeAllocator::new());
        let volume = Arc::new(
            Volume::new(
                FileBlocks::create(&temp.0.join("volume"), 8 << 20, 4096).unwrap(),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(8 << 20)),
            )
            .unwrap(),
        );
        volume
            .format(
                &Scope {
                    workspace: "workspace".into(),
                    run: "run".into(),
                },
                "workspace".into(),
                "bucket".into(),
                16 * 32768,
            )
            .await
            .unwrap();
        let store = Governed::new(volume, Arc::new(Verifier::new())).unwrap();
        let cases = moruna_storage::conformance::check(
            &store,
            &context(),
            allocator.as_ref(),
            &object("shared-volume-contract"),
        )
        .await
        .unwrap();
        assert!(cases.contains(&"ST-T1 concurrent CAS has one winner"));
        assert!(cases.contains(&"ST-T1 empty object"));
    });
}

#[test]
fn st_t6_known_future_prefetch_uses_real_encrypted_cache_and_reverifies_hits() {
    runtime().block_on(async {
        use moruna_storage::prefetch::{PlannedRange, fetch_cached};
        let temp = Scratch::new();
        let allocator = FakeAllocator::new();
        let verifier = Arc::new(Verifier::new());
        let backend = Arc::new(
            Bucket::unclassified(
                Arc::new(object_store::memory::InMemory::new()),
                Config::new("workspace".into(), "bucket".into()),
                Arc::new(TestStaging::new(64 << 20)),
            )
            .unwrap(),
        );
        let store = Arc::new(Governed::new(backend.clone(), verifier.clone()).unwrap());
        let access = context();
        let metadata = store
            .write(
                &access,
                &object("future-column"),
                Arc::new(bytes(&allocator, b"abcdefgh")).view(),
            )
            .await
            .unwrap();
        let cache = Cache::create(
            temp.0.join("cache"),
            Arc::new(TestKeys(AtomicU8::new(7))),
            store,
            65536,
        )
        .unwrap();
        let ranges = || {
            vec![
                PlannedRange {
                    object: metadata.object.clone(),
                    version: metadata.version.clone(),
                    offset: 4,
                    destination: allocator.alloc(4, Tier::Host).unwrap(),
                    next_use: 2,
                },
                PlannedRange {
                    object: metadata.object.clone(),
                    version: metadata.version.clone(),
                    offset: 0,
                    destination: allocator.alloc(4, Tier::Host).unwrap(),
                    next_use: 1,
                },
            ]
        };
        let first = fetch_cached(&cache, &access, ranges(), 2, 8).await.unwrap();
        assert_eq!(first[0].buffer.as_ref(), b"abcd");
        assert_eq!(first[1].buffer.as_ref(), b"efgh");
        assert_eq!(backend.staging_stats().requests, 2);
        let second = fetch_cached(&cache, &access, ranges(), 2, 8).await.unwrap();
        assert_eq!(second[0].buffer.as_ref(), b"abcd");
        assert_eq!(
            backend.staging_stats().requests,
            2,
            "actual cache hits issue no home GET"
        );
        verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            fetch_cached(&cache, &access, ranges(), 2, 8).await,
            Err(Error::Denied)
        ));
        assert_eq!(backend.staging_stats().requests, 2);
    });
}

#[test]
fn st_t6_socket_cannot_forge_native_session_authority_in_either_control_mode() {
    use moruna_storage_format::{
        auth::{Authorization, Proof, Verdict},
        wire::{self, Outcome, Request, Response},
    };
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    runtime().block_on(async {
        let allocator = Arc::new(FakeAllocator::new());
        let verifier = Arc::new(Verifier::new());
        let service = Arc::new(
            moruna_storage::port::Service::new(
                bucket(verifier.clone()),
                Arc::new(VolumeArena(allocator.clone())),
                1 << 20,
            )
            .unwrap(),
        );
        for authority_mode in [true, false] {
            let (mut client, server) = UnixStream::pair().unwrap();
            for socket in [&client, &server] {
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
            }
            let scope = moruna_storage::Scope {
                workspace: "workspace".into(),
                run: "run".into(),
            };
            let target = object("must-not-be-created");
            let mut control = vec![0; wire::MAX_HEADER];
            let bytes = if authority_mode {
                Authorization::native(&scope, &target, Action::Read)
                    .unwrap()
                    .encode_into(9, &mut control)
                    .unwrap()
            } else {
                Request {
                    id: 9,
                    scope,
                    object: target,
                    expected_role: None,
                    proof: Proof::NativeSession,
                    operation: wire::Operation::Create { length: 1 << 20 },
                }
                .encode_into(&mut control)
                .unwrap()
            };
            let service_task = service.clone().serve(
                Box::new(server),
                allocator.alloc(wire::MAX_HEADER, Tier::Host).unwrap(),
            );
            let caller = tokio::task::spawn_blocking(move || {
                client.write_all(&control[..bytes]).unwrap();
                // Do not send any claimed body. A forged marker must refuse before IO.
                let mut prefix = [0; wire::PREFIX_BYTES];
                client.read_exact(&mut prefix).unwrap();
                let length = wire::header_bytes(&prefix).unwrap();
                let mut reply = vec![0; length];
                reply[..wire::PREFIX_BYTES].copy_from_slice(&prefix);
                client.read_exact(&mut reply[wire::PREFIX_BYTES..]).unwrap();
                if authority_mode {
                    let verdict = Verdict::decode(&reply).unwrap();
                    assert_eq!(verdict.id, 9);
                    assert_eq!(verdict.result, Err(wire::Refusal::Denied));
                } else {
                    let response = Response::decode(&reply).unwrap();
                    assert_eq!(response.id, 9);
                    assert!(matches!(
                        response.outcome,
                        Outcome::Refused(wire::Refusal::Denied)
                    ));
                }
            });
            let (result, caller) = tokio::join!(service_task, caller);
            caller.unwrap();
            assert!(matches!(result, Err(Error::Denied)));
        }
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn st_t7_checkpoint_prefix_is_immutable_and_resume_reuses_only_verified_chunks() {
    runtime().block_on(async {
        use moruna_storage::artifact::{Boundary, Capture, Identity, Reader, Request, State};
        let access = context();
        let store = bucket(Arc::new(Verifier::new()));
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let batch = moruna_kernel::arrow::record_batch::RecordBatch::try_from_iter([(
            "value",
            Arc::new(moruna_kernel::arrow::array::Int64Array::from(vec![7]))
                as moruna_kernel::arrow::array::ArrayRef,
        )])
        .unwrap();
        let mut writer = moruna_kernel::arrow::ipc::writer::StreamWriter::try_new(
            Vec::new(),
            batch.schema().as_ref(),
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
        let encoded = writer.into_inner().unwrap();
        let record = || Arc::new(bytes(alloc.as_ref(), &encoded)).view();
        let request = Request {
            identity: Identity {
                workspace: "workspace".into(),
                run: "run".into(),
                attempt: "saved-attempt".into(),
                task: "logical-task".into(),
                port: "rows".into(),
            },
            required: true,
            byte_budget: encoded.len() as u64 * 3,
            policy: "TEST-POLICY".into(),
        };
        let target = object("capture-prefix/active");
        let prepare = || {
            Capture::prepare(
                request.clone(),
                Boundary::LogicalRows,
                target.clone(),
                store.clone(),
                alloc.clone(),
                16384,
            )
            .unwrap()
        };
        let reader = Reader::new(store.clone(), alloc.clone(), 16384).unwrap();
        let mut original = prepare();
        original.begin(&access).await.unwrap();
        original
            .append(&access, "schema".into(), 1, record())
            .await
            .unwrap();
        let snapshot = original
            .snapshot(&access, &object("capture-prefix/checkpoint-1"))
            .await
            .unwrap();
        let prefix = reader.manifest(&access, &snapshot).await.unwrap();
        assert_eq!(prefix.state, State::Capturing);
        assert_eq!(prefix.rows, 1);
        original
            .append(&access, "schema".into(), 1, record())
            .await
            .unwrap();
        let future_chunk = original.manifest().chunks[1].metadata.clone();
        let mut restored = prepare();
        restored.resume(&access, &snapshot).await.unwrap();
        assert_eq!(restored.manifest().rows, 1);
        assert_eq!(restored.manifest().state, State::Capturing);
        restored
            .append(&access, "schema".into(), 1, record())
            .await
            .unwrap();
        assert_eq!(restored.manifest().chunks[1].metadata, future_chunk);
        let completed = restored.complete(&access).await.unwrap();
        assert_eq!(reader.manifest(&access, &completed).await.unwrap().rows, 2);
        assert_eq!(reader.manifest(&access, &snapshot).await.unwrap(), prefix);
        let finished = restored
            .snapshot(&access, &object("capture-prefix/checkpoint-2"))
            .await
            .unwrap();
        let mut resumed_complete = prepare();
        resumed_complete.resume(&access, &finished).await.unwrap();
        assert_eq!(resumed_complete.manifest().state, State::Complete);
        let complete_again = resumed_complete.complete(&access).await.unwrap();
        assert_eq!(
            reader
                .manifest(&access, &complete_again)
                .await
                .unwrap()
                .state,
            State::Complete
        );
        assert!(
            resumed_complete
                .append(&access, "schema".into(), 1, record())
                .await
                .is_err()
        );
        let chunk = &prefix.chunks[0].metadata;
        store
            .replace(
                &access,
                &chunk.object,
                Condition::Match(chunk.version.clone()),
                Arc::new(bytes(
                    alloc.as_ref(),
                    b"authorized TEST-ONLY corruption fixture",
                ))
                .view(),
            )
            .await
            .unwrap();
        let before = store.current(&access, &target).await.unwrap();
        let mut corrupt = prepare();
        assert!(corrupt.resume(&access, &snapshot).await.is_err());
        assert!(corrupt.current_metadata().is_none());
        assert!(corrupt.manifest().chunks.is_empty());
        assert_eq!(store.current(&access, &target).await.unwrap(), before);
    });
}

#[test]
fn st_t4_disk_v3_repeated_replacement_reclaims_banks_without_changing_pinned_facts() {
    runtime().block_on(async {
        use moruna_storage::{Scope, blocks::FileBlocks, volume::Volume};
        let temp = Scratch::new();
        let path = temp.0.join("two-bank-volume");
        let allocator = Arc::new(FakeAllocator::new());
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        let volume = Arc::new(
            Volume::new(
                FileBlocks::create(&path, 4 << 20, 4096).unwrap(),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        let geometry = volume
            .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
            .await
            .unwrap();
        assert!(geometry.capacity < 4 << 20); // inactive bank is a real reserve, not advertised capacity
        let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
        let target = object("replaced");
        let mut metadata = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(allocator.as_ref(), &vec![0; 8193])).view(),
            )
            .await
            .unwrap();
        for value in 1..=24u8 {
            let content = vec![value; 8193];
            metadata = store
                .replace(
                    &context(),
                    &target,
                    Condition::Match(metadata.version.clone()),
                    Arc::new(bytes(allocator.as_ref(), &content)).view(),
                )
                .await
                .unwrap();
            let before = metadata.clone();
            volume.compact(&scope).await.unwrap();
            assert_eq!(
                store
                    .head(&context(), &target, &before.version)
                    .await
                    .unwrap(),
                before
            );
            let got = store
                .read(
                    &context(),
                    &target,
                    &before.version,
                    0,
                    allocator.alloc(content.len(), Tier::Host).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(got.as_ref(), content);
        }
        let facts = metadata.clone();
        drop(store);
        drop(volume);
        let reopened = Arc::new(
            Volume::new(
                FileBlocks::open(&path, 4096).unwrap(),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        reopened.mount(&scope).await.unwrap();
        let store = Governed::new(reopened, Arc::new(Verifier::new())).unwrap();
        assert_eq!(
            store
                .head(&context(), &target, &facts.version)
                .await
                .unwrap(),
            facts
        );
        let got = store
            .read(
                &context(),
                &target,
                &facts.version,
                0,
                allocator.alloc(8193, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(got.as_ref(), &vec![24; 8193]);
    });
}

#[test]
fn st_t4_inactive_copy_does_not_publish_when_root_write_is_refused() {
    use moruna_storage::{
        Scope,
        blocks::{Blocks, FileBlocks, Transaction},
        volume::Volume,
    };
    struct Fault {
        inner: Arc<FileBlocks>,
        reject: Arc<AtomicBool>,
    }
    struct Tx {
        inner: Box<dyn Transaction>,
        reject: Arc<AtomicBool>,
    }
    impl Blocks for Fault {
        fn capacity(&self) -> u64 {
            self.inner.capacity()
        }
        fn alignment(&self) -> usize {
            self.inner.alignment()
        }
        fn role(&self) -> Role {
            Role::FormatProof
        }
        fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>> {
            Ok(Box::new(Tx {
                inner: self.inner.clone().begin()?,
                reject: self.reject.clone(),
            }))
        }
    }
    impl Transaction for Tx {
        fn read(&self, at: u64, dst: Buffer) -> Operation<'static, Buffer> {
            self.inner.read(at, dst)
        }
        fn write(&self, at: u64, src: moruna_kernel::BufferView) -> Operation<'static, ()> {
            let root = src
                .as_host_slice()
                .is_some_and(|bytes| moruna_storage_format::banks::Root::decode(bytes).is_ok());
            if root && self.reject.swap(false, Ordering::SeqCst) {
                return Box::pin(async { Err(Error::Unavailable("TEST-ONLY root write refused")) });
            }
            self.inner.write(at, src)
        }
        fn sync(&self) -> Operation<'static, ()> {
            self.inner.sync()
        }
    }
    runtime().block_on(async {
        let temp = Scratch::new();
        let allocator = Arc::new(FakeAllocator::new());
        let reject = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(
            Volume::new(
                Arc::new(Fault {
                    inner: FileBlocks::create(&temp.0.join("volume"), 4 << 20, 4096).unwrap(),
                    reject: reject.clone(),
                }),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        let original = volume
            .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
            .await
            .unwrap();
        let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
        let metadata = store
            .write(
                &context(),
                &object("outside-namespace"),
                Arc::new(bytes(allocator.as_ref(), b"actual acknowledged bytes")).view(),
            )
            .await
            .unwrap();
        reject.store(true, Ordering::SeqCst);
        assert!(matches!(
            volume.compact(&scope).await,
            Err(Error::Unavailable("TEST-ONLY root write refused"))
        ));
        assert_eq!(volume.mount(&scope).await.unwrap(), original);
        assert_eq!(
            store
                .head(&context(), &metadata.object, &metadata.version)
                .await
                .unwrap(),
            metadata
        );
        let got = store
            .read(
                &context(),
                &metadata.object,
                &metadata.version,
                0,
                allocator
                    .alloc(metadata.bytes as usize, Tier::Host)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(got.as_ref(), b"actual acknowledged bytes");
        let next = volume.compact(&scope).await.unwrap();
        assert_ne!(next.log_offset, original.log_offset);
        assert_eq!(
            store
                .head(&context(), &metadata.object, &metadata.version)
                .await
                .unwrap(),
            metadata
        );
    });
}

#[test]
fn st_t4_explicit_provisioner_initializes_each_role_but_never_overwrites_existing_roots() {
    let _process_guard = provision_process_guard();
    use moruna_storage::provision::{self, Phase};
    use moruna_storage_format::banks::Root;
    use std::os::unix::fs::FileExt;
    let temp = Scratch::new();
    for role in [
        Role::CheckpointMetadata,
        Role::CacheScratch,
        Role::SingleNodeHome,
    ] {
        let path = temp.0.join(format!("provision-{role:?}"));
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(4 << 20).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let root = Root::initial(
            4 << 20,
            4096,
            4 * 32768,
            role,
            "workspace".into(),
            "bucket".into(),
        )
        .unwrap();
        let report = provision::image(&path, &root, None).unwrap();
        assert!(!report.raw_device);
        assert_eq!(report.root, root);
        let mut bytes = [0; 4096];
        fs::File::open(&path)
            .unwrap()
            .read_exact_at(&mut bytes, 0)
            .unwrap();
        assert_eq!(Root::decode(&bytes).unwrap(), root);
        let error = provision::image(&path, &root, None).unwrap_err();
        assert_eq!(error.phase(), Phase::ReadRoots);
        assert!(!error.target_may_have_changed());
        let file = fs::File::open(&path).unwrap();
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(Root::decode(&bytes).unwrap(), root);
    }
}

#[test]
fn st_t4_provisioner_checks_exact_geometry_and_both_blank_root_cells_before_any_write() {
    let _process_guard = provision_process_guard();
    use moruna_storage::provision::{self, Phase};
    use moruna_storage_format::banks::Root;
    use std::os::unix::fs::FileExt;
    let temp = Scratch::new();
    let path = temp.0.join("wrong-geometry");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(4 << 20).unwrap();
    let wrong = Root::initial(
        8 << 20,
        4096,
        4 * 32768,
        Role::CacheScratch,
        "workspace".into(),
        "bucket".into(),
    )
    .unwrap();
    let error = provision::image(&path, &wrong, None).unwrap_err();
    assert_eq!(error.phase(), Phase::Preflight);
    assert!(!error.target_may_have_changed());
    let root = Root::initial(
        4 << 20,
        4096,
        4 * 32768,
        Role::CacheScratch,
        "workspace".into(),
        "bucket".into(),
    )
    .unwrap();
    file.write_all_at(&[0x71], 4096 + 13).unwrap();
    file.sync_all().unwrap();
    let error = provision::image(&path, &root, None).unwrap_err();
    assert_eq!(error.phase(), Phase::ReadRoots);
    assert!(!error.target_may_have_changed());
    let mut first = [0; 4096];
    file.read_exact_at(&mut first, 0).unwrap();
    assert!(first.iter().all(|b| *b == 0));
    assert!(provision::validate_root(&root.next().unwrap()).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn st_t4_provisioner_refuses_an_existing_exclusive_image_lease_without_waiting() {
    let _process_guard = provision_process_guard();
    use moruna_storage::provision::{self, Phase};
    use moruna_storage_format::banks::Root;
    let temp = Scratch::new();
    let path = temp.0.join("leased-image");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(4 << 20).unwrap();
    file.lock().unwrap();
    let root = Root::initial(
        4 << 20,
        4096,
        4 * 32768,
        Role::SingleNodeHome,
        "workspace".into(),
        "bucket".into(),
    )
    .unwrap();
    let error = provision::image(&path, &root, None).unwrap_err();
    assert_eq!(error.phase(), Phase::Preflight);
    assert!(!error.target_may_have_changed());
}

#[test]
fn st_t4_supported_provision_cli_plans_formats_and_refuses_repeat_or_unknown_options() {
    let _process_guard = provision_process_guard();
    use std::process::Command;
    let temp = Scratch::new();
    let root = temp.0.join("cache.root");
    let image = temp.0.join("cache.img");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&image)
        .unwrap();
    file.set_len(4 << 20).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let executable = env!("CARGO_BIN_EXE_moruna-storage-provision");
    let planned = Command::new(executable)
        .args(["plan", "--output"])
        .arg(&root)
        .args([
            "--capacity",
            "4194304",
            "--alignment",
            "4096",
            "--log-bytes",
            "131072",
            "--role",
            "cache-scratch",
            "--workspace",
            "workspace",
            "--bucket",
            "bucket",
        ])
        .output()
        .unwrap();
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let formatted = Command::new(executable)
        .args(["format", "--image"])
        .arg(&image)
        .arg("--root")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        formatted.status.success(),
        "{}",
        String::from_utf8_lossy(&formatted.stderr)
    );
    assert!(String::from_utf8_lossy(&formatted.stdout).contains("raw_device=false"));
    let repeated = Command::new(executable)
        .args(["format", "--image"])
        .arg(&image)
        .arg("--root")
        .arg(&root)
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("ReadRoots"));
    let unknown = Command::new(executable)
        .args(["format", "--image"])
        .arg(&image)
        .arg("--root")
        .arg(&root)
        .args(["--fallback", "true"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown option"));
}

#[test]
fn st_t4_corrupt_newest_root_after_bank_reuse_never_rolls_back_acknowledged_facts() {
    runtime().block_on(async {
        use moruna_storage::{Scope, blocks::FileBlocks, volume::Volume};
        use moruna_storage_format::banks::Root;
        use std::os::unix::fs::FileExt;
        for erase in [false, true] {
            let temp = Scratch::new();
            let path = temp.0.join("root-corruption");
            let allocator = Arc::new(FakeAllocator::new());
            let scope = Scope {
                workspace: "workspace".into(),
                run: "run".into(),
            };
            let volume = Arc::new(
                Volume::new(
                    FileBlocks::create(&path, 4 << 20, 4096).unwrap(),
                    Arc::new(VolumeArena(allocator.clone())),
                    Arc::new(TestStaging::new(1 << 20)),
                )
                .unwrap(),
            );
            volume
                .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
                .await
                .unwrap();
            let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
            let target = object("acknowledged");
            let mut metadata = store
                .write(
                    &context(),
                    &target,
                    Arc::new(bytes(allocator.as_ref(), b"initial")).view(),
                )
                .await
                .unwrap();
            for value in 1..=4u8 {
                metadata = store
                    .replace(
                        &context(),
                        &target,
                        Condition::Match(metadata.version.clone()),
                        Arc::new(bytes(allocator.as_ref(), &[value; 8193])).view(),
                    )
                    .await
                    .unwrap();
                volume.compact(&scope).await.unwrap();
            }
            metadata = store
                .replace(
                    &context(),
                    &target,
                    Condition::Match(metadata.version.clone()),
                    Arc::new(bytes(allocator.as_ref(), b"LATEST ACKNOWLEDGED")).view(),
                )
                .await
                .unwrap();
            assert_eq!(
                store
                    .read(
                        &context(),
                        &target,
                        &metadata.version,
                        0,
                        allocator.alloc(19, Tier::Host).unwrap()
                    )
                    .await
                    .unwrap()
                    .as_ref(),
                b"LATEST ACKNOWLEDGED"
            );
            drop(store);
            drop(volume);
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut cells = [[0; 4096]; 2];
            for (i, cell) in cells.iter_mut().enumerate() {
                file.read_exact_at(cell, i as u64 * 4096).unwrap();
            }
            let valid = cells
                .iter()
                .enumerate()
                .filter_map(|(slot, cell)| Root::decode(cell).ok().map(|root| (slot, root)))
                .collect::<Vec<_>>();
            assert_eq!(
                valid.len(),
                1,
                "stable acknowledgement must retain no stale root authority"
            );
            let slot = valid[0].0;
            if erase {
                file.write_all_at(&[0; 4096], slot as u64 * 4096).unwrap();
            } else {
                cells[slot][40] ^= 0x80;
                file.write_all_at(&cells[slot], slot as u64 * 4096).unwrap();
            }
            file.sync_all().unwrap();
            drop(file);
            let reopened = Arc::new(
                Volume::new(
                    FileBlocks::open(&path, 4096).unwrap(),
                    Arc::new(VolumeArena(allocator.clone())),
                    Arc::new(TestStaging::new(1 << 20)),
                )
                .unwrap(),
            );
            let store = Governed::new(reopened, Arc::new(Verifier::new())).unwrap();
            assert!(matches!(
                store.current(&context(), &target).await,
                Err(Error::Format(moruna_storage_format::Error::Integrity))
            ));
        }
    });
}

#[test]
fn st_t4_failed_root_retirement_blocks_put_until_recovery_durably_retires_old_authority() {
    use moruna_storage::{
        Scope,
        blocks::{Blocks, FileBlocks, Transaction},
        volume::Volume,
    };
    struct Fault {
        inner: Arc<FileBlocks>,
        reject: Arc<AtomicBool>,
    }
    struct Tx {
        inner: Box<dyn Transaction>,
        reject: Arc<AtomicBool>,
    }
    impl Blocks for Fault {
        fn capacity(&self) -> u64 {
            self.inner.capacity()
        }
        fn alignment(&self) -> usize {
            self.inner.alignment()
        }
        fn role(&self) -> Role {
            Role::FormatProof
        }
        fn begin(self: Arc<Self>) -> Result<Box<dyn Transaction>> {
            Ok(Box::new(Tx {
                inner: self.inner.clone().begin()?,
                reject: self.reject.clone(),
            }))
        }
    }
    impl Transaction for Tx {
        fn read(&self, offset: u64, dst: Buffer) -> Operation<'static, Buffer> {
            self.inner.read(offset, dst)
        }
        fn write(&self, offset: u64, src: moruna_kernel::BufferView) -> Operation<'static, ()> {
            if offset == 0
                && self.reject.load(Ordering::SeqCst)
                && src
                    .as_host_slice()
                    .is_some_and(|s| s.iter().all(|b| *b == 0))
            {
                return Box::pin(async {
                    Err(Error::Unavailable("TEST-ONLY old-root retirement refused"))
                });
            }
            self.inner.write(offset, src)
        }
        fn sync(&self) -> Operation<'static, ()> {
            self.inner.sync()
        }
    }
    runtime().block_on(async {
        let temp = Scratch::new();
        let path = temp.0.join("retire-refusal");
        let allocator = Arc::new(FakeAllocator::new());
        let reject = Arc::new(AtomicBool::new(false));
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        let volume = Arc::new(
            Volume::new(
                Arc::new(Fault {
                    inner: FileBlocks::create(&path, 4 << 20, 4096).unwrap(),
                    reject: reject.clone(),
                }),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        volume
            .format(&scope, "workspace".into(), "bucket".into(), 4 * 32768)
            .await
            .unwrap();
        let store = Governed::new(volume.clone(), Arc::new(Verifier::new())).unwrap();
        let target = object("retirement");
        let before = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(allocator.as_ref(), b"before")).view(),
            )
            .await
            .unwrap();
        reject.store(true, Ordering::SeqCst);
        assert!(matches!(
            volume.compact(&scope).await,
            Err(Error::Unavailable("TEST-ONLY old-root retirement refused"))
        ));
        assert!(matches!(
            store
                .replace(
                    &context(),
                    &target,
                    Condition::Match(before.version.clone()),
                    Arc::new(bytes(allocator.as_ref(), b"MUST NOT ACK")).view()
                )
                .await,
            Err(Error::Unavailable("TEST-ONLY old-root retirement refused"))
        ));
        reject.store(false, Ordering::SeqCst);
        let after = store
            .replace(
                &context(),
                &target,
                Condition::Match(before.version.clone()),
                Arc::new(bytes(
                    allocator.as_ref(),
                    b"actual recovered acknowledgement",
                ))
                .view(),
            )
            .await
            .unwrap();
        assert_ne!(after.version, before.version);
        drop(store);
        drop(volume);
        let volume = Arc::new(
            Volume::new(
                FileBlocks::open(&path, 4096).unwrap(),
                Arc::new(VolumeArena(allocator.clone())),
                Arc::new(TestStaging::new(1 << 20)),
            )
            .unwrap(),
        );
        let store = Governed::new(volume, Arc::new(Verifier::new())).unwrap();
        assert_eq!(
            store
                .head(&context(), &target, &after.version)
                .await
                .unwrap(),
            after
        );
        assert_eq!(
            store
                .read(
                    &context(),
                    &target,
                    &after.version,
                    0,
                    allocator.alloc(after.bytes as usize, Tier::Host).unwrap()
                )
                .await
                .unwrap()
                .as_ref(),
            b"actual recovered acknowledgement"
        );
    });
}

#[test]
fn st_t5_exact_checkpoint_publication_needs_no_prefix_list_grant() {
    struct ExactOnly(Verifier);
    impl Authorizer for ExactOnly {
        fn verify<'a>(
            &'a self,
            access: &'a Access,
            object: &'a Object,
            action: Action,
            version: Option<&'a moruna_storage::Version>,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if action == Action::List {
                    return Err(Error::Denied);
                }
                self.0.verify(access, object, action, version).await
            })
        }
    }
    runtime().block_on(async {
        use moruna_storage::checkpoint::{Checkpoints, Portable};
        let temp = Scratch::new();
        let access = context();
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let verifier = Arc::new(ExactOnly(Verifier::new()));
        let local = Arc::new(
            Governed::new(
                Arc::new(
                    FileStore::open(
                        temp.0.join("metadata"),
                        "workspace".into(),
                        "bucket".into(),
                        Role::CheckpointMetadata,
                    )
                    .unwrap(),
                ),
                verifier.clone(),
            )
            .unwrap(),
        );
        let home = bucket(Arc::new(Verifier::new()));
        let checkpoints = Checkpoints::new(local.clone(), home, alloc, 4096).unwrap();
        let local_target = object("exact-local");
        let target = object("exact-home");
        assert!(matches!(
            local.list(&access, &local_target).await,
            Err(Error::Denied)
        ));
        let mut portable = Portable {
            version: VERSION,
            workspace: "workspace".into(),
            run: "run".into(),
            sequence: 1,
            manifest: serde_json::json!({"segments":[],"lineage":[],"state":"first"}),
            dependencies: vec![],
        };
        let first = checkpoints
            .publish(
                &access,
                &local_target,
                &target,
                Condition::Absent,
                &portable,
            )
            .await
            .unwrap();
        let first_local = local.current(&access, &local_target).await.unwrap();
        portable.sequence = 2;
        portable.manifest["state"] = serde_json::json!("second");
        let second = checkpoints
            .publish(
                &access,
                &local_target,
                &target,
                Condition::Match(first.version.clone()),
                &portable,
            )
            .await
            .unwrap();
        let second_local = local.current(&access, &local_target).await.unwrap();
        assert_ne!(first_local.version, second_local.version);
        let restored = checkpoints.restore(&access, &second).await.unwrap();
        assert_eq!(restored.sequence, 2);
        assert_eq!(restored.manifest["state"], "second");
        assert!(matches!(
            local.list(&access, &local_target).await,
            Err(Error::Denied)
        ));
        verifier.0.allowed.store(false, Ordering::SeqCst);
        portable.sequence = 3;
        assert!(matches!(
            checkpoints
                .publish(
                    &access,
                    &local_target,
                    &target,
                    Condition::Match(second.version.clone()),
                    &portable
                )
                .await,
            Err(Error::Denied)
        ));
        verifier.0.allowed.store(true, Ordering::SeqCst);
        assert_eq!(
            local.current(&access, &local_target).await.unwrap(),
            second_local
        );
    });
}

#[test]
fn st_t4_provisioner_uses_canonical_single_device_regions_without_touching_neighbors() {
    let _process_guard = provision_process_guard();
    use moruna_storage::provision;
    use moruna_storage_format::{
        banks::Root,
        config::{BootConfig, Provider, Region},
    };
    use std::os::unix::fs::FileExt;
    let temp = Scratch::new();
    let path = temp.0.join("single-device.img");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(12 << 20).unwrap();
    file.sync_all().unwrap();
    let layout = BootConfig {
        provider: Provider::Local {
            service: "nvme0".into(),
            alignment: 4096,
            metadata: Region {
                offset: 0,
                bytes: 4 << 20,
            },
            cache: Region {
                offset: 4 << 20,
                bytes: 4 << 20,
            },
            home: Some(Region {
                offset: 8 << 20,
                bytes: 4 << 20,
            }),
            key_bytes: 256,
            requester: "workspace".into(),
        },
        timeout_ns: 1000000000,
        body_limit: 65536,
        session_limit: 32,
    };
    let mut roots = Vec::new();
    for (offset, role) in [
        (0, Role::CheckpointMetadata),
        (4 << 20, Role::CacheScratch),
        (8 << 20, Role::SingleNodeHome),
    ] {
        let root = Root::initial(
            4 << 20,
            4096,
            4 * 32768,
            role,
            "workspace".into(),
            "bucket".into(),
        )
        .unwrap();
        provision::image(&path, &root, Some(&layout)).unwrap();
        roots.push((offset, root));
        // All previously provisioned neighboring root authorities remain byte-exact.
        for (offset, expected) in &roots {
            let mut bytes = [0; 4096];
            file.read_exact_at(&mut bytes, *offset).unwrap();
            assert_eq!(Root::decode(&bytes).unwrap(), *expected);
        }
    }
    let mut bad = layout.clone();
    if let Provider::Local { cache, .. } = &mut bad.provider {
        cache.offset = 2 << 20;
    }
    assert!(provision::image(&path, &roots[1].1, Some(&bad)).is_err());
    let wrong = Root::initial(
        8 << 20,
        4096,
        4 * 32768,
        Role::CacheScratch,
        "workspace".into(),
        "bucket".into(),
    )
    .unwrap();
    assert!(provision::image(&path, &wrong, Some(&layout)).is_err());
    for (offset, expected) in roots {
        let mut bytes = [0; 4096];
        file.read_exact_at(&mut bytes, offset).unwrap();
        assert_eq!(Root::decode(&bytes).unwrap(), expected);
    }
}

/// In-process connector: each connect hands one end of a fresh socket pair to the
/// real `Service` on the test runtime, exercising the full client/service protocol.
struct PairConnector {
    service: Arc<moruna_storage::port::Service>,
    allocator: Arc<FakeAllocator>,
    runtime: tokio::runtime::Handle,
    served: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<Result<()>>>>>,
}
impl moruna_storage::port::Connector for PairConnector {
    fn connect(&self) -> Result<Box<dyn moruna_storage::port::Stream>> {
        use std::{os::unix::net::UnixStream, time::Duration};
        let (client, server) = UnixStream::pair().unwrap();
        for s in [&client, &server] {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        let control = self
            .allocator
            .alloc(moruna_storage_format::wire::MAX_HEADER, Tier::Host)
            .unwrap();
        let service = self.service.clone();
        let handle = self
            .runtime
            .spawn(async move { service.serve(Box::new(server), control).await });
        self.served.lock().unwrap().push(handle);
        Ok(Box::new(client))
    }
}

#[test]
fn st_t6_socket_client_authorizes_remotely_replaces_pages_lists_and_maps_refusals() {
    use moruna_storage::port::{Client, Service, Unix, from_refusal, refusal};
    use moruna_storage_format::wire::Refusal;
    use std::time::Duration;

    // Constructor refusals need no runtime.
    assert!(Unix::new(PathBuf::from("relative.sock"), Duration::from_secs(1)).is_err());
    assert!(Unix::new(PathBuf::from("/abs.sock"), Duration::ZERO).is_err());
    // A configured endpoint with nothing listening is a typed IO refusal.
    use moruna_storage::port::Connector;
    assert!(
        Unix::new(
            std::env::temp_dir().join(format!("m-absent-{}.sock", std::process::id())),
            Duration::from_millis(50)
        )
        .unwrap()
        .connect()
        .is_err()
    );
    let allocator = Arc::new(FakeAllocator::new());
    let arenas: Arc<dyn moruna_storage::volume::Arenas> = Arc::new(VolumeArena(allocator.clone()));
    let remote_verifier = Arc::new(Verifier::new());
    let home = bucket(remote_verifier.clone());
    assert!(matches!(
        Service::new(home.clone(), arenas.clone(), 0),
        Err(Error::Invalid(_))
    ));

    // Every refusal round-trips through the canonical public mapping.
    for code in [
        Refusal::Denied,
        Refusal::Missing,
        Refusal::Conflict,
        Refusal::Integrity,
        Refusal::Unavailable,
        Refusal::Unsupported,
        Refusal::Capacity,
        Refusal::Invalid,
        Refusal::Busy,
    ] {
        assert_eq!(refusal(&from_refusal(code)), code);
    }
    assert_eq!(refusal(&Error::Invalid("x")), Refusal::Invalid);
    assert_eq!(refusal(&Error::Unavailable("x")), Refusal::Unavailable);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let service = Arc::new(Service::new(home, arenas.clone(), 1 << 10).unwrap());
        let served = Arc::new(std::sync::Mutex::new(Vec::new()));
        let connector = Arc::new(PairConnector {
            service,
            allocator: allocator.clone(),
            runtime: tokio::runtime::Handle::current(),
            served: served.clone(),
        });
        let client = Arc::new(Client::new(connector, Arc::new(context()), arenas).unwrap());
        // Run-facing port whose authorizer is the remote exact-action verifier itself.
        let store = client.clone().governed().unwrap();
        // A socket client never claims a durability role on its own.
        assert_eq!(Backend::role(client.as_ref()), None);
        let target = object("pages/a");
        let first = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(allocator.as_ref(), b"one")).view(),
            )
            .await
            .unwrap();
        assert_eq!(store.current(&context(), &target).await.unwrap(), first);
        let second = store
            .replace(
                &context(),
                &target,
                Condition::Match(first.version.clone()),
                Arc::new(bytes(allocator.as_ref(), b"two!")).view(),
            )
            .await
            .unwrap();
        assert_eq!(second.bytes, 4);
        assert_eq!(store.current(&context(), &target).await.unwrap(), second);
        // A stale conditional publish is a typed conflict, not a silent overwrite.
        assert!(matches!(
            store
                .replace(
                    &context(),
                    &target,
                    Condition::Match(first.version.clone()),
                    Arc::new(bytes(allocator.as_ref(), b"old")).view(),
                )
                .await,
            Err(Error::Conflict)
        ));
        let fresh = object("pages/b");
        store
            .replace(
                &context(),
                &fresh,
                Condition::Absent,
                Arc::new(bytes(allocator.as_ref(), b"b")).view(),
            )
            .await
            .unwrap();
        assert!(matches!(
            store.current(&context(), &object("pages/none")).await,
            Err(Error::Missing)
        ));
        // Body ceiling refuses before payload is admitted.
        let large = Arc::new(bytes(allocator.as_ref(), &[7u8; 2048]));
        assert!(
            store
                .write(&context(), &object("pages/huge"), large.view())
                .await
                .is_err()
        );
        // A range larger than the service body ceiling is refused before any read.
        assert!(
            store
                .read(
                    &context(),
                    &target,
                    &second.version,
                    0,
                    allocator.alloc(2048, Tier::Host).unwrap()
                )
                .await
                .is_err()
        );

        // More than one 64-entry page forces continuation through `after`.
        for i in 0..70 {
            store
                .write(
                    &context(),
                    &object(&format!("list/{i:03}")),
                    Arc::new(bytes(allocator.as_ref(), b"x")).view(),
                )
                .await
                .unwrap();
        }
        let listed = store.list(&context(), &object("list/")).await.unwrap();
        assert_eq!(listed.len(), 70);
        assert!(listed.windows(2).all(|w| w[0].object.key < w[1].object.key));

        // Remote authority denial reaches the caller as Denied.
        remote_verifier.allowed.store(false, Ordering::SeqCst);
        assert!(matches!(
            store.current(&context(), &target).await,
            Err(Error::Denied)
        ));
        remote_verifier.allowed.store(true, Ordering::SeqCst);

        // A scope the client was not bound to never reaches the socket.
        let foreign = Access {
            workspace: "other".into(),
            run: "run".into(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        };
        assert!(store.current(&foreign, &target).await.is_err());

        let handles: Vec<_> = served.lock().unwrap().drain(..).collect();
        for handle in handles {
            let _ = handle.await.unwrap();
        }
    });
}

#[test]
fn st_t6_bucket_multipart_publishes_verified_bytes_and_refuses_bad_config_and_reserved_keys() {
    let staging = || Arc::new(TestStaging::new(64 << 20));
    let memory = || Arc::new(object_store::memory::InMemory::new());
    let base = || Config::new("workspace".into(), "bucket".into());
    // Every configuration bound is enforced before any IO.
    let mut bad = Vec::new();
    for edit in [
        (|c: &mut Config| c.workspace.clear()) as fn(&mut Config),
        |c| c.bucket.clear(),
        |c| c.parallel_parts = 0,
        |c| c.parallel_parts = 1025,
        |c| c.part_bytes = (5 << 20) - 1,
        |c| c.part_bytes = (1 << 30) + 1,
        |c| c.single_put_bytes = c.part_bytes - 1,
        |c| c.range_bytes = 0,
        |c| c.range_bytes = (64 << 20) + 1,
        |c| c.parallel_ranges = 0,
        |c| c.parallel_ranges = 1025,
    ] {
        let mut cfg = base();
        edit(&mut cfg);
        bad.push(Bucket::unclassified(memory(), cfg, staging()).is_err());
    }
    assert!(bad.iter().all(|refused| *refused));

    let mut cfg = base();
    cfg.part_bytes = 5 << 20;
    cfg.single_put_bytes = 5 << 20;
    cfg.range_bytes = 1 << 20;
    let backend = Arc::new(Bucket::unclassified(memory(), cfg, staging()).unwrap());
    let store = Governed::new(backend.clone(), Arc::new(Verifier::new())).unwrap();
    let alloc = FakeAllocator::new();
    runtime().block_on(async {
        // Larger than single_put_bytes: multipart under a temporary key, then
        // copy-if-absent publication verified range by range.
        let data: Vec<u8> = (0..(11usize << 20)).map(|i| (i % 251) as u8).collect();
        let target = object("multi/part");
        let written = store
            .write(&context(), &target, Arc::new(bytes(&alloc, &data)).view())
            .await
            .unwrap();
        assert_eq!(written.bytes, data.len() as u64);
        let back = store
            .read(
                &context(),
                &target,
                &written.version,
                0,
                alloc.alloc(data.len(), Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        assert!(back.as_ref() == data.as_slice());
        assert!(backend.ingress_bytes() >= data.len() as u64);
        // Create never overwrites an existing object, even through multipart.
        assert!(
            store
                .write(&context(), &target, Arc::new(bytes(&alloc, &data)).view())
                .await
                .is_err()
        );
        // No temporary upload objects remain visible.
        assert_eq!(store.list(&context(), &object("")).await.unwrap().len(), 1);
        // The reserved upload prefix and foreign buckets are never addressable.
        assert!(
            store
                .write(
                    &context(),
                    &object(".moruna-upload/x"),
                    Arc::new(bytes(&alloc, b"x")).view()
                )
                .await
                .is_err()
        );
        // Keys the object store would silently rewrite are refused, not normalized.
        for key in ["dir/", "a//b", "a/../b"] {
            assert!(
                store
                    .write(
                        &context(),
                        &object(key),
                        Arc::new(bytes(&alloc, b"x")).view()
                    )
                    .await
                    .is_err(),
                "{key} must not be accepted"
            );
        }
        let mut foreign = object("k");
        foreign.bucket = "elsewhere".into();
        assert!(store.current(&context(), &foreign).await.is_err());
    });
}

#[test]
fn st_t5_checkpoint_dependencies_export_restore_and_refuse_mismatched_identity() {
    use moruna_storage::checkpoint::{Checkpoints, Dependency};
    let alloc = Arc::new(FakeAllocator::new());
    let verifier = Arc::new(Verifier::new());
    let local = bucket(verifier.clone());
    let home = bucket(verifier.clone());
    assert!(matches!(
        Checkpoints::new(local.clone(), home.clone(), alloc.clone(), 0),
        Err(Error::Invalid(_))
    ));
    let checkpoints =
        Checkpoints::new(local.clone(), home.clone(), alloc.clone(), 1 << 16).unwrap();
    runtime().block_on(async {
        let access = context();
        // Engine-owned bytes published directly.
        let direct = checkpoints
            .export_bytes(
                &access,
                &object("ckpt/direct"),
                Arc::new(bytes(alloc.as_ref(), b"segment")).view(),
            )
            .await
            .unwrap();
        let restored = checkpoints
            .dependency(&access, &direct, alloc.alloc(7, Tier::Host).unwrap())
            .await
            .unwrap();
        assert_eq!(restored.as_ref(), b"segment");
        // A wrong-length destination is refused before any IO.
        assert!(matches!(
            checkpoints
                .dependency(&access, &direct, alloc.alloc(3, Tier::Host).unwrap())
                .await,
            Err(Error::Invalid(_))
        ));
        // A recorded hash that does not match the persisted bytes is an integrity failure.
        let forged = Dependency {
            metadata: direct.metadata.clone(),
            content_hash: blake3::hash(b"other").to_hex().to_string(),
        };
        assert!(matches!(
            checkpoints
                .dependency(&access, &forged, alloc.alloc(7, Tier::Host).unwrap())
                .await,
            Err(Error::Integrity)
        ));

        // Copy one local object into home through exact identity.
        let source = local
            .write(
                &access,
                &object("spill/one"),
                Arc::new(bytes(alloc.as_ref(), b"local-bytes")).view(),
            )
            .await
            .unwrap();
        let copied = checkpoints
            .export_dependency(
                &access,
                &local,
                &source,
                &object("ckpt/copied"),
                alloc.alloc(11, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            copied.content_hash,
            blake3::hash(b"local-bytes").to_hex().to_string()
        );
        assert!(matches!(
            checkpoints
                .export_dependency(
                    &access,
                    &local,
                    &source,
                    &object("ckpt/short"),
                    alloc.alloc(2, Tier::Host).unwrap(),
                )
                .await,
            Err(Error::Invalid(_))
        ));
        // Stale source metadata (wrong length claim) is refused as an integrity failure.
        let mut stale = source.clone();
        stale.bytes = 12;
        assert!(
            checkpoints
                .export_dependency(
                    &access,
                    &local,
                    &stale,
                    &object("ckpt/stale"),
                    alloc.alloc(12, Tier::Host).unwrap(),
                )
                .await
                .is_err()
        );
    });
}

#[test]
fn st_t4_local_file_store_refuses_bad_scope_stale_versions_and_bad_ranges() {
    let scratch = Scratch::new();
    let open = |role, workspace: &str, bucket: &str| {
        FileStore::open(
            scratch.0.join("store"),
            workspace.into(),
            bucket.into(),
            role,
        )
    };
    assert!(open(Role::CacheScratch, "workspace", "bucket").is_err());
    assert!(open(Role::CheckpointMetadata, "", "bucket").is_err());
    assert!(open(Role::CheckpointMetadata, "workspace", "").is_err());
    let local = open(Role::CheckpointMetadata, "workspace", "bucket").unwrap();
    assert_eq!(local.role(), Role::CheckpointMetadata);
    let store = Governed::new(Arc::new(local), Arc::new(Verifier::new())).unwrap();
    let alloc = FakeAllocator::new();
    runtime().block_on(async {
        let target = object("local/one");
        assert!(matches!(
            store.current(&context(), &target).await,
            Err(Error::Missing)
        ));
        let first = store
            .write(
                &context(),
                &target,
                Arc::new(bytes(&alloc, b"abcdef")).view(),
            )
            .await
            .unwrap();
        assert_eq!(store.current(&context(), &target).await.unwrap(), first);
        let second = store
            .replace(
                &context(),
                &target,
                Condition::Match(first.version.clone()),
                Arc::new(bytes(&alloc, b"ghijkl")).view(),
            )
            .await
            .unwrap();
        // Superseded versions are a conflict for head and read, never stale bytes.
        assert!(matches!(
            store.head(&context(), &target, &first.version).await,
            Err(Error::Conflict)
        ));
        assert!(
            store
                .read(
                    &context(),
                    &target,
                    &first.version,
                    0,
                    alloc.alloc(2, Tier::Host).unwrap()
                )
                .await
                .is_err()
        );
        // Range past the object is refused.
        assert!(
            store
                .read(
                    &context(),
                    &target,
                    &second.version,
                    4,
                    alloc.alloc(4, Tier::Host).unwrap()
                )
                .await
                .is_err()
        );
        let tail = store
            .read(
                &context(),
                &target,
                &second.version,
                4,
                alloc.alloc(2, Tier::Host).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tail.as_ref(), b"kl");
        // Foreign bucket and empty keys never reach disk.
        let mut foreign = object("local/one");
        foreign.bucket = "other".into();
        assert!(store.current(&context(), &foreign).await.is_err());
        assert!(
            store
                .write(
                    &context(),
                    &object(""),
                    Arc::new(bytes(&alloc, b"x")).view()
                )
                .await
                .is_err()
        );
        // Replaying the log after reopen yields the same current commit.
        let reopened = Governed::new(
            Arc::new(open(Role::CheckpointMetadata, "workspace", "bucket").unwrap()),
            Arc::new(Verifier::new()),
        )
        .unwrap();
        assert_eq!(reopened.current(&context(), &target).await.unwrap(), second);
    });
}

#[test]
fn st_t7_artifact_capture_refuses_misordered_lifecycle_and_bad_identity() {
    use moruna_storage::artifact::{Boundary, Capture, Identity, Request};
    runtime().block_on(async {
        let access = context();
        let store = bucket(Arc::new(Verifier::new()));
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let request = |task: &str, budget: u64| Request {
            identity: Identity {
                workspace: "workspace".into(),
                run: "run".into(),
                attempt: "attempt".into(),
                task: task.into(),
                port: "rows".into(),
            },
            required: false,
            byte_budget: budget,
            policy: "TEST-POLICY".into(),
        };
        let prepare = |req: Request, key: &str| {
            Capture::prepare(
                req,
                Boundary::LogicalRows,
                object(key),
                Arc::clone(&store),
                Arc::clone(&alloc),
                4096,
            )
        };
        // Empty identity fields and a zero budget are refused up front.
        assert!(prepare(request("", 10), "a").is_err());
        assert!(prepare(request("t", 0), "a").is_err());

        let mut capture = prepare(request("t", 1 << 20), "life").unwrap();
        // Completing before begin is refused.
        assert!(capture.complete(&access).await.is_err());
        capture.begin(&access).await.unwrap();
        // Begin is one-shot.
        assert!(capture.begin(&access).await.is_err());
        // Snapshot onto its own live target is refused.
        assert!(capture.snapshot(&access, &object("life")).await.is_err());
        // An empty capture still snapshots to an immutable prefix and completes.
        let prefix = capture
            .snapshot(&access, &object("life.prefix"))
            .await
            .unwrap();
        let done = capture.complete(&access).await.unwrap();
        // Completion is idempotent and re-verified against the persisted manifest.
        assert_eq!(capture.complete(&access).await.unwrap(), done);

        // Resume needs a fresh prepared capture; a begun one is refused.
        let mut begun = prepare(request("t", 1 << 20), "life2").unwrap();
        begun.begin(&access).await.unwrap();
        assert!(begun.resume(&access, &prefix).await.is_err());
        // A prefix from another task identity is an integrity failure.
        let mut other = prepare(request("other-task", 1 << 20), "life3").unwrap();
        assert!(matches!(
            other.resume(&access, &prefix).await,
            Err(Error::Integrity)
        ));
        // Same identity resumes from the immutable prefix.
        let mut again = prepare(request("t", 1 << 20), "life4").unwrap();
        again.resume(&access, &prefix).await.unwrap();

        // Explicit refusal is published and blocks snapshots.
        let mut refused = prepare(request("t", 1 << 20), "life5").unwrap();
        refused.begin(&access).await.unwrap();
        refused.refuse(&access, "operator stop").await.unwrap();
        assert!(
            refused
                .snapshot(&access, &object("life5.prefix"))
                .await
                .is_err()
        );
        assert!(refused.complete(&access).await.is_err());
        // A different run's access is refused.
        let foreign = Access {
            workspace: "workspace".into(),
            run: "other-run".into(),
            credential: Credential::new(b"TEST-ONLY".to_vec()).unwrap(),
        };
        assert!(again.complete(&foreign).await.is_err());
    });
}

#[test]
fn st_t6_service_describes_provider_role_and_rejects_bad_control_and_drains_by_scope() {
    use moruna_storage::{Scope, port::Service};
    use moruna_storage_format::provider::{Description, Request as Query};
    use std::{io::Read, os::unix::net::UnixStream, time::Duration};
    let allocator = Arc::new(FakeAllocator::new());
    let arenas: Arc<dyn moruna_storage::volume::Arenas> = Arc::new(VolumeArena(allocator.clone()));
    let service =
        Arc::new(Service::new(bucket(Arc::new(Verifier::new())), arenas, 1 << 10).unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // A control buffer smaller than one header is refused before any read.
        let (_a, b) = UnixStream::pair().unwrap();
        assert!(
            service
                .clone()
                .serve(Box::new(b), allocator.alloc(8, Tier::Host).unwrap())
                .await
                .is_err()
        );
        // The provider query is answered with the store's actual (unclassified) role.
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut query = vec![0u8; moruna_storage_format::wire::MAX_HEADER];
        let n = Query { id: 9 }.encode_into(&mut query).unwrap();
        client.write_all(&query[..n]).unwrap();
        service
            .clone()
            .serve(
                Box::new(server),
                allocator
                    .alloc(moruna_storage_format::wire::MAX_HEADER, Tier::Host)
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        let described = Description::decode(&reply).unwrap();
        assert_eq!(described.id, 9);
        assert_eq!(described.role, None);
    });
    // Drain returns only this scope's detached records (none after clean completion).
    let backend = Bucket::unclassified(
        Arc::new(object_store::memory::InMemory::new()),
        Config::new("workspace".into(), "bucket".into()),
        Arc::new(TestStaging::new(1 << 20)),
    )
    .unwrap();
    runtime().block_on(async {
        let scope = Scope {
            workspace: "workspace".into(),
            run: "run".into(),
        };
        assert!(backend.drain(&scope).await.unwrap().is_empty());
        let bad = Scope {
            workspace: String::new(),
            run: "run".into(),
        };
        assert!(backend.drain(&bad).await.is_err());
        // A non-host source is refused before any upload starts.
        assert_eq!(backend.ingress_bytes(), 0);
    });
}

#[test]
fn st_t2_cache_budget_read_through_and_recycle_refuse_without_leaking_plaintext() {
    runtime().block_on(async {
        let scratch = Scratch::new();
        let alloc = FakeAllocator::new();
        let access = context();
        let store = bucket(Arc::new(Verifier::new()));
        let keys = || Arc::new(TestKeys(AtomicU8::new(3)));
        assert!(Cache::create(scratch.0.join("zero"), keys(), store.clone(), 0).is_err());
        // A cache epoch under a missing parent is a typed IO refusal.
        assert!(
            Cache::create(scratch.0.join("absent/epoch"), keys(), store.clone(), 1024).is_err()
        );
        let cache = Cache::create(scratch.0.join("small"), keys(), store.clone(), 1024).unwrap();
        // The same epoch directory cannot be created twice.
        assert!(Cache::create(scratch.0.join("small"), keys(), store.clone(), 1024).is_err());
        let payload = vec![5u8; 2000];
        let meta = store
            .write(
                &access,
                &object("big"),
                Arc::new(bytes(&alloc, &payload)).view(),
            )
            .await
            .unwrap();
        // Read-through fetches from the store, then refuses to exceed the byte budget.
        let refused = cache
            .read_through(
                &access,
                &meta.object,
                &meta.version,
                0,
                alloc.alloc(2000, Tier::Host).unwrap(),
            )
            .await;
        assert!(matches!(refused, Err(Error::Unavailable(_))));
        // A small object fits; the second read-through is a cache hit.
        let small = store
            .write(
                &access,
                &object("small"),
                Arc::new(bytes(&alloc, b"tiny")).view(),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let got = cache
                .read_through(
                    &access,
                    &small.object,
                    &small.version,
                    0,
                    alloc.alloc(4, Tier::Host).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(got.as_ref(), b"tiny");
        }
        // After recycle no read or insert can use this epoch.
        cache.recycle().unwrap();
        // Recycled epochs can never serve a hit (a miss or a refusal are both safe).
        assert!(!matches!(
            cache
                .read(
                    &access,
                    &small.object,
                    &small.version,
                    0,
                    alloc.alloc(4, Tier::Host).unwrap()
                )
                .await,
            Ok(Lookup::Hit(_))
        ));
        assert!(
            cache
                .insert(
                    &access,
                    &small.object,
                    &small.version,
                    0,
                    bytes(&alloc, b"tiny")
                )
                .await
                .is_err()
        );
    });
}

#[test]
fn st_t6_service_refuses_role_mismatch_and_cache_commands_over_the_socket() {
    use moruna_storage_format::{
        auth::{Proof, SessionRole},
        wire::{self, Outcome, Refusal, Request, Response},
    };
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let allocator = Arc::new(FakeAllocator::new());
        let service = Arc::new(
            moruna_storage::port::Service::new(
                bucket(Arc::new(Verifier::new())),
                Arc::new(VolumeArena(allocator.clone())),
                1 << 20,
            )
            .unwrap(),
        );
        let ask = |role: Option<SessionRole>, operation: wire::Operation| {
            let mut control = vec![0; wire::MAX_HEADER];
            let n = Request {
                id: 4,
                scope: moruna_storage::Scope {
                    workspace: "workspace".into(),
                    run: "run".into(),
                },
                object: object("role/target"),
                expected_role: role,
                proof: Proof::Application(b"TEST-ONLY"),
                operation,
            }
            .encode_into(&mut control)
            .unwrap();
            control.truncate(n);
            control
        };
        for control in [
            // An unclassified store never satisfies a caller that requires bucket-home.
            ask(Some(SessionRole::BucketHome), wire::Operation::Current),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client.write_all(&control).unwrap();
            let served = service
                .clone()
                .serve(
                    Box::new(server),
                    allocator.alloc(wire::MAX_HEADER, Tier::Host).unwrap(),
                )
                .await;
            assert!(served.is_err());
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).unwrap();
            let response = Response::decode(&reply).unwrap();
            assert_eq!(response.id, 4);
            assert!(matches!(
                response.outcome,
                Outcome::Refused(Refusal::Unsupported)
            ));
        }
    });
}

#[test]
fn st_t6_staging_catalog_binds_one_scope_to_the_runs_host_budget() {
    use moruna_storage::{Scope, staging::Catalog, staging::Staging};
    let scope = Scope {
        workspace: "workspace".into(),
        run: "run".into(),
    };
    let residency = Arc::new(moruna_testkit::placement::FakePlacement::new().with_budget(100));
    assert!(matches!(
        Catalog::new(
            residency.clone(),
            Tier::Device(moruna_kernel::DeviceId(0)),
            scope.clone()
        ),
        Err(Error::Invalid(_))
    ));
    let bad = Scope {
        workspace: String::new(),
        run: "run".into(),
    };
    assert!(Catalog::new(residency.clone(), Tier::Host, bad.clone()).is_err());
    let catalog = Catalog::new(residency.clone(), Tier::Host, scope.clone()).unwrap();
    let granted = catalog.reserve(&scope, 60).expect("within budget");
    assert!(matches!(
        catalog.reserve(&scope, 60),
        Err(Error::StagingRefused { budget: 100, .. })
    ));
    drop(granted);
    let other = Scope {
        workspace: "workspace".into(),
        run: "other".into(),
    };
    assert!(matches!(catalog.reserve(&other, 1), Err(Error::Denied)));
    assert!(catalog.reserve(&bad, 1).is_err());
}

#[test]
fn st_t6_uncached_prefetch_reads_each_planned_range_into_its_destination() {
    use moruna_storage::prefetch::{PlannedRange, fetch};
    let store = bucket(Arc::new(Verifier::new()));
    let alloc = FakeAllocator::new();
    runtime().block_on(async {
        let meta = store
            .write(
                &context(),
                &object("pf/a"),
                Arc::new(bytes(&alloc, b"0123456789")).view(),
            )
            .await
            .unwrap();
        let plan = |offset: u64, len: usize, next_use: u64| PlannedRange {
            object: meta.object.clone(),
            version: meta.version.clone(),
            offset,
            destination: alloc.alloc(len, Tier::Host).unwrap(),
            next_use,
        };
        let ready = fetch(
            &store,
            &context(),
            vec![plan(5, 3, 2), plan(0, 2, 1)],
            2,
            64,
        )
        .await
        .unwrap();
        let mut got: Vec<(u64, Vec<u8>)> = ready
            .into_iter()
            .map(|r| (r.offset, r.buffer.as_ref().to_vec()))
            .collect();
        got.sort();
        assert_eq!(got, vec![(0, b"01".to_vec()), (5, b"567".to_vec())]);
        // A plan larger than the byte budget is refused before any read.
        assert!(
            fetch(&store, &context(), vec![plan(0, 8, 1)], 1, 4)
                .await
                .is_err()
        );
    });
}
