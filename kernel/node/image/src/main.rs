//! Validate the actual immutable node/initial engine configuration before packaging.
use moruna_image::{DescSource, Description};
use std::{fs, path::Path, process::ExitCode};
fn page(root: &Path, name: &str) -> Result<Vec<u8>, String> {
    let bytes = fs::read(root.join(name)).map_err(|e| format!("{name}: {e}"))?;
    if bytes.is_empty() || bytes.len() >= 4096 || bytes.contains(&0) {
        return Err(format!(
            "{name}: expected nonempty bounded text without embedded padding"
        ));
    }
    Ok(bytes)
}
fn binary_asset(root: &Path, name: &str) -> Result<Vec<u8>, String> {
    let mut bytes = fs::read(root.join(name)).map_err(|e| format!("{name}: {e}"))?;
    if bytes.is_empty() || bytes.len() > moruna_storage_format::wire::MAX_HEADER {
        return Err(format!("invalid bounded asset {name}"));
    }
    bytes.resize(bytes.len().div_ceil(4096) * 4096, 0);
    Ok(bytes)
}
fn admission_asset(
    root: &Path,
    name: &str,
) -> Result<moruna_storage_format::auth::Admission, String> {
    let control = binary_asset(root, name)?;
    let length = moruna_storage_format::wire::header_bytes(&control)
        .map_err(|_| format!("invalid {name} header"))?;
    if length > control.len() || control[length..].iter().any(|b| *b != 0) {
        return Err(format!("invalid {name} padding"));
    }
    moruna_storage_format::auth::Admission::decode(&control[..length])
        .map_err(|_| format!("invalid {name}"))
}
fn storage_assets(root: &Path, node: &moruna_node::Config) -> Result<(), String> {
    use moruna_storage_format::{
        Role,
        auth::SessionRole,
        bindings::EngineBindings,
        config::{BootConfig, Provider},
        grants::GrantSet,
    };
    let bindings = EngineBindings::decode_page(&binary_asset(root, "engine.bindings")?)
        .map_err(|_| "invalid canonical engine.bindings")?;
    let admission = admission_asset(root, "storage.admission")?;
    if admission != bindings.admission {
        return Err("home admission must equal canonical engine binding identity and role".into());
    }
    GrantSet::decode_page(&binary_asset(root, "storage.grants")?)
        .map_err(|_| "invalid explicit trusted storage grants")?;
    let config = BootConfig::decode_page(&binary_asset(root, "storage.conf")?)
        .map_err(|_| "invalid canonical storage boot config")?;
    let service = node
        .services
        .iter()
        .find(|s| s.image == "image.storage")
        .ok_or("missing actual storage service")?;
    let linked = |slot: &str| {
        node.links.iter().any(|l| {
            l.left == service.name && l.left_slot == slot
                || l.right == service.name && l.right_slot == slot
        })
    };
    if !linked("storage.admission") || !linked("storage.authority") {
        return Err("storage requires actual private admission and authority channel links".into());
    }
    let unified = matches!(&config.provider, Provider::Local { home: Some(_), .. });
    match config.provider.clone() {
        Provider::Bucket {
            credential_bytes, ..
        } => {
            let credential =
                fs::read(root.join("storage.credential")).map_err(|e| e.to_string())?;
            if admission.role != SessionRole::BucketHome
                || credential.len() != credential_bytes as usize
                || !linked("storage.network-listener")
                || linked("storage.block-directory")
            {
                return Err("bucket provider inputs/role/transport disagree".into());
            }
        }
        Provider::Raw {
            role: Role::SingleNodeHome,
            ..
        } => {
            if admission.role != SessionRole::SingleNodeHome
                || !linked("storage.block-directory")
                || linked("storage.network-listener")
            {
                return Err("single-node home inputs/role/transport disagree".into());
            }
        }
        Provider::Local { home: Some(_), .. } => {
            if admission.role != SessionRole::SingleNodeHome
                || !linked("storage.block-directory")
                || linked("storage.network-listener")
            {
                return Err(
                    "unified single-node home requires actual local provider and raw role".into(),
                );
            }
            if node.services.iter().any(|s| s.name == "local") {
                return Err("unified home must not create a second local owner".into());
            }
        }
        _ => return Err("initial home cannot be a metadata/cache provider".into()),
    }
    let local = if unified {
        Some(service)
    } else {
        node.services.iter().find(|s| s.name == "local")
    };
    if let Some(local) = local {
        let config = if unified {
            config
        } else {
            BootConfig::decode_page(&binary_asset(root, "local.conf")?)
                .map_err(|_| "invalid canonical local config")?
        };
        let Provider::Local {
            requester,
            key_bytes,
            home,
            ..
        } = config.provider
        else {
            return Err(
                "local component must own real metadata/cache regions under one physical lease"
                    .into(),
            );
        };
        if home.is_some() != unified {
            return Err(
                "local raw-home region must be selected explicitly by home assembly".into(),
            );
        }
        let metadata = admission_asset(root, "checkpoint.admission")?;
        let cache = admission_asset(root, "cache.admission")?;
        if metadata.scope != bindings.admission.scope
            || cache.scope != bindings.admission.scope
            || metadata.role != SessionRole::CheckpointMetadata
            || cache.role != SessionRole::CacheScratch
            || requester != bindings.admission.scope.workspace
        {
            return Err("local binding scope/role/key requester disagree".into());
        }
        for slot in [
            "storage.admission",
            "storage.authority",
            "storage.block-directory",
        ] {
            if !node.links.iter().any(|l| {
                l.left == local.name && l.left_slot == slot
                    || l.right == local.name && l.right_slot == slot
            }) {
                return Err(format!("local missing actual {slot} link"));
            }
        }
        if local.image != "image.storage" {
            return Err("local must use actual storage service executable".into());
        }
        let key = binary_asset(root, "cache.key")?;
        let length = key_bytes as usize;
        if length > key.len() || key[length..].iter().any(|b| *b != 0) {
            return Err("invalid cache secret page length/padding".into());
        }
        moruna_storage_format::cache::WorkspaceKey::decode_secret(&key[..length], &requester)
            .map_err(|_| "invalid actual cache key/requester")?;
    }
    maintenance_assets(root, node)?;
    Ok(())
}

