//! Explicit durable-format fixture for native portable engine tests, not cloud/device proof.
use moruna_kernel::Allocator;
use moruna_runtime::portable::{EngineCheckpoints, Publisher};
use moruna_storage::{
    Access, Action, Authorizer, Credential, Governed, Metadata, Object, Operation, Role,
    checkpoint::{Checkpoints, Portable},
    local::FileStore,
};
use moruna_testkit::FakeAllocator;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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
                && access.workspace == "po-t8"
                && object.workspace == "po-t8"
                && access.credential.application() == Some(&b"TEST-ONLY-PO-T8"[..])
            {
                Ok(())
            } else {
                Err(moruna_storage::Error::Denied)
            }
        })
    }
}
fn object(key: &str) -> Object {
    Object {
        workspace: "po-t8".into(),
        bucket: "home".into(),
        key: key.into(),
    }
}
fn governed(path: PathBuf, role: Role, verifier: Arc<Verifier>) -> Arc<Governed> {
    Arc::new(
        Governed::new(
            Arc::new(FileStore::open(path, "po-t8".into(), "home".into(), role).unwrap()),
            verifier,
        )
        .unwrap(),
    )
}
pub struct Fixture {
    pub publisher: Arc<Publisher>,
    access: Arc<Access>,
    authority: Arc<Checkpoints>,
    home: Arc<Governed>,
    alloc: Arc<dyn Allocator>,
}
impl Fixture {
    pub fn open(root: &Path, node: &str, run: &str) -> Self {
        let verifier = Arc::new(Verifier(AtomicBool::new(true)));
        let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
        let home = governed(
            root.join("governed-format-home"),
            Role::FormatProof,
            verifier.clone(),
        );
        let local = governed(
            root.join(format!("{node}-governed-metadata")),
            Role::CheckpointMetadata,
            verifier,
        );
        let access = Arc::new(Access {
            workspace: "po-t8".into(),
            run: run.into(),
            credential: Credential::new(b"TEST-ONLY-PO-T8".to_vec()).unwrap(),
        });
        let authority =
            Arc::new(Checkpoints::new(local, home.clone(), alloc.clone(), 1 << 20).unwrap());
        let publisher = Arc::new(
            Publisher::new(
                authority.clone(),
                access.clone(),
                object("local-checkpoint"),
                object("checkpoint"),
                64 << 10,
                1 << 20,
                4 << 20,
                Duration::from_secs(20),
            )
            .unwrap(),
        );
        Self {
            publisher,
            access,
            authority,
            home,
            alloc,
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    pub fn latest(&self) -> Option<Metadata> {
        Self::runtime()
            .block_on(self.home.list(&self.access, &object("checkpoint")))
            .unwrap()
            .into_iter()
            .find(|metadata| metadata.object == object("checkpoint"))
    }
    pub fn read(&self, pinned: &Metadata) -> Portable {
        Self::runtime()
            .block_on(self.authority.restore(&self.access, pinned))
            .unwrap()
    }
    pub fn restore(&self, pinned: &Metadata, staging: &Path) -> PathBuf {
        let bridge = EngineCheckpoints::new(
            self.authority.clone(),
            self.alloc.clone(),
            64 << 10,
            1 << 20,
        )
        .unwrap();
        Self::runtime().block_on(async {
            let manifest = bridge.restore(&self.access, pinned, staging).await.unwrap();
            self.publisher.resume_at(pinned).await.unwrap();
            manifest
        })
    }
}
