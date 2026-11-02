//! Real guest bootstrap: named startup memory plus actual admitted storage/account channels.
//! The descriptor contains configuration only. Native session credentials come from the real
//! connected service grants; no opaque token, local fallback, environment or URL grants access.
use crate::{
    Components,
    cost::{RunLearning, Session},
    history::{Historical, Output, ReadPort},
    portable::{EngineCheckpoints, Publisher},
};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use moruna_kernel::Allocator;
use moruna_kernel::{MorunaError, RunId};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use moruna_storage::Scope;
use moruna_storage::{Access, Governed, Object};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// The only supported descriptor major, carried by readonly `engine.bindings` startup memory.
pub const VERSION: u32 = 1;
/// Admission for required logical task output, not a physical bucket reservation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retained {
    /// Actual logical task.
    pub task: String,
    /// Actual declared logical output port.
    pub port: String,
    /// Actual immutable attempt identity.
    pub attempt: String,
    /// Governed destination under the issuer's real policy.
    pub target: Object,
    /// Existing policy decision identifier; it does not confer authority itself.
    pub policy: String,
    /// Required capture only; false is refused rather than silently dropped.
    pub required: bool,
    /// Explicit retained encoded-byte ceiling.
    pub byte_budget: u64,
}
/// Explicit required-capture working set.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// Requested logical outputs.
    pub outputs: Vec<Retained>,
    /// Maximum retained input bytes.
    pub queue_bytes: u64,
    /// Maximum queued batches.
    pub queue_batches: usize,
    /// Charged complete IPC stream capacity.
    pub encoding_bytes: usize,
    /// Initial/final drain deadline.
    pub drain_ms: u64,
}
/// Two genuinely distinct checkpoint storage roles; no bucket-as-local relabeling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    /// Connected native service for actual local durable checkpoint metadata.
    pub local_service_slot: String,
    /// Actual local durable target.
    pub local_target: Object,
    /// Actual bucket acknowledgement target on the home service.
    pub bucket_target: Object,
    /// Maximum bounded dependency transfer chunk.
    pub chunk_bytes: usize,
    /// Reserved native codec working set before quiescence.
    pub encoding_bytes: usize,
    /// Whole required publication deadline.
    pub timeout_ms: u64,
}
/// One governed observation store shared by registration and live runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    /// Existing storage namespace; no directory/home inference.
    pub target: Object,
    /// Existing registered model name.
    pub model: String,
    /// Maximum records admitted to a scan.
    pub records: usize,
    /// Maximum encoded observation or opaque fitted state.
    pub object_bytes: usize,
    /// Total history bytes admitted to a fit.
    pub scan_bytes: u64,
    /// Maximum queued cost IO requests.
    pub queue: usize,
    /// Existing raw history retention before summarisation.
    pub retention: usize,
}
/// Configuration transported by a readonly startup grant, never evidence of permission.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bindings {
    #[serde(skip)]
    admission: Option<moruna_storage_format::auth::Admission>,
    /// Exact descriptor major.
    pub version: u32,
    /// Actual requesting workspace admitted by the service.
    pub workspace: String,
    /// Actual assigned run, canonical 32 hex characters.
    pub run: String,
    /// This Linux program's own carved account, never its supervisor's account.
    pub account_slot: String,
    /// Actual connected governed bucket-home service.
    pub storage_slot: String,
    /// Explicit connected encrypted CacheScratch service. Absence selects no cache;
    /// a configured grant is always consumed and used, never silently discarded.
    pub cache_slot: Option<String>,
    /// Explicit charged control/SDK arena, counted in the process baseline/hard account.
    pub control_bytes: u64,
    /// All encoded metadata remains explicitly bounded.
    pub metadata_bytes: usize,
    /// Native service operation deadline.
    pub operation_ms: u64,
    /// Explicit reader capability selection; authorization is still live per read.
    pub reader: bool,
    /// Required retained logical rows, if requested.
    pub capture: Option<Capture>,
    /// Actual local+bucket checkpoint capabilities, if requested.
    pub checkpoint: Option<Checkpoint>,
    /// Same governed observations/model, if configured.
    pub cost: Option<Cost>,
}
fn refused(reason: impl Into<String>) -> MorunaError {
    MorunaError::Config {
        name: "engine.bindings",
        msg: reason.into(),
    }
}
fn slot(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
impl Bindings {
    /// Validate configuration without minting credentials, connecting, allocating or touching IO.
    pub fn parse(bytes: &[u8]) -> moruna_kernel::Result<Self> {
        let envelope = moruna_storage_format::bindings::EngineBindings::decode_page(bytes)
            .map_err(|_| refused("malformed canonical engine bindings envelope"))?;
        let mut value = Self::parse_payload(&envelope.payload)?;
        if value.workspace != envelope.admission.scope.workspace
            || value.run != envelope.admission.scope.run
        {
            return Err(refused(
                "JSON configuration differs from canonical admitted identity",
            ));
        }
        value.admission = Some(envelope.admission);
        Ok(value)
    }
    fn parse_payload(bytes: &[u8]) -> moruna_kernel::Result<Self> {
        if bytes.len() > 65536 {
            return Err(refused("descriptor exceeds 64 KiB"));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| refused("malformed descriptor or unknown fields"))?;
        if value.version != VERSION
            || RunId::from_hex(&value.run).is_none()
            || value.workspace.is_empty()
            || !slot(&value.account_slot)
            || !slot(&value.storage_slot)
            || value.control_bytes == 0
            || !value.control_bytes.is_multiple_of(4096)
            || value.metadata_bytes == 0
            || value.operation_ms == 0
        {
            return Err(refused(
                "descriptor version/identity/slot/admission invalid",
            ));
        }
        moruna_storage::Scope::validate_parts(&value.workspace, &value.run)
            .map_err(|_| refused("invalid native requester scope"))?;
        for target in value
            .capture
            .iter()
            .flat_map(|capture| capture.outputs.iter().map(|output| &output.target))
            .chain(
                value
                    .checkpoint
                    .iter()
                    .flat_map(|checkpoint| [&checkpoint.local_target, &checkpoint.bucket_target]),
            )
            .chain(value.cost.iter().map(|cost| &cost.target))
        {
            target
                .validate()
                .map_err(|_| refused("malformed governed target identity"))?;
        }
        if let Some(cache_slot) = &value.cache_slot
            && (!slot(cache_slot)
                || cache_slot == &value.storage_slot
                || value
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| &checkpoint.local_service_slot == cache_slot))
        {
            return Err(refused(
                "cache requires a distinct actual CacheScratch service slot",
            ));
        }
        if let Some(checkpoint) = &value.checkpoint
            && (!slot(&checkpoint.local_service_slot)
                || checkpoint.local_service_slot == value.storage_slot
                || checkpoint.chunk_bytes == 0
                || checkpoint.encoding_bytes == 0
                || checkpoint.timeout_ms == 0)
        {
            return Err(refused(
                "checkpoint needs distinct actual local/home services and positive budgets",
            ));
        }
        if let Some(capture) = &value.capture
            && (capture.outputs.is_empty()
                || capture.outputs.iter().any(|output| {
                    !output.required
                        || output.task.is_empty()
                        || output.port.is_empty()
                        || output.attempt.is_empty()
                        || output.policy.is_empty()
                        || output.byte_budget == 0
                })
                || capture.queue_bytes == 0
                || capture.queue_batches == 0
                || capture.encoding_bytes == 0
                || capture.drain_ms == 0)
        {
            return Err(refused(
                "required retained output identity/admission invalid",
            ));
        }
        if let Some(cost) = &value.cost
            && (cost.model.is_empty()
                || cost.records == 0
                || cost.object_bytes == 0
                || cost.scan_bytes == 0
                || cost.queue == 0
                || cost.retention == 0)
        {
            return Err(refused("governed cost admission invalid"));
        }
        Ok(value)
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct Arenas {
    scope: Scope,
    alloc: Arc<dyn Allocator>,
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl moruna_storage::volume::Arenas for Arenas {
    fn allocator(&self, scope: &Scope) -> moruna_storage::Result<Arc<dyn Allocator>> {
        if scope != &self.scope {
            return Err(moruna_storage::Error::Denied);
        }
        Ok(self.alloc.clone())
    }
}
/// Constructed only from actual native startup grants. This owns the admitted scoped clients,
/// their real account and its charged control arena through the canonical storage authority.
pub struct Guest {
    bindings: Bindings,
    home: Arc<Governed>,
    access: Arc<Access>,
    reader: Option<Arc<ReadPort>>,
    checkpoint: Option<Arc<Publisher>>,
    restore: Option<Arc<EngineCheckpoints>>,
    cost: Option<Arc<Session>>,
    history: std::sync::Mutex<Option<Arc<Historical>>>,
    claimed: std::sync::atomic::AtomicBool,
}
impl Guest {
    /// Use the canonical startup authority when executing on the Moruna kernel.
    /// Ordinary host processes have no native binding; malformed/missing native
    /// grants remain errors rather than silently selecting host defaults.
    pub fn for_current_process() -> moruna_kernel::Result<Option<Arc<Self>>> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if moruna_micro::on_kernel() {
            return Self::from_start().map(Some);
        }
        Ok(None)
    }

    /// Consume `engine.bindings` and real named capabilities on a Linux x86_64 Moruna process.
    /// Off-kernel/unsupported targets refuse; there is no host filesystem/socket default.
    pub fn from_start() -> moruna_kernel::Result<Arc<Self>> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            native::from_start()
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            Err(refused(
                "native guest bootstrap requires a real Linux x86_64 Moruna start page",
            ))
        }
    }
    /// The actual immutable assigned run.
    pub fn run_id(&self) -> RunId {
        RunId::from_hex(&self.bindings.run).expect("validated run identity")
    }
    /// Actual most recently admitted capture state, absent before a checked job is bound.
    pub fn history(&self) -> Option<Arc<Historical>> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// Read an actual governed input range into the caller's charged destination. A native
    /// configured cache is part of this SAME authority, not a source re-query or shadow store.
    pub async fn read_object(
        &self,
        object: &Object,
        version: &moruna_storage::Version,
        offset: u64,
        destination: moruna_kernel::Buffer,
    ) -> moruna_storage::Result<moruna_kernel::Buffer> {
        self.home
            .read(&self.access, object, version, offset, destination)
            .await
    }
    /// Existing known-future range planner over the same governed read path. Every supplied
    /// destination is already charged; the canonical port enforces concurrency/byte ceilings.
    pub async fn prefetch(
        &self,
        ranges: Vec<moruna_storage::prefetch::PlannedRange>,
        parallel: usize,
        byte_budget: u64,
    ) -> moruna_storage::Result<Vec<moruna_storage::prefetch::ReadyRange>> {
        moruna_storage::prefetch::fetch(&self.home, &self.access, ranges, parallel, byte_budget)
            .await
    }
    /// Same native reader, if explicitly selected.
    pub fn reader(&self) -> Option<Arc<ReadPort>> {
        self.reader.clone()
    }
    /// Same native checkpoint writer, if explicitly selected.
    pub fn checkpoints(&self) -> Option<Arc<Publisher>> {
        self.checkpoint.clone()
    }
    /// Actual materializer and live authority; no serialized Arc or filesystem token.
    pub fn restore(&self) -> Option<(Arc<EngineCheckpoints>, Arc<Access>)> {
        self.restore
            .clone()
            .map(|restore| (restore, self.access.clone()))
    }
    /// Same native store/model session for registration and actual live engine execution.
    pub fn cost_session(&self) -> Option<Arc<Session>> {
        self.cost.clone()
    }
    /// Bind to the actual checked job/plan. Required logical boundaries are resolved by the
    /// same native helper; permissions remain enforced by the real service on every operation.
    pub fn components(
        &self,
        job: &moruna_job::Job,
        parts: &crate::job::Parts,
    ) -> moruna_kernel::Result<Components> {
        if job.id.as_ref().is_some_and(|run| run != &self.bindings.run) {
            return Err(refused("checked job differs from assigned run"));
        }
        if self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err(refused(
                "guest bindings already consumed by an attempt; require fresh admitted startup grants",
            ));
        }
        let history = self
            .bindings
            .capture
            .as_ref()
            .map(|capture| {
                let outputs = capture
                    .outputs
                    .iter()
                    .map(|output| Output {
                        request: moruna_storage::artifact::Request {
                            identity: moruna_storage::artifact::Identity {
                                workspace: self.bindings.workspace.clone(),
                                run: self.bindings.run.clone(),
                                attempt: output.attempt.clone(),
                                task: output.task.clone(),
                                port: output.port.clone(),
                            },
                            required: output.required,
                            byte_budget: output.byte_budget,
                            policy: output.policy.clone(),
                        },
                        target: output.target.clone(),
                        access: self.access.clone(),
                    })
                    .collect::<Vec<_>>();
                let bindings = Historical::bindings_for_job(job, parts, &outputs)?;
                Historical::new(
                    self.home.clone(),
                    outputs,
                    bindings,
                    capture.queue_bytes,
                    capture.queue_batches,
                    capture.encoding_bytes,
                    self.bindings.metadata_bytes,
                    Duration::from_millis(self.bindings.operation_ms),
                    Duration::from_millis(capture.drain_ms),
                )
            })
            .transpose()?;
        let learning = self
            .cost
            .as_ref()
            .map(|session| RunLearning::for_job(session.online()?, job, parts))
            .transpose()?;
        *self.history.lock().unwrap_or_else(|e| e.into_inner()) = history.clone();
        Ok(Components {
            run_id: Some(self.run_id()),
            history,
            learning,
            checkpoints: self.checkpoint.clone(),
            ..Components::default()
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod native {
    use super::*;
    use crate::cost::CostSettings;
    use moruna_storage::native::{Account, Client};
    use moruna_storage_format::auth::SessionRole;
    use moruna_sys::{Memory, ObjectType, Rights, map};
    use std::sync::OnceLock;
    const PAGE: u64 = 4096;

    pub(super) fn from_start() -> moruna_kernel::Result<Arc<Guest>> {
        static START: OnceLock<Result<Arc<Guest>, String>> = OnceLock::new();
        if moruna_micro::start::kernel().is_none() {
            return Err(refused(
                "no actual Moruna start page; native bootstrap cannot use host fallback",
            ));
        }
        START
            .get_or_init(|| load().map_err(|error| error.to_string()))
            .clone()
            .map_err(refused)
    }
    fn descriptor() -> moruna_kernel::Result<Bindings> {
        let handle = moruna_micro::start::take("engine.bindings")
            .ok_or_else(|| refused("missing actual readonly engine.bindings start grant"))?;
        let (kind, rights) = handle
            .info()
            .map_err(|_| refused("engine.bindings handle inspection refused"))?;
        if kind != ObjectType::Memory
            || !rights.contains(Rights::READ | Rights::MAP)
            || rights.contains(Rights::WRITE)
        {
            return Err(refused(
                "engine.bindings must be an actual readonly Memory grant",
            ));
        }
        let memory = Memory(handle);
        let first = memory
            .map(0, PAGE, map::READ)
            .map_err(|_| refused("engine.bindings prefix mapping refused"))?;
        let mut page = [0; PAGE as usize];
        first
            .read(0, &mut page)
            .map_err(|_| refused("engine.bindings prefix read refused"))?;
        let length = moruna_storage_format::wire::header_bytes(&page)
            .map_err(|_| refused("engine.bindings canonical framing refused"))?;
        let mut bytes = Vec::with_capacity(length.div_ceil(PAGE as usize) * PAGE as usize);
        bytes.extend_from_slice(&page);
        for offset in (PAGE as usize..length).step_by(PAGE as usize) {
            let mapping = memory
                .map(offset as u64, PAGE, map::READ)
                .map_err(|_| refused("engine.bindings memory range refused"))?;
            mapping
                .read(0, &mut page)
                .map_err(|_| refused("engine.bindings read refused"))?;
            bytes.extend_from_slice(&page);
        }
        Bindings::parse(&bytes)
    }

    fn load() -> moruna_kernel::Result<Arc<Guest>> {
        let bindings = descriptor()?;
        let account = Account::from_slot(&bindings.account_slot)
            .map_err(|error| refused(error.to_string()))?;
        let usage = moruna_sys::Usage::own()
            .map_err(|_| refused("actual child account readback refused"))?;
        if bindings.control_bytes > usage.room() {
            return Err(refused(
                "control/SDK reservation does not fit actual child account room",
            ));
        }
        // This explicit role requests ordinary pages, no devices, pinning, huge pages or
        // RDMA. It is a real arena under the real child account and part of the baseline.
        let alloc: Arc<dyn Allocator> = moruna_arena::Arena::new(moruna_arena::ArenaConfig {
            host_bytes: bindings.control_bytes,
            ..moruna_arena::ArenaConfig::default()
        })?;
        let scope = Scope {
            workspace: bindings.workspace.clone(),
            run: bindings.run.clone(),
        };
        let arenas: Arc<dyn moruna_storage::volume::Arenas> = Arc::new(Arenas {
            scope: scope.clone(),
            alloc: alloc.clone(),
        });
        let timeout = bindings
            .operation_ms
            .checked_mul(1_000_000)
            .ok_or_else(|| refused("native service deadline overflow"))?;
        let home_client = Client::from_slot(
            &bindings.storage_slot,
            scope.clone(),
            account.clone(),
            arenas.clone(),
            timeout,
        )
        .map_err(|error| refused(error.to_string()))?;
        if bindings.admission.as_ref().map(|admission| admission.role) != Some(home_client.role()) {
            return Err(refused(
                "actual home session role differs from canonical admission",
            ));
        }
        let local_client = bindings
            .checkpoint
            .as_ref()
            .map(|checkpoint| {
                Client::from_slot(
                    &checkpoint.local_service_slot,
                    scope.clone(),
                    account.clone(),
                    arenas.clone(),
                    timeout,
                )
                .map_err(|error| refused(error.to_string()))
            })
            .transpose()?;
        if !matches!(
            home_client.role(),
            SessionRole::BucketHome | SessionRole::SingleNodeHome
        ) {
            return Err(refused("actual storage session is not a configured home"));
        }
        if bindings.checkpoint.is_some() && home_client.role() != SessionRole::BucketHome {
            return Err(refused(
                "portable checkpoint requires actual BucketHome attestation",
            ));
        }
        if local_client
            .as_ref()
            .is_some_and(|client| client.role() != SessionRole::CheckpointMetadata)
        {
            return Err(refused(
                "local checkpoint capability lacks actual CheckpointMetadata attestation",
            ));
        }
        let cache_client = bindings
            .cache_slot
            .as_ref()
            .map(|slot| {
                let client = Client::from_slot(
                    slot,
                    scope.clone(),
                    account.clone(),
                    arenas.clone(),
                    timeout,
                )
                .map_err(|error| refused(error.to_string()))?;
                if client.role() != SessionRole::CacheScratch {
                    return Err(refused(
                        "cache capability lacks actual CacheScratch attestation",
                    ));
                }
                Ok(client)
            })
            .transpose()?;
        let mut clients = vec![home_client.clone()];
        if let Some(local) = &local_client {
            clients.push(local.clone());
        }
        if let Some(cache) = &cache_client {
            clients.push(cache.clone());
        }
        let credential = moruna_storage::Credential::native_clients(&clients)
            .map_err(|error| refused(error.to_string()))?;
        let access = Arc::new(Access {
            workspace: bindings.workspace.clone(),
            run: bindings.run.clone(),
            credential,
        });
        let home = Arc::new(
            home_client
                .governed()
                .map_err(|error| refused(error.to_string()))?,
        );
        let home = match cache_client {
            Some(cache) => crate::object_cache::native(home, cache, access.clone())
                .map_err(|error| refused(error.to_string()))?,
            None => home,
        };
        let reader = if bindings.reader {
            Some(Arc::new(
                ReadPort::new(
                    home.clone(),
                    access.clone(),
                    alloc.clone(),
                    bindings.metadata_bytes,
                )
                .map_err(|error| refused(error.to_string()))?,
            ))
        } else {
            None
        };
        let (checkpoint, restore) = match (&bindings.checkpoint, local_client) {
            (Some(checkpoint), Some(local)) => {
                let local = Arc::new(
                    local
                        .governed()
                        .map_err(|error| refused(error.to_string()))?,
                );
                let authority = Arc::new(
                    moruna_storage::checkpoint::Checkpoints::new(
                        local,
                        home.clone(),
                        alloc.clone(),
                        bindings.metadata_bytes,
                    )
                    .map_err(|error| refused(error.to_string()))?,
                );
                let publisher = Arc::new(
                    Publisher::new(
                        authority.clone(),
                        access.clone(),
                        checkpoint.local_target.clone(),
                        checkpoint.bucket_target.clone(),
                        checkpoint.chunk_bytes,
                        bindings.metadata_bytes,
                        checkpoint.encoding_bytes,
                        Duration::from_millis(checkpoint.timeout_ms),
                    )
                    .map_err(|error| refused(error.to_string()))?,
                );
                let restore = Arc::new(
                    EngineCheckpoints::new(
                        authority,
                        alloc.clone(),
                        checkpoint.chunk_bytes,
                        bindings.metadata_bytes,
                    )
                    .map_err(|error| refused(error.to_string()))?,
                );
                (Some(publisher), Some(restore))
            }
            (None, None) => (None, None),
            _ => return Err(refused("local checkpoint service binding incomplete")),
        };
        let cost = bindings
            .cost
            .as_ref()
            .map(|cost| {
                let store = crate::cost_store::Store::open(
                    home.clone(),
                    access.clone(),
                    alloc.clone(),
                    cost.target.clone(),
                    crate::cost_store::Limits {
                        records: cost.records,
                        object_bytes: cost.object_bytes,
                        scan_bytes: cost.scan_bytes,
                        queue: cost.queue,
                        timeout: Duration::from_millis(bindings.operation_ms),
                        retention: moruna_cost::Retention {
                            per_key: cost.retention,
                        },
                    },
                )
                .map_err(|error| refused(error.to_string()))?;
                let mut settings = CostSettings::embedder(Arc::new(store));
                settings.model = cost.model.clone();
                Ok::<_, MorunaError>(Arc::new(Session::new(settings)))
            })
            .transpose()?;
        Ok(Arc::new(Guest {
            bindings,
            home,
            access,
            reader,
            checkpoint,
            restore,
            cost,
            history: std::sync::Mutex::new(None),
            claimed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor() -> serde_json::Value {
        serde_json::json!({"version":VERSION,"workspace":"workspace","run":"07070707070707070707070707070707",
            "account_slot":"account","storage_slot":"storage","control_bytes":8<<20,"metadata_bytes":65536,
            "operation_ms":2000,"reader":true,"capture":null,"checkpoint":null,"cost":null})
    }
    #[test]
    fn configured_cache_must_name_a_distinct_actual_service() {
        let mut value = descriptor();
        value["cache_slot"] = serde_json::json!("storage");
        assert!(Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).is_err());
        value["cache_slot"] = serde_json::json!("cache");
        assert!(
            Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).is_ok(),
            "actual CacheScratch attestation is still checked when consuming the real grant"
        );
    }
    #[test]
    fn canonical_envelope_identity_must_match_json_configuration() {
        use moruna_storage_format::{
            Scope,
            auth::{Admission, SessionRole},
            bindings::EngineBindings,
        };
        let value = descriptor();
        let mut envelope = EngineBindings {
            admission: Admission {
                scope: Scope {
                    workspace: value["workspace"].as_str().unwrap().into(),
                    run: value["run"].as_str().unwrap().into(),
                },
                role: SessionRole::BucketHome,
            },
            payload: serde_json::to_vec(&value).unwrap(),
        };
        let mut page = vec![0; moruna_storage_format::wire::MAX_HEADER];
        let used = envelope.encode_into(&mut page).unwrap();
        let valid = Bindings::parse(&page[..used]).unwrap();
        assert_eq!(valid.run, envelope.admission.scope.run);
        envelope.admission.scope.run = "ffffffffffffffffffffffffffffffff".into();
        page.fill(0);
        let used = envelope.encode_into(&mut page).unwrap();
        assert!(
            Bindings::parse(&page[..used]).is_err(),
            "JSON identity cannot override actual admitted identity"
        );
        assert!(
            Bindings::parse(&envelope.payload).is_err(),
            "bare legacy JSON is no bootstrap authority"
        );
    }
    #[test]
    fn descriptor_has_one_explicit_identity_and_no_credentials_or_implicit_defaults() {
        let value = descriptor();
        let parsed = Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.run, "07070707070707070707070707070707");
        for change in ["unknown", "credential"] {
            let mut altered = value.clone();
            altered[change] = serde_json::json!("not-a-grant");
            assert!(Bindings::parse_payload(&serde_json::to_vec(&altered).unwrap()).is_err());
        }
        for field in [
            "account_slot",
            "storage_slot",
            "control_bytes",
            "operation_ms",
        ] {
            let mut altered = value.clone();
            altered.as_object_mut().unwrap().remove(field);
            assert!(
                Bindings::parse_payload(&serde_json::to_vec(&altered).unwrap()).is_err(),
                "{field} has no hidden default"
            );
        }
        let mut unsupported = value.clone();
        unsupported["version"] = serde_json::json!(VERSION + 1);
        assert!(Bindings::parse_payload(&serde_json::to_vec(&unsupported).unwrap()).is_err());
        let mut invalid = value;
        invalid["run"] = serde_json::json!("account-name-is-not-a-run-id");
        assert!(Bindings::parse_payload(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    #[test]
    fn local_checkpoint_cannot_relabel_the_home_slot_and_capture_cannot_be_optional() {
        let mut value = descriptor();
        let object =
            serde_json::json!({"workspace":"workspace","bucket":"home","key":"checkpoint"});
        value["checkpoint"] = serde_json::json!({"local_service_slot":"storage","local_target":object,
            "bucket_target":object,"chunk_bytes":65536,"encoding_bytes":1<<20,"timeout_ms":2000});
        assert!(Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).is_err());
        value["checkpoint"]["local_service_slot"] = serde_json::json!("checkpoint.local");
        assert!(
            Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).is_ok(),
            "actual attestation is still required at native bind"
        );
        value["capture"] = serde_json::json!({"outputs":[{"task":"kernel-1","port":"out","attempt":"attempt-1",
            "target":object,"policy":"actual-policy","required":false,"byte_budget":4096}],
            "queue_bytes":4096,"queue_batches":1,"encoding_bytes":4096,"drain_ms":2000});
        assert!(Bindings::parse_payload(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