/// Optional private maintenance has exactly one designated resident recipient.
/// Neither a runtime admission nor a tenant listener receives this capability.
fn maintenance_assets(root: &Path, node: &moruna_node::Config) -> Result<(), String> {
    let workers: Vec<_> = node
        .services
        .iter()
        .filter(|s| s.image == "image.maintenance")
        .collect();
    let links: Vec<_> = node
        .links
        .iter()
        .filter(|l| l.left_slot == "storage.maintenance" || l.right_slot == "storage.maintenance")
        .collect();
    let configs: Vec<_> = node
        .grants
        .iter()
        .filter(|g| g.slot == "storage.maintenance-config")
        .collect();
    if workers.is_empty() && links.is_empty() && configs.is_empty() {
        if root.join("maintenance.conf").exists() {
            return Err("maintenance config without explicit private recipient".into());
        }
        return Ok(());
    }
    if workers.len() != 1 || links.len() != 1 || configs.len() != 2 {
        return Err(
            "maintenance requires exactly one private connected worker and config pair".into(),
        );
    }
    let worker = workers[0];
    let link = links[0];
    if link.left_slot != "storage.maintenance" || link.right_slot != "storage.maintenance" {
        return Err("maintenance endpoints must use the canonical private slot".into());
    }
    let target = if link.right == worker.name {
        &link.left
    } else if link.left == worker.name {
        &link.right
    } else {
        return Err("maintenance endpoint does not reach its designated worker".into());
    };
    if !node
        .services
        .iter()
        .any(|s| &s.name == target && s.image == "image.storage")
    {
        return Err("maintenance endpoint target must be actual storage service".into());
    }
    if node
        .links
        .iter()
        .filter(|l| l.left == worker.name || l.right == worker.name)
        .count()
        != 1
        || node
            .grants
            .iter()
            .filter(|g| g.service == worker.name)
            .count()
            != 1
    {
        return Err("maintenance worker must not receive tenant/admission capabilities".into());
    }
    for (service, source) in [
        (target.as_str(), "maintenance.server-config"),
        (worker.name.as_str(), "maintenance.client-config"),
    ] {
        if !configs
            .iter()
            .any(|g| g.service == service && g.source == source)
        {
            return Err("maintenance requires canonical identical immutable config grants".into());
        }
    }
    let config_file = match target.as_str() {
        "storage" => "storage.conf",
        "local" => "local.conf",
        _ => return Err("maintenance requires an explicitly selected native raw owner".into()),
    };
    let config =
        moruna_storage_format::config::BootConfig::decode_page(&binary_asset(root, config_file)?)
            .map_err(|_| "invalid maintenance target storage config")?;
    if !matches!(
        config.provider,
        moruna_storage_format::config::Provider::Raw { .. }
            | moruna_storage_format::config::Provider::Local { .. }
    ) {
        return Err("remote bucket proxy cannot own native media maintenance".into());
    }
    moruna_storage_format::config::MaintenanceConfig::decode_page(&binary_asset(
        root,
        "maintenance.conf",
    )?)
    .map_err(|_| "invalid finite canonical maintenance config")?;
    Ok(())
}

