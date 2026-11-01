//! Governed native read-through wiring. Storage owns cache identities, keys, encryption,
//! epoch invalidation and the actual device; this adapter only orders real home authority
//! before each cache lookup and preserves the caller's charged allocation.
use moruna_kernel::{Buffer, BufferView};
use moruna_storage::{
    Access, Action, Authorizer, Backend, Condition, Error, Governed, Metadata, Object, Operation,
    Scope, Version, cache::Lookup,
};
use std::sync::Arc;

trait Ranges: Send + Sync {
    fn lookup<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Lookup>;
    fn fill<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        source: BufferView,
    ) -> Operation<'a, ()>;
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl Ranges for moruna_storage::native::Client {
    fn lookup<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Lookup> {
        Box::pin(self.cache_lookup(access, object, version, offset, destination))
    }
    fn fill<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        source: BufferView,
    ) -> Operation<'a, ()> {
        Box::pin(self.cache_fill(access, object, version, offset, source))
    }
}
struct Cached {
    home: Arc<Governed>,
    cache: Arc<dyn Ranges>,
    access: Arc<Access>,
}
impl Cached {
    fn scope(&self, scope: &Scope) -> moruna_storage::Result<()> {
        if scope.workspace != self.access.workspace || scope.run != self.access.run {
            return Err(Error::Denied);
        }
        Ok(())
    }
    fn wrap(
        home: Arc<Governed>,
        cache: Arc<dyn Ranges>,
        access: Arc<Access>,
    ) -> moruna_storage::Result<Arc<Governed>> {
        let owner = Arc::new(Self {
            home,
            cache,
            access,
        });
        Ok(Arc::new(Governed::new(owner.clone(), owner)?))
    }
}
/// Only a real admitted CacheScratch connection can construct production native wiring.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn native(
    home: Arc<Governed>,
    cache: Arc<moruna_storage::native::Client>,
    access: Arc<Access>,
) -> moruna_storage::Result<Arc<Governed>> {
    if cache.role() != moruna_storage_format::auth::SessionRole::CacheScratch
        || cache.scope().workspace != access.workspace
        || cache.scope().run != access.run
    {
        return Err(Error::Denied);
    }
    Cached::wrap(home, cache, access)
}
impl Authorizer for Cached {
    fn verify<'a>(
        &'a self,
        access: &'a Access,
        object: &'a Object,
        action: Action,
        version: Option<&'a Version>,
    ) -> Operation<'a, ()> {
        // Never replace the caller's authority with the adapter's retained authority.
        Box::pin(self.home.authorize(access, object, action, version))
    }
}
impl Backend for Cached {
    fn role(&self) -> Option<moruna_storage_format::auth::SessionRole> {
        self.home.role()
    }
    fn current<'a>(&'a self, scope: &'a Scope, object: &'a Object) -> Operation<'a, Metadata> {
        Box::pin(async move {
            self.scope(scope)?;
            self.home.current(&self.access, object).await
        })
    }
    fn head<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            self.scope(scope)?;
            self.home.head(&self.access, object, version).await
        })
    }
    fn read<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        version: &'a Version,
        offset: u64,
        destination: Buffer,
    ) -> Operation<'a, Buffer> {
        Box::pin(async move {
            self.scope(scope)?;
            // This is a fresh actual HOME decision/readback, even if the range is cached.
            // A cached success, name match or unpinned current version cannot substitute it.
            let home = self.home.head(&self.access, object, version).await?;
            if home.object != *object || home.version != *version {
                return Err(Error::Integrity);
            }
            if offset
                .checked_add(destination.len() as u64)
                .is_none_or(|end| end > home.bytes)
            {
                return Err(Error::Invalid("cached home range"));
            }
            let allocation = destination.host_ptr().map(|pointer| pointer as usize);
            let length = destination.len();
            let checked = |buffer: Buffer| {
                if buffer.len() != length
                    || buffer.host_ptr().map(|pointer| pointer as usize) != allocation
                {
                    return Err(Error::Integrity);
                }
                Ok(buffer)
            };
            match self
                .cache
                .lookup(&self.access, object, version, offset, destination)
                .await?
            {
                Lookup::Hit(buffer) => checked(buffer),
                Lookup::Miss(destination) => {
                    let destination = checked(destination)?;
                    let buffer = self
                        .home
                        .read(&self.access, object, version, offset, destination)
                        .await?;
                    let buffer = Arc::new(buffer);
                    self.cache
                        .fill(&self.access, object, version, offset, buffer.view())
                        .await?;
                    // The actual completed fill must release its input ownership. Never
                    // allocate an uncharged replacement to hide a retained service loan.
                    Arc::try_unwrap(buffer)
                        .map_err(|_| Error::Unavailable("cache fill retained charged source"))
                }
            }
        })
    }
    fn write<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        source: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            self.scope(scope)?;
            self.home.write(&self.access, object, source).await
        })
    }
    fn list<'a>(&'a self, scope: &'a Scope, prefix: &'a Object) -> Operation<'a, Vec<Metadata>> {
        Box::pin(async move {
            self.scope(scope)?;
            self.home.list(&self.access, prefix).await
        })
    }
    fn replace<'a>(
        &'a self,
        scope: &'a Scope,
        object: &'a Object,
        condition: Condition,
        source: BufferView,
    ) -> Operation<'a, Metadata> {
        Box::pin(async move {
            self.scope(scope)?;
            self.home
                .replace(&self.access, object, condition, source)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::{Allocator, Tier};
    use moruna_storage::{Credential, Role, local::FileStore};
    use moruna_testkit::FakeAllocator;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    struct Verifier(AtomicBool);
    impl Authorizer for Verifier {
        fn verify<'a>(
            &'a self,
            access: &'a Access,
            object: &'a Object,
            _: Action,
            _: Option<&'a Version>,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.0.load(Ordering::SeqCst)
                    && access.workspace == "cache-wiring-fixture"
                    && object.workspace == access.workspace
                    && access.credential.application() == Some(&b"TEST-ONLY-CACHE"[..])
                {
                    Ok(())
                } else {
                    Err(Error::Denied)
                }
            })
        }
    }
    // Test-only protocol probe, NOT an encryption/device implementation. The real native
    // codec/service owns those proofs. This checks engine ordering, hit use and refusals.
    struct Entry {
        object: Object,
        version: Version,
        offset: u64,
        bytes: Vec<u8>,
    }
    #[derive(Default)]
    struct Probe {
        entry: Mutex<Option<Entry>>,
        lookups: AtomicUsize,
        hits: AtomicUsize,
        fills: AtomicUsize,
        corrupt: AtomicBool,
        refuse_fill: AtomicBool,
    }
    impl Ranges for Probe {
        fn lookup<'a>(
            &'a self,
            _: &'a Access,
            object: &'a Object,
            version: &'a Version,
            offset: u64,
            mut destination: Buffer,
        ) -> Operation<'a, Lookup> {
            Box::pin(async move {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.corrupt.load(Ordering::SeqCst) {
                    return Err(Error::Integrity);
                }
                if let Some(entry) = self.entry.lock().unwrap().as_ref() {
                    if entry.object == *object
                        && entry.version == *version
                        && entry.offset == offset
                        && entry.bytes.len() == destination.len()
                    {
                        destination.as_mut().copy_from_slice(&entry.bytes);
                        self.hits.fetch_add(1, Ordering::SeqCst);
                        return Ok(Lookup::Hit(destination));
                    }
                }
                Ok(Lookup::Miss(destination))
            })
        }
        fn fill<'a>(
            &'a self,
            _: &'a Access,
            object: &'a Object,
            version: &'a Version,
            offset: u64,
            source: BufferView,
        ) -> Operation<'a, ()> {
            Box::pin(async move {
                if self.refuse_fill.load(Ordering::SeqCst) {
                    return Err(Error::Denied);
                }
                *self.entry.lock().unwrap() = Some(Entry {
                    object: object.clone(),
                    version: version.clone(),
                    offset,
                    bytes: source.as_host_slice().ok_or(Error::Integrity)?.to_vec(),
                });
                self.fills.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn real_home_versions_and_live_authority_gate_every_cache_hit_and_fill() {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let run = moruna_kernel::RunId(nonce).to_hex();
        let root = Scratch(std::env::temp_dir().join(format!("moruna-runtime-cache-{run}")));
        let verifier = Arc::new(Verifier(AtomicBool::new(true)));
        let home = Arc::new(
            Governed::new(
                Arc::new(
                    FileStore::open(
                        root.0.clone(),
                        "cache-wiring-fixture".into(),
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
            workspace: "cache-wiring-fixture".into(),
            run,
            credential: Credential::new(b"TEST-ONLY-CACHE".to_vec()).unwrap(),
        });
        let object = Object {
            workspace: access.workspace.clone(),
            bucket: "home".into(),
            key: "actual-input".into(),
        };
        let probe = Arc::new(Probe::default());
        let cached = Cached::wrap(home.clone(), probe.clone(), access.clone()).unwrap();
        let alloc = FakeAllocator::new();
        let buffer = |data: &[u8]| {
            let mut b = alloc.alloc(data.len(), Tier::Host).unwrap();
            b.as_mut().copy_from_slice(data);
            Arc::new(b)
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let first = home
                .write(&access, &object, buffer(b"first").view())
                .await
                .unwrap();
            for _ in 0..2 {
                let destination = alloc.alloc(5, Tier::Host).unwrap();
                let pointer = destination.host_ptr();
                let actual = cached
                    .read(&access, &object, &first.version, 0, destination)
                    .await
                    .unwrap();
                assert_eq!(actual.as_ref(), b"first");
                assert_eq!(actual.host_ptr(), pointer);
            }
            assert_eq!(probe.fills.load(Ordering::SeqCst), 1);
            assert_eq!(
                probe.hits.load(Ordering::SeqCst),
                1,
                "second read actually uses cached bytes"
            );
            let before = probe.lookups.load(Ordering::SeqCst);
            verifier.0.store(false, Ordering::SeqCst);
            assert!(matches!(
                cached
                    .read(
                        &access,
                        &object,
                        &first.version,
                        0,
                        alloc.alloc(5, Tier::Host).unwrap()
                    )
                    .await,
                Err(Error::Denied)
            ));
            assert_eq!(
                probe.lookups.load(Ordering::SeqCst),
                before,
                "revocation refuses before cache access"
            );
            verifier.0.store(true, Ordering::SeqCst);
            probe.corrupt.store(true, Ordering::SeqCst);
            assert!(matches!(
                cached
                    .read(
                        &access,
                        &object,
                        &first.version,
                        0,
                        alloc.alloc(5, Tier::Host).unwrap()
                    )
                    .await,
                Err(Error::Integrity)
            ));
            assert_eq!(
                probe.fills.load(Ordering::SeqCst),
                1,
                "corruption is not a miss/home fallback"
            );
            probe.corrupt.store(false, Ordering::SeqCst);
            let second = home
                .replace(
                    &access,
                    &object,
                    Condition::Match(first.version),
                    buffer(b"other").view(),
                )
                .await
                .unwrap();
            probe.refuse_fill.store(true, Ordering::SeqCst);
            assert!(
                matches!(
                    cached
                        .read(
                            &access,
                            &object,
                            &second.version,
                            0,
                            alloc.alloc(5, Tier::Host).unwrap()
                        )
                        .await,
                    Err(Error::Denied)
                ),
                "configured cache fill failure is loud"
            );
            probe.refuse_fill.store(false, Ordering::SeqCst);
            let actual = cached
                .read(
                    &access,
                    &object,
                    &second.version,
                    0,
                    alloc.alloc(5, Tier::Host).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(actual.as_ref(), b"other");
            assert_eq!(
                probe.fills.load(Ordering::SeqCst),
                2,
                "new immutable home version invalidates prior range identity"
            );
            let before = probe.lookups.load(Ordering::SeqCst);
            assert!(
                cached
                    .read(
                        &access,
                        &object,
                        &second.version,
                        4,
                        alloc.alloc(5, Tier::Host).unwrap()
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                probe.lookups.load(Ordering::SeqCst),
                before,
                "invalid actual home range never reaches cache"
            );
        });
    }
}
