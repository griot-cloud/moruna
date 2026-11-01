//! Explicit controlled CI authority assets, using production codecs and verifier
//! policy. This executable neither boots a node nor supplies application credentials.
use moruna_kernel::RunId;
use moruna_storage_format::{
    Object, Scope,
    auth::{Admission, SessionRole},
    bindings::EngineBindings,
    grants::{Actions, Grant, GrantSet, Resource, Rule},
    wire::MAX_HEADER,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};
fn encode(
    encode: impl FnOnce(&mut [u8]) -> moruna_storage_format::Result<usize>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = vec![0; MAX_HEADER];
    let used = encode(&mut bytes)?;
    bytes.truncate(used.div_ceil(4096) * 4096);
    Ok(bytes)
}
fn write(root: &Path, name: &str, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let storage_proof = args.len() == 4 && args[3] == "--storage-write-proof";
    if args.len() != 3 && !storage_proof {
        return Err(
            "usage: node-ci-assets EXACT_PEER_JOB NEW_ABSOLUTE_ASSETS_DIR [--storage-write-proof]"
                .into(),
        );
    }
    let out = Path::new(&args[2]);
    if !out.is_absolute() {
        return Err("absolute new asset directory required".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(&args[1])?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("job exceeds canonical CI ceiling".into());
    }
    let job = moruna_runtime::host::session::read_document(&bytes).map_err(|(_, e)| e)?;
    let run = job
        .id
        .as_deref()
        .and_then(RunId::from_hex)
        .ok_or("explicit canonical peer job id required")?;
    let scope = Scope {
        workspace: "moruna-ci".into(),
        run: run.to_hex(),
    };
    let admission = Admission {
        scope: scope.clone(),
        role: SessionRole::SingleNodeHome,
    };
    // The existing five image/refusal/cancel cases request no native object IO.
    // This exact, finite READ rule enables genuine admitted home sessions but
    // grants no write/list/publish, arbitrary prefix or ambient application access.
    // The explicit storage-only boot fixture instead admits READ|WRITE of ONE
    // fixed test object across its two distinct boot/run Accounts; no other key.
    let grants = GrantSet::new(vec![Grant {
        id: 1,
        admission: admission.clone(),
        valid_from_ns: 0,
        expires_ns: 3_600_000_000_000,
        rules: vec![Rule {
            resource: Resource::Exact {
                object: Object {
                    workspace: scope.workspace.clone(),
                    bucket: "jobs".into(),
                    key: if storage_proof {
                        "native-io-proof".into()
                    } else {
                        format!("{}/readonly-marker", scope.run)
                    },
                },
                version: None,
            },
            actions: Actions::new(if storage_proof {
                Actions::READ | Actions::WRITE
            } else {
                Actions::READ
            })?,
        }],
    }])?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "version":1,"workspace":scope.workspace,"run":scope.run,
        "account_slot":"account","storage_slot":"storage","cache_slot":null,
        "control_bytes":4194304,"metadata_bytes":65536,"operation_ms":3000,
        "reader":false,"capture":null,"checkpoint":null,"cost":null
    }))?;
    let envelope = EngineBindings {
        admission: admission.clone(),
        payload,
    };
    let bindings = encode(|dst| envelope.encode_into(dst))?;
    moruna_runtime::bootstrap::Bindings::parse(&bindings)?;
    let grant_bytes = encode(|dst| grants.encode_into(dst))?;
    GrantSet::decode_page(&grant_bytes)?;
    let admission_bytes = encode(|dst| admission.encode_into(dst))?;
    fs::DirBuilder::new().mode(0o700).create(out)?;
    write(out, "engine.bindings", &bindings)?;
    write(out, "storage.grants", &grant_bytes)?;
    write(out, "storage.admission", &admission_bytes)?;
    // Canonical engine identity, distinct from the whole input document SHA256.
    // No second JobSpec parser or digest implementation lives in the proof runner.
    let identity = serde_json::to_vec(&serde_json::json!({
        "version": 1, "run_id": run.to_hex(),
        "engine_spec_digest": moruna_plan::job_digest(&job)
    }))?;
    write(out, "identity.json", &identity)?;
    Ok(())
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("node CI assets refused: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