// This allowlist only enables explicitly authored one-job proof commands.
fn validate_kd3_command(config: &engine::Config) -> Result<(), String> {
    fn path(value: &str) -> bool {
        value.starts_with("/kd3/")
            && !value.contains("..")
            && !value.chars().any(char::is_whitespace)
    }
    let fixture = config.program == "/moruna/bin/moruna-kd3-fixture"
        && matches!(config.args.as_slice(), [run, slot, adapter] if run == "run-grant" && slot == "kd3.case" && adapter == "native")
        && config.grants.iter().any(|grant| {
            grant.slot == "kd3.case"
                && matches!(&grant.source, engine::Source::Data(from) if from == "kd3.case")
        });
    let library = config.program == "/opt/python-std/bin/python3"
        && matches!(config.args.as_slice(), [script, matrix, case, adapter, staging, mode]
            if script == "/kd3/kd3_library.py" && path(matrix) && matrix.ends_with(".json")
            && ["library.iterator", "library.python.source", "library.python.sink", "library.python.kernel"].contains(&case.as_str()) && adapter == "native"
            && staging.starts_with("/tmp/kd3/") && !staging.contains("..")
            && !staging.chars().any(char::is_whitespace)
            && ["rows", "adapter-refusal"].contains(&mode.as_str()));
    let nonce = config
        .env
        .iter()
        .filter_map(|env| env.strip_prefix("MORUNA_KD3_PROOF_NONCE="))
        .collect::<Vec<_>>();
    if config.runs != 1
        || !(fixture || library)
        || nonce.len() != 1
        || nonce[0].len() != 32
        || !nonce[0].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("KD3 proof requires one exact fixture command and explicit nonce".into());
    }
    Ok(())
}

#[cfg(test)]
mod kd3_command_tests {
    #[test]
    fn fixed_native_command_and_nonce_are_required() {
        let base = "program /moruna/bin/moruna-kd3-fixture\narg run-grant\narg kd3.case\narg native\nenv MORUNA_KD3_PROOF_NONCE=11111111111111111111111111111111\nmemory 805306368\nscratch 268435456\ngrant kd3.case data kd3.case\nruns 1\n";
        let config = engine::Config::parse(base).unwrap();
        super::validate_kd3_command(&config).unwrap();
        let mut wrong = config.clone();
        wrong.args[1] = "/kd3/../secret.json".into();
        assert!(super::validate_kd3_command(&wrong).is_err());
        let mut wrong = config.clone();
        wrong.env.clear();
        assert!(super::validate_kd3_command(&wrong).is_err());
        let mut wrong = config;
        wrong.args[2] = "linux".into();
        assert!(super::validate_kd3_command(&wrong).is_err());
        let mut library = wrong;
        library.program = "/opt/python-std/bin/python3".into();
        library.args = [
            "/kd3/kd3_library.py",
            "/kd3/matrix.json",
            "library.iterator",
            "native",
            "/tmp/kd3/staging/one",
            "rows",
        ]
        .map(str::to_owned)
        .to_vec();
        super::validate_kd3_command(&library).unwrap();
        library.args[4] = "/kd3/staging/one".into();
        assert!(super::validate_kd3_command(&library).is_err());
        library.args[4] = "/tmp/kd3/../outside".into();
        assert!(super::validate_kd3_command(&library).is_err());
    }
}

