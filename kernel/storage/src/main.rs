//! Native storage process assembly from explicit startup capabilities. No ambient socket,
//! credential, guessed listener, mount fallback, plaintext cache or fake verifier exists.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
#[cfg(not(target_os = "none"))]
compile_error!("native storage service binary requires the actual kernel target");
extern crate alloc;
use alloc::{boxed::Box, format, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use moruna_storage_format::{
    Role,
    auth::{Admission, SessionRole, Verifier},
    config::{BootConfig, MAX_CONFIG, Provider, validate_regions},
    wire::{Outcome, Refusal, Request},
};
use moruna_storage_service::{
    admission::{Binder, Factory, Pending},
    delegation::Delegator,
    device::Device,
    encrypted::Encrypted,
    keys::Keys,
    maintenance_config, maintenance_server,
    memory::Loan,
    network::Network,
    protocol::{admission, cache_admin},
    proxy::Proxy,
    session::Backend,
    volume::Volume,
};
use moruna_svc::{
    Channel, Console, Context, DEADLINE_INFINITE, Dispatcher, Handle, Memory, ObjectType,
    PrivateMemory, Rights, println, start,
};
use netstack_protocol::Target;
moruna_svc::main!(run);

struct Shared<T> {
    inner: Rc<RefCell<T>>,
    role: SessionRole,
    namespace: Option<Rc<RefCell<Volume>>>,
}
impl<T: Backend> Backend for Shared<T> {
    fn role(&self) -> SessionRole {
        self.role
    }
    fn namespace_volume(&self) -> Result<Rc<RefCell<Volume>>, Refusal> {
        self.namespace.clone().ok_or(Refusal::Unsupported)
    }
    fn execute(
        &mut self,
        request: &Request<'_>,
        account: &Handle,
        payload: Option<Loan>,
        verifier: &dyn Verifier,
    ) -> Result<Outcome, Refusal> {
        self.inner
            .try_borrow_mut()
            .map_err(|_| Refusal::Busy)?
            .execute(request, account, payload, verifier)
    }
}
struct Providers {
    remote: Option<Remote>,
    home: Option<Rc<RefCell<Volume>>>,
    metadata: Option<Rc<RefCell<Volume>>>,
    cache: Option<Rc<RefCell<Encrypted>>>,
    requester: Option<String>,
    out: Rc<Console>,
}
struct Remote {
    listener: Channel,
    target: Target,
    tls_name: String,
    credential: Loan,
    credential_bytes: u64,
    timeout_ns: u64,
}
impl Factory for Providers {
    fn open(
        &mut self,
        admission: &Admission,
        account: &Handle,
    ) -> Result<Box<dyn Backend>, Refusal> {
        match admission.role {
            SessionRole::BucketHome => {
                let remote = self.remote.as_ref().ok_or(Refusal::Unsupported)?;
                let connected = moruna_svc::connect(&remote.listener).map_err(|cause| {
                    println!(self.out, "storage network connect refused: {:?}", cause);
                    Refusal::Unavailable
                })?;
                let network =
                    Network::new(connected, remote.timeout_ns, account).map_err(|cause| {
                        println!(self.out, "storage network hello refused: {:?}", cause);
                        Refusal::Unavailable
                    })?;
                let credential = remote
                    .credential
                    .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
                    .map_err(|_| Refusal::Unavailable)?;
                let proxy = Proxy::new(
                    network,
                    remote.target.clone(),
                    remote.tls_name.clone(),
                    credential,
                    remote.credential_bytes,
                    account,
                )
                .map_err(|cause| {
                    println!(self.out, "storage bucket provider refused: {:?}", cause);
                    Refusal::Unavailable
                })?;
                Ok(Box::new(proxy))
            }
            SessionRole::SingleNodeHome => Ok(Box::new(Shared {
                inner: self.home.as_ref().ok_or(Refusal::Unsupported)?.clone(),
                role: SessionRole::SingleNodeHome,
                namespace: self.home.clone(),
            })),
            SessionRole::CheckpointMetadata => Ok(Box::new(Shared {
                inner: self.metadata.as_ref().ok_or(Refusal::Unsupported)?.clone(),
                role: SessionRole::CheckpointMetadata,
                namespace: self.metadata.clone(),
            })),
            SessionRole::CacheScratch => {
                if self.requester.as_ref() != Some(&admission.scope.workspace) {
                    return Err(Refusal::Denied);
                }
                Ok(Box::new(Shared {
                    inner: self.cache.as_ref().ok_or(Refusal::Unsupported)?.clone(),
                    role: SessionRole::CacheScratch,
                    namespace: None,
                }))
            }
        }
    }
}
struct CacheControl {
    cache: Rc<RefCell<Encrypted>>,
    account: Handle,
    out: Rc<Console>,
}
impl cache_admin::Server for CacheControl {
    fn recycle(&mut self, _: &mut Context<'_>) -> Result<(), cache_admin::Error> {
        let mut cache = self
            .cache
            .try_borrow_mut()
            .map_err(|_| cache_admin::Error::InUse)?;
        if !cache.quiescent() {
            return Err(cache_admin::Error::InUse);
        }
        cache.recycle(&self.account).map_err(|cause| {
            println!(self.out, "storage cache recycle refused: {:?}", cause);
            cache_admin::Error::Unavailable
        })
    }
}
fn take(name: &str) -> Result<Handle, String> {
    start::take(name).ok_or_else(|| format!("required startup grant missing: {name}"))
}
fn readonly(memory: &Memory) -> Result<(), String> {
    let (kind, rights) = memory
        .0
        .info()
        .map_err(|cause| format!("startup Memory info: {}", cause.name()))?;
    if kind != ObjectType::Memory
        || !rights.contains(Rights::READ | Rights::MAP)
        || rights.contains(Rights::WRITE)
    {
        return Err("startup metadata/secret grant must be readonly Memory".into());
    }
    Ok(())
}
fn configuration(account: &Handle) -> Result<BootConfig, String> {
    let memory = Memory(take("storage.config")?);
    readonly(&memory)?;
    let source = Loan::new(memory, 0, MAX_CONFIG as u64, false)
        .map_err(|cause| format!("config Memory: {}", cause.name()))?;
    let mut private = PrivateMemory::create_in(MAX_CONFIG as u64, account)
        .map_err(|cause| format!("config account admission: {}", cause.name()))?;
    private
        .with_bytes(|bytes| {
            source
                .read(0, &mut bytes[..MAX_CONFIG])
                .map_err(|_| String::from("config read refused"))?;
            BootConfig::decode_page(&bytes[..MAX_CONFIG])
                .map_err(|cause| format!("config refused: {cause}"))
        })
        .map_err(|cause| format!("config private memory: {}", cause.name()))?
}
fn block(service: &str, timeout_ns: u64) -> Result<Device, String> {
    // Explicit LISTENER contract; use the sole devices SDK helper, no endpoint guessing.
    let listener = Channel(take("storage.block-directory")?);
    let mut directory =
        devices_protocol::open_directory(listener, moruna_svc::deadline_in(timeout_ns))
            .map_err(|cause| format!("block directory: {cause:?}"))?;
    directory.core().set_timeout(timeout_ns);
    Device::open(&mut directory, service, timeout_ns)
        .map_err(|cause| format!("actual block lease: {cause:?}"))
}
fn providers(config: &BootConfig, account: &Handle, out: Rc<Console>) -> Result<Providers, String> {
    let mut providers = Providers {
        remote: None,
        home: None,
        metadata: None,
        cache: None,
        requester: None,
        out,
    };
    match &config.provider {
        Provider::Bucket {
            host,
            port,
            tls_name,
            credential_bytes,
        } => {
            let credential = Memory(take("storage.credential")?);
            readonly(&credential)?;
            let credential = Loan::new(credential, 0, *credential_bytes as u64, false)
                .map_err(|cause| format!("credential Memory: {}", cause.name()))?;
            providers.remote = Some(Remote {
                listener: Channel(take("storage.network-listener")?),
                target: Target::Name(host.clone(), *port),
                tls_name: tls_name.clone(),
                credential,
                credential_bytes: *credential_bytes as u64,
                timeout_ns: config.timeout_ns,
            });
        }
        Provider::Raw {
            service,
            alignment,
            role,
        } => {
            let device = block(service, config.timeout_ns)?;
            let volume = Rc::new(RefCell::new(
                Volume::mount(device, account, *alignment, *role)
                    .map_err(|cause| format!("raw mount refused: {cause:?}"))?,
            ));
            match role {
                Role::SingleNodeHome => providers.home = Some(volume),
                Role::CheckpointMetadata => providers.metadata = Some(volume),
                _ => return Err("unsupported raw role".into()),
            }
        }
        Provider::Local {
            service,
            alignment,
            metadata,
            cache,
            home,
            key_bytes,
            requester,
        } => {
            let device = block(service, config.timeout_ns)?;
            let mut regions = Vec::new();
            regions.push(*metadata);
            regions.push(*cache);
            if let Some(home) = home {
                regions.push(*home);
            }
            validate_regions(&regions, *alignment, device.capacity())
                .map_err(|cause| format!("actual local regions refused: {cause}"))?;
            let mut views = device
                .regions(&regions)
                .map_err(|cause| format!("local owner views refused: {cause:?}"))?
                .into_iter();
            let metadata = views.next().ok_or("metadata view missing")?;
            let cache = views.next().ok_or("cache view missing")?;
            providers.metadata = Some(Rc::new(RefCell::new(
                Volume::mount(metadata, account, *alignment, Role::CheckpointMetadata)
                    .map_err(|cause| format!("actual metadata role mount: {cause:?}"))?,
            )));
            let key = Memory(take("storage.key")?);
            readonly(&key)?;
            let keys = Keys::new(key, *key_bytes as u64, requester.clone(), account)
                .map_err(|cause| format!("actual workspace key refused: {cause:?}"))?;
            providers.cache = Some(Rc::new(RefCell::new(
                Encrypted::mount(cache, account, *alignment, keys)
                    .map_err(|cause| format!("actual encrypted cache role mount: {cause:?}"))?,
            )));
            providers.requester = Some(requester.clone());
            if home.is_some() {
                providers.home = Some(Rc::new(RefCell::new(
                    Volume::mount(
                        views.next().ok_or("home view missing")?,
                        account,
                        *alignment,
                        Role::SingleNodeHome,
                    )
                    .map_err(|cause| format!("actual raw home role mount: {cause:?}"))?,
                )));
            }
        }
    }
    // Both role views share the same physical Controller. Configure only after all
    // synchronous mounts finish, using the explicit admitted session budget bounded
    // by the actual original-owner ticket capacity (never a second connection).
    for volume in [&providers.home, &providers.metadata].into_iter().flatten() {
        volume
            .try_borrow()
            .map_err(|_| "mounted owner busy")?
            .configure_async(
                config
                    .session_limit
                    .min(moruna_storage_service::device::MAX_IN_FLIGHT),
            )
            .map_err(|cause| format!("original async admission: {cause:?}"))?;
    }
    Ok(providers)
}
fn serve(out: Rc<Console>) -> Result<(), String> {
    let account = take("account")?;
    let (kind, rights) = account
        .info()
        .map_err(|cause| format!("service Account info: {}", cause.name()))?;
    if kind != ObjectType::Account || !rights.contains(Rights::READ | Rights::WRITE) {
        return Err("actual carved service Account required".into());
    }
    let config = configuration(&account)?;
    let providers = providers(&config, &account, out.clone())?;
    let cache = providers.cache.clone();
    let maintenance_endpoint = start::take("storage.maintenance");
    let maintenance_policy = maintenance_config::load(&account)
        .map_err(|cause| format!("private maintenance policy refused: {cause:?}"))?;
    if maintenance_endpoint.is_some() != maintenance_policy.is_some() {
        return Err("maintenance endpoint and readonly policy must be explicitly paired".into());
    }
    let mut local_owners = Vec::new();
    if maintenance_endpoint.is_some() {
        if !rights.contains(Rights::DUPLICATE | Rights::TRANSFER) {
            return Err(
                "maintenance requires actual service Account duplicate/transfer rights".into(),
            );
        }
        for volume in [&providers.home, &providers.metadata].into_iter().flatten() {
            if !local_owners.iter().any(|known| Rc::ptr_eq(known, volume)) {
                local_owners.push(volume.clone());
            }
        }
        if local_owners.is_empty() {
            return Err(
                "maintenance requires actual local raw owners, never a bucket proxy".into(),
            );
        }
    }
    let admin = start::take("storage.cache-admin");
    if admin.is_some() && cache.is_none() {
        return Err("cache admin capability without actual cache backend".into());
    }
    let reserved = 1 + usize::from(admin.is_some()) + usize::from(maintenance_endpoint.is_some());
    if config.session_limit > moruna_svc::limits::WAIT_MAX_ITEMS - reserved {
        return Err("session limit exceeds actual dispatcher capacity".into());
    }
    let pending = Pending::new(config.session_limit)
        .map_err(|cause| format!("session admission: {cause:?}"))?;
    let maintenance_handler = if let Some(policy) = maintenance_policy {
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(local_owners.len())
            .map_err(|_| "maintenance owner budget")?;
        for volume in local_owners {
            owners.push(
                pending
                    .try_borrow_mut()
                    .map_err(|_| "private maintenance owner busy")?
                    .namespace_owner(volume)
                    .map_err(|cause| format!("actual maintenance driver owner: {cause:?}"))?,
            );
        }
        let account = Rc::new(moruna_sys::Account(
            account
                .duplicate(Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER)
                .map_err(|cause| format!("actual service maintenance Account: {}", cause.name()))?,
        ));
        Some(
            maintenance_server::handler(owners, account, policy, pending.clone())
                .map_err(|cause| format!("private maintenance assembly: {cause:?}"))?,
        )
    } else {
        None
    };
    let delegator = Delegator::new(Channel(take("storage.authority")?), config.timeout_ns)
        .map_err(|cause| format!("actual designated authority: {cause:?}"))?;
    let binder = Binder::new(
        delegator,
        Box::new(providers),
        pending.clone(),
        config.body_limit,
    )
    .map_err(|cause| format!("private binder: {cause:?}"))?;
    let mut dispatcher = Dispatcher::new();
    dispatcher
        .serve(
            Channel(take("storage.admission")?),
            Box::new(admission::Serve(binder)),
        )
        .map_err(|cause| format!("connected admission endpoint: {}", cause.name()))?;
    if let (Some(endpoint), Some(handler)) = (maintenance_endpoint, maintenance_handler) {
        dispatcher
            .serve(Channel(endpoint), handler)
            .map_err(|cause| format!("private maintenance endpoint: {}", cause.name()))?;
    }
    if let (Some(admin), Some(cache)) = (admin, cache) {
        let account = account
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER)
            .map_err(|cause| format!("cache Account loan: {}", cause.name()))?;
        dispatcher
            .serve(
                Channel(admin),
                Box::new(cache_admin::Serve(CacheControl {
                    cache,
                    account,
                    out: out.clone(),
                })),
            )
            .map_err(|cause| format!("private cache admin: {}", cause.name()))?;
    }
    while dispatcher.channels() > 0 {
        pending
            .try_borrow_mut()
            .map_err(|_| "pending session owner busy")?
            .register(&mut dispatcher)
            .map_err(|cause| format!("session registration: {}", cause.name()))?;
        dispatcher
            .turn(DEADLINE_INFINITE)
            .map_err(|cause| format!("storage dispatch: {}", cause.name()))?;
    }
    Ok(())
}
fn run() -> i64 {
    let Some(out) = start::take("console").map(Console).map(Rc::new) else {
        return 90;
    };
    match serve(out.clone()) {
        Ok(()) => 0,
        Err(cause) => {
            println!(out, "storage service refused: {}", cause);
            1
        }
    }
}
