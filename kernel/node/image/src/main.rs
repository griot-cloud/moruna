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

const USAGE: &str = "usage: moruna-node-image [--profile standalone] DESCRIPTION ASSETS NODE_RAM CPUS AGENT_PORT\n       moruna-node-image --profile cloud DESCRIPTION ASSETS NODE_RAM";

/// The node RAM a profile's accounts are checked against: whole 2 MiB pages, 256 MiB at least.
fn node_ram(ram: &str) -> Result<u64, String> {
    let ram: u64 = ram.parse().map_err(|_| "invalid node RAM")?;
    if ram < 256 * 1024 * 1024 || !ram.is_multiple_of(2 * 1024 * 1024) {
        return Err("invalid fixed VM resources".into());
    }
    Ok(ram)
}

/// The description, which must start the node supervisor and nothing else, binding each
/// `(slot, file)` to exactly that data file.
fn supervisor_only(
    description: &str,
    profile: &str,
    bindings: &[(&str, &str)],
) -> Result<moruna_image::DescProcess, String> {
    let description =
        Description::parse(&fs::read_to_string(description).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if description.processes.len() != 1 || description.processes[0].program != "node-supervisor" {
        return Err(format!("{profile} image must start only node-supervisor"));
    }
    let process = description
        .processes
        .into_iter()
        .next()
        .ok_or("missing node-supervisor")?;
    for (slot, file) in bindings {
        if !process
            .handles
            .iter()
            .any(|h| h.slot == *slot && h.source == DescSource::Data((*file).into()))
        {
            return Err(format!("boot {slot} must bind exact {file}"));
        }
    }
    Ok(process)
}

/// network.conf through the network server's parser, its listener grants matching the node's
/// actual channel bindings: node.conf's links to the network server, and the granting slot of
/// its `restart` record, whose grant must hold `grant` and cover the listener the supervisor
/// mints from it at each start of the service that restarts (item 2.2c).
fn network_config(
    assets: &Path,
    node: &moruna_node::Config,
) -> Result<moruna_netstack::Config, String> {
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
    let restart = node
        .restart
        .as_ref()
        .filter(|record| record.server == service.name);
    if let Some(record) = restart {
        linked.push(record.granting.as_str());
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
    if let Some(record) = restart
        && !network.grants.iter().any(|(slot, granting)| {
            *slot == record.granting
                && granting.rights & netstack_protocol::rights::GRANT != 0
                && granting.covers(&record.policy)
        })
    {
        return Err(format!(
            "the restart record's listener is not one {} can grant",
            record.granting
        ));
    }
    Ok(network)
}

/// The cloud profile (system design 1A, work item 1.1): the supervisor and node.conf, and,
/// when node.conf runs the network server, network.conf. Nothing a launcher's node has (no
/// engine config, agent port, layers or resources contract).
fn cloud(args: &[String]) -> Result<(), String> {
    let [description, assets, ram] = args else {
        return Err(USAGE.into());
    };
    let ram = node_ram(ram)?;
    let assets = Path::new(assets);
    let process = supervisor_only(description, "cloud", &[("config", "node.conf")])?;
    let node = moruna_node::Config::parse(&page(assets, "node.conf")?, ram)
        .map_err(|e| format!("node config: {e:?}"))?;
    if node.services.iter().any(|s| s.image == "image.netstack") {
        if !process.handles.iter().any(|h| {
            h.slot == "network.config" && h.source == DescSource::Data("network.conf".into())
        }) {
            return Err("boot network.config must bind exact network.conf".into());
        }
        network_config(assets, &node)?;
    }
    Ok(())
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    if args.get(1).map(String::as_str) == Some("--profile") {
        match args.get(2).map(String::as_str) {
            Some("standalone") => {
                args.drain(1..3);
            }
            Some("cloud") => return cloud(&args[3..]),
            Some(other) => return Err(format!("unknown profile {other}: standalone or cloud")),
            None => return Err(USAGE.into()),
        }
    }
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
        return Err(USAGE.into());
    };
    let ram = node_ram(ram)?;
    let cpus: u32 = cpus.parse().map_err(|_| "invalid CPU count")?;
    let port: u32 = port.parse().map_err(|_| "invalid agent port")?;
    if cpus == 0 || cpus > 64 || port == 0 {
        return Err("invalid fixed VM resources".into());
    }
    let assets = Path::new(assets);
    let process = supervisor_only(
        description,
        "standalone",
        &[
            ("config", "node.conf"),
            ("initial.job", "initial.job"),
            ("engine.config", "engine.conf"),
        ],
    )?;
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
    // TLS's time is the core's wall clock, which the machine's clock sets at boot (QEMU's
    // `-rtc`): network.conf carries none (system design 3D).
    network_config(assets, &node)?;
    storage_assets(assets, &node)?;
    let initial = moruna_node::InitialJob::parse(&page(assets, "initial.job")?)
        .map_err(|e| format!("initial job: {e:?}"))?;
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
    match run(std::env::args().collect()) {
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

#[cfg(test)]
mod profile_tests {
    use super::*;

    const RAM: &str = "2147483648";

    fn node_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn args(words: &[&str]) -> Vec<String> {
        core::iter::once("moruna-node-image")
            .chain(words.iter().copied())
            .map(String::from)
            .collect()
    }

    /// A fresh directory holding the committed cloud assets, any file replaced by `changes`.
    struct Assets(std::path::PathBuf);
    impl Assets {
        fn new(tag: &str, changes: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir().join(format!(
                "moruna-node-image-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            for name in ["node.conf", "network.conf"] {
                fs::copy(node_dir().join("cloud").join(name), root.join(name)).unwrap();
            }
            fs::copy(node_dir().join("cloud.image"), root.join("cloud.image")).unwrap();
            for (name, text) in changes {
                fs::write(root.join(name), text).unwrap();
            }
            Self(root)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
        fn cloud(&self, ram: &str) -> Result<(), String> {
            let (image, assets) = (self.path("cloud.image"), self.path(""));
            run(args(&["--profile", "cloud", &image, &assets, ram]))
        }
    }
    impl Drop for Assets {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_committed_cloud_profile_validates_and_writes_no_launcher_contract() {
        let image = node_dir().join("cloud.image");
        let assets = node_dir().join("cloud");
        run(args(&[
            "--profile",
            "cloud",
            image.to_str().unwrap(),
            assets.to_str().unwrap(),
            RAM,
        ]))
        .unwrap();
        assert!(!assets.join("node-resources.json").exists());
        let copy = Assets::new("copy", &[]);
        copy.cloud(RAM).unwrap();
        assert!(!copy.0.join("node-resources.json").exists());
    }

    #[test]
    fn an_unknown_or_missing_profile_is_refused() {
        let err = run(args(&["--profile", "bios", "d", "a", RAM])).unwrap_err();
        assert_eq!(err, "unknown profile bios: standalone or cloud");
        assert_eq!(run(args(&["--profile"])).unwrap_err(), USAGE);
    }

    #[test]
    fn cloud_takes_no_cpus_or_agent_port() {
        let assets = Assets::new("arity", &[]);
        let (image, dir) = (assets.path("cloud.image"), assets.path(""));
        let err = run(args(&[
            "--profile",
            "cloud",
            &image,
            &dir,
            RAM,
            "2",
            "5100",
        ]))
        .unwrap_err();
        assert_eq!(err, USAGE);
        assert_eq!(
            run(args(&["--profile", "cloud", &image, &dir])).unwrap_err(),
            USAGE
        );
    }

    #[test]
    fn cloud_refuses_bad_node_ram() {
        let assets = Assets::new("ram", &[]);
        assert_eq!(assets.cloud("lots").unwrap_err(), "invalid node RAM");
        assert_eq!(
            assets.cloud("134217728").unwrap_err(),
            "invalid fixed VM resources"
        );
        assert_eq!(
            assets.cloud("268435457").unwrap_err(),
            "invalid fixed VM resources"
        );
    }

    #[test]
    fn cloud_refuses_a_description_it_does_not_list() {
        let unbound = Assets::new(
            "unbound",
            &[(
                "cloud.image",
                "moruna-image 1\nprocess node-supervisor\n  handle console console\n  handle data:other.conf config\n",
            )],
        );
        assert_eq!(
            unbound.cloud(RAM).unwrap_err(),
            "boot config must bind exact node.conf"
        );
        let other = Assets::new(
            "other",
            &[(
                "cloud.image",
                "moruna-image 1\nprocess devices\n  handle console console\n",
            )],
        );
        assert_eq!(
            other.cloud(RAM).unwrap_err(),
            "cloud image must start only node-supervisor"
        );
        let missing = Assets::new("missing", &[]);
        fs::remove_file(missing.0.join("cloud.image")).unwrap();
        assert!(missing.cloud(RAM).is_err());
    }

    /// The committed node.conf with the network server item 1.3 adds, bound in the description.
    const NETWORKED: &str = "moruna-node 1\nservice devices image.devices 67108864\nservice network image.netstack 67108864\nbootstrap devices devices\ngrant network config network.config\nlink devices directory-net network directory\n";
    const BOUND: &str = "moruna-image 1\nprocess node-supervisor\n  handle console console\n  handle data:node.conf config\n  handle data:network.conf network.config\n";

    #[test]
    fn cloud_validates_node_config_with_its_parser() {
        let budget = Assets::new(
            "budget",
            &[(
                "node.conf",
                "moruna-node 1\nservice devices image.devices 268435456\nservice other image.devices 268435456\n",
            )],
        );
        assert_eq!(
            budget.cloud("268435456").unwrap_err(),
            "node config: Budget"
        );
        let unknown = Assets::new(
            "unknown",
            &[("node.conf", "moruna-node 1\nservices devices\n")],
        );
        assert_eq!(unknown.cloud(RAM).unwrap_err(), "node config: Syntax");
        let missing = Assets::new("nonode", &[]);
        fs::remove_file(missing.0.join("node.conf")).unwrap();
        assert!(missing.cloud(RAM).unwrap_err().starts_with("node.conf: "));
    }

    /// Item 1.1's node.conf, which runs no network server.
    const UNNETWORKED: &str = "moruna-node 1\nservice devices image.devices 67108864\nbootstrap devices devices\ngrant devices drivers drivers\n";

    #[test]
    fn cloud_checks_network_conf_only_when_node_conf_runs_the_network_server() {
        // A profile that runs no network server: network.conf is neither needed nor read.
        let none = Assets::new(
            "nonet",
            &[
                ("node.conf", UNNETWORKED),
                ("network.conf", "not a network config"),
            ],
        );
        none.cloud(RAM).unwrap();
        // With one, network.conf must be bound and pass the network server's parser.
        let unbound = Assets::new(
            "unbound-net",
            &[
                ("node.conf", NETWORKED),
                (
                    "cloud.image",
                    "moruna-image 1\nprocess node-supervisor\n  handle console console\n  handle data:node.conf config\n",
                ),
                ("network.conf", "moruna-netstack 1\n"),
            ],
        );
        assert_eq!(
            unbound.cloud(RAM).unwrap_err(),
            "boot network.config must bind exact network.conf"
        );
        let grantless = Assets::new(
            "grantless",
            &[
                ("node.conf", NETWORKED),
                ("cloud.image", BOUND),
                (
                    "network.conf",
                    "moruna-netstack 1\ninterface wan card=net0 dhcp\n",
                ),
            ],
        );
        assert_eq!(
            grantless.cloud(RAM).unwrap_err(),
            "network config: line 0: no grant: nothing could reach the server"
        );
        let listener = Assets::new(
            "listener",
            &[
                ("node.conf", NETWORKED),
                ("cloud.image", BOUND),
                (
                    "network.conf",
                    "moruna-netstack 1\ninterface wan card=net0 dhcp\ngrant listen-api rights=listen\n",
                ),
            ],
        );
        assert_eq!(
            listener.cloud(RAM).unwrap_err(),
            "network listener grants must match actual node channel bindings"
        );
        let linked = Assets::new(
            "linked",
            &[
                (
                    "node.conf",
                    &format!(
                        "{NETWORKED}service api image.devices 4096\nlink network listen-api api server\n"
                    ),
                ),
                ("cloud.image", BOUND),
                (
                    "network.conf",
                    "moruna-netstack 1\ninterface wan card=net0 dhcp\ngrant listen-api rights=listen\n",
                ),
            ],
        );
        linked.cloud(RAM).unwrap();
        let unreadable = Assets::new(
            "unreadable",
            &[("node.conf", NETWORKED), ("cloud.image", BOUND)],
        );
        fs::remove_file(unreadable.0.join("network.conf")).unwrap();
        assert!(
            unreadable
                .cloud(RAM)
                .unwrap_err()
                .starts_with("network.conf: ")
        );
        let binary = Assets::new(
            "binary",
            &[("node.conf", NETWORKED), ("cloud.image", BOUND)],
        );
        fs::write(binary.0.join("network.conf"), [0xff_u8, 0xfe]).unwrap();
        assert_eq!(binary.cloud(RAM).unwrap_err(), "network config encoding");
        let unserved = Assets::new(
            "unserved",
            &[
                (
                    "node.conf",
                    "moruna-node 1\nservice network image.netstack 67108864\n",
                ),
                ("cloud.image", BOUND),
                (
                    "network.conf",
                    "moruna-netstack 1\ngrant listen-api rights=listen\n",
                ),
            ],
        );
        assert_eq!(
            unserved.cloud(RAM).unwrap_err(),
            "network listener grants must match actual node channel bindings"
        );
    }

    /// The network server's grant for a `restart` record's granting slot (item 2.2c).
    #[test]
    fn cloud_binds_the_restart_record_s_granting_slot() {
        let node = format!(
            "{NETWORKED}service api image.devices 4096\nrestart api network network listen-supervisor rights=listen local=0.0.0.0/0@443 sockets=64\n"
        );
        let network =
            |grant: &str| format!("moruna-netstack 1\ninterface wan card=net0 dhcp\n{grant}\n");
        let granting = network("grant listen-supervisor rights=grant,listen local=0.0.0.0/0@443");
        // The record binds the supervisor's grant, with no link.
        let granted = Assets::new(
            "granted",
            &[
                ("node.conf", &node),
                ("cloud.image", BOUND),
                ("network.conf", &granting),
            ],
        );
        granted.cloud(RAM).unwrap();
        // Without the record, the same grant is bound to nothing.
        let unbound = Assets::new(
            "ungranted",
            &[
                ("node.conf", NETWORKED),
                ("cloud.image", BOUND),
                ("network.conf", &granting),
            ],
        );
        assert_eq!(
            unbound.cloud(RAM).unwrap_err(),
            "network listener grants must match actual node channel bindings"
        );
        // A grant without the `grant` right, or narrower than the listener the record mints,
        // could not mint it at the first start.
        for grant in [
            "grant listen-supervisor rights=listen local=0.0.0.0/0@443",
            "grant listen-supervisor rights=grant local=0.0.0.0/0@443",
            "grant listen-supervisor rights=grant,listen local=0.0.0.0/0@8443",
            "grant listen-supervisor rights=grant,listen local=0.0.0.0/0@443 sockets=16",
        ] {
            let refused = Assets::new(
                "narrow",
                &[
                    ("node.conf", &node),
                    ("cloud.image", BOUND),
                    ("network.conf", &network(grant)),
                ],
            );
            assert_eq!(
                refused.cloud(RAM).unwrap_err(),
                "the restart record's listener is not one listen-supervisor can grant",
                "{grant}"
            );
        }
    }

    #[test]
    fn standalone_is_the_default_and_named_profile_alike() {
        let assets = Assets::new("standalone", &[]);
        let (image, dir) = (assets.path("cloud.image"), assets.path(""));
        let expected = "boot initial.job must bind exact initial.job";
        assert_eq!(
            run(args(&[&image, &dir, RAM, "2", "5100"])).unwrap_err(),
            expected
        );
        assert_eq!(
            run(args(&[
                "--profile",
                "standalone",
                &image,
                &dir,
                RAM,
                "2",
                "5100"
            ]))
            .unwrap_err(),
            expected
        );
        assert_eq!(
            run(args(&[&image, &dir, RAM, "0", "5100"])).unwrap_err(),
            "invalid fixed VM resources"
        );
        assert_eq!(
            run(args(&[&image, &dir, RAM, "2", "port"])).unwrap_err(),
            "invalid agent port"
        );
        assert_eq!(run(args(&[&image, &dir, RAM])).unwrap_err(), USAGE);
    }
}