fn validate_initial_services(initial: &moruna_node::InitialJob) -> Result<(), String> {
    for slot in ["linux", "network", "storage.admission", "local.admission"] {
        if let Some(grant) = initial.grants.iter().find(|grant| grant.slot == slot)
            && !grant.listener
        {
            return Err(format!(
                "initial {slot} must be retained by admission as a service"
            ));
        }
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args: Vec<_> = std::env::args().collect();
    let proof_command = args
        .last()
        .is_some_and(|arg| arg == "--kd3-conformance-proof");
    if proof_command {
        if !cfg!(feature = "kd3-conformance-proof") {
            return Err("KD3 command assembly requires explicit nondefault proof feature".into());
        }
        args.pop();
    }
    let [_, description, assets, ram, cpus, port] = args.as_slice() else {
        return Err("usage: moruna-node-image DESCRIPTION ASSETS NODE_RAM CPUS AGENT_PORT".into());
    };
    let ram: u64 = ram.parse().map_err(|_| "invalid node RAM")?;
    let cpus: u32 = cpus.parse().map_err(|_| "invalid CPU count")?;
    let port: u32 = port.parse().map_err(|_| "invalid agent port")?;
    if ram < 256 * 1024 * 1024
        || ram % (2 * 1024 * 1024) != 0
        || cpus == 0
        || cpus > 64
        || port == 0
    {
        return Err("invalid fixed VM resources".into());
    }
    let assets = Path::new(assets);
    let description =
        Description::parse(&fs::read_to_string(description).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if description.processes.len() != 1 || description.processes[0].program != "node-supervisor" {
        return Err("standalone image must start only node-supervisor".into());
    }
    let process = &description.processes[0];
    for (slot, file) in [
        ("config", "node.conf"),
        ("initial.job", "initial.job"),
        ("engine.config", "engine.conf"),
    ] {
        if !process
            .handles
            .iter()
            .any(|h| h.slot == slot && h.source == DescSource::Data(file.into()))
        {
            return Err(format!("boot {slot} must bind exact {file}"));
        }
    }
    let node = moruna_node::Config::parse(&page(assets, "node.conf")?, ram)
        .map_err(|e| format!("node config: {e:?}"))?;
    if node.services.iter().any(|s| s.image == "image.maintenance") {
        for slot in ["maintenance.server-config", "maintenance.client-config"] {
            if !process
                .handles
                .iter()
                .any(|h| h.slot == slot && h.source == DescSource::Data("maintenance.conf".into()))
            {
                return Err(
                    "maintenance participants require the same immutable canonical asset".into(),
                );
            }
        }
    }
    let admission = node
        .admission
        .as_ref()
        .ok_or("initial node requires explicit admission bootstrap")?;
    for (slot, source) in [
        ("initial-job", "initial.job"),
        ("initial-config", "engine.config"),
    ] {
        if !node
            .grants
            .iter()
            .any(|g| &g.service == admission && g.slot == slot && g.source == source)
        {
            return Err(format!("admission {slot} must bind exact {source}"));
        }
    }
    let network = page(assets, "network.conf")?;
    let network = moruna_netstack::parse(
        std::str::from_utf8(&network).map_err(|_| "network config encoding")?,
    )
    .map_err(|e| format!("network config: {e}"))?;
    let service = node
        .services
        .iter()
        .find(|s| s.image == "image.netstack")
        .ok_or("missing native network service")?;
    let mut linked = Vec::new();
    for link in &node.links {
        if link.left == service.name && link.left_slot.starts_with("listen") {
            linked.push(link.left_slot.as_str());
        }
        if link.right == service.name && link.right_slot.starts_with("listen") {
            linked.push(link.right_slot.as_str());
        }
    }
    if network
        .grants
        .iter()
        .any(|(slot, _)| !linked.contains(&slot.as_str()))
        || linked
            .iter()
            .any(|slot| !network.grants.iter().any(|(s, _)| s.as_str() == *slot))
    {
        return Err("network listener grants must match actual node channel bindings".into());
    }
    if network.settings.tls_unix_seconds == 0 {
        return Err("standalone TLS requires an explicitly injected trusted UTC anchor".into());
    }
    storage_assets(assets, &node)?;
    let initial = moruna_node::InitialJob::parse(&page(assets, "initial.job")?)
        .map_err(|e| format!("initial job: {e:?}"))?;
    validate_initial_services(&initial)?;
    let engine_bytes = page(assets, "engine.conf")?;
    let engine = engine::Config::parse(
        std::str::from_utf8(&engine_bytes).map_err(|_| "engine config encoding")?,
    )?;
    if proof_command {
        validate_kd3_command(&engine)?;
    } else if engine.runs != 1
        || engine.args != ["serve", "--listen", &format!("vsock://-1:{port}")]
    {
        return Err("initial engine must serve exactly one run at declared agent port".into());
    }
    let required = engine
        .memory
        .checked_add(engine.scratch)
        .ok_or("engine budget overflow")?;
    if initial.budget <= required {
        return Err(
            "initial account must cover engine memory, scratch and native supervisor overhead"
                .into(),
        );
    }
    let account = node
        .services
        .iter()
        .find(|s| &s.name == admission)
        .ok_or("missing admission service")?;
    if account.account_bytes <= initial.budget {
        return Err("admission account lacks initial child budget and service overhead".into());
    }
    let contract = format!(
        "{{\"node_memory_bytes\":{ram},\"job_memory_bytes\":{},\"job_account_bytes\":{},\"scratch_bytes\":{},\"cpus\":{cpus}}}\n",
        engine.memory, initial.budget, engine.scratch
    );
    fs::write(assets.join("node-resources.json"), contract).map_err(|e| e.to_string())?;
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("moruna-node-image: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;
    #[test]
    fn initial_service_connections_outlive_the_first_job() {
        for slot in ["linux", "network", "storage.admission", "local.admission"] {
            let transferred = moruna_node::InitialJob::parse(
                format!("moruna-node-job 1\nbudget 8192\ngrant {slot} initial.endpoint\n")
                    .as_bytes(),
            )
            .unwrap();
            assert!(validate_initial_services(&transferred).is_err());
            let retained = moruna_node::InitialJob::parse(
                format!("moruna-node-job 1\nbudget 8192\nservice {slot} initial.endpoint\n")
                    .as_bytes(),
            )
            .unwrap();
            assert!(validate_initial_services(&retained).is_ok());
        }
    }
    fn config(extra: &str) -> moruna_node::Config {
        moruna_node::Config::parse(format!(
            "moruna-node 1\nservice storage image.storage 4096\nservice worker image.maintenance 4096\nservice tenant image.engine 4096\n{extra}"
        ).as_bytes(), 64 * 1024).unwrap()
    }
    #[test]
    fn maintenance_cannot_be_routed_to_a_tenant_or_extra_worker_channel() {
        let grants = "grant storage storage.maintenance-config maintenance.server-config\ngrant worker storage.maintenance-config maintenance.client-config\n";
        let tenant = config(&format!(
            "{grants}link storage storage.maintenance tenant storage.maintenance\n"
        ));
        assert!(maintenance_assets(Path::new("unused-assets"), &tenant).is_err());
        let extra = config(&format!(
            "{grants}link storage storage.maintenance worker storage.maintenance\nlink worker public tenant listener\n"
        ));
        assert!(maintenance_assets(Path::new("unused-assets"), &extra).is_err());
    }
    #[test]
    fn maintenance_worker_cannot_receive_runtime_authority_grants() {
        let node = config(
            "link storage storage.maintenance worker storage.maintenance\ngrant storage storage.maintenance-config maintenance.server-config\ngrant worker storage.maintenance-config maintenance.client-config\ngrant worker admission private-authority\n",
        );
        assert!(maintenance_assets(Path::new("unused-assets"), &node).is_err());
    }
    #[test]
    fn maintenance_accepts_bounded_raw_owner_and_refuses_bucket_proxy() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!(
            "moruna-node-maintenance-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        fs::create_dir(&root.0).unwrap();
        let node = config(
            "link storage storage.maintenance worker storage.maintenance\ngrant storage storage.maintenance-config maintenance.server-config\ngrant worker storage.maintenance-config maintenance.client-config\n",
        );
        fs::write(root.0.join("maintenance.conf"), b"moruna-storage-maintenance 1\ninterval-ns 1000000000\nmax-objects 1024\nmax-roots 16\nmetadata-bytes 65536\npage-io 4096\n").unwrap();
        let common =
            "moruna-storage 1\ntimeout-ns 1000000000\nbody-limit 1048576\nsession-limit 4\n";
        fs::write(
            root.0.join("storage.conf"),
            format!("{common}provider checkpoint-metadata\nblock-service nvme0\nalignment 4096\n"),
        )
        .unwrap();
        maintenance_assets(&root.0, &node).unwrap();
        fs::write(root.0.join("storage.conf"), format!(
            "{common}provider bucket\nhost objects.example\nport 443\ntls-name objects.example\ncredential-bytes 32\n"
        )).unwrap();
        assert!(
            maintenance_assets(&root.0, &node)
                .unwrap_err()
                .contains("bucket")
        );
    }
}
