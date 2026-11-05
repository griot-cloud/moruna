//! Explicit TEST-ONLY TCG proof through the real prepared native SDK pipeline.
//! Usage: cargo run -p moruna-runtime --example kd3-native-proof
//! --features native-tcg-proof -- CONFIG.json. Production launch remains KVM-only.
#![deny(unsafe_code)]
#[cfg(feature = "native-tcg-proof")]
#[path = "kd3/common.rs"]
mod common;
#[cfg(feature = "native-tcg-proof")]
mod proof {
    use super::common::{self, Result};
    use moruna_runtime::host::{
        launch_witness::{self, Admission, Phase, Probe},
        native::{self, CancelOutcome, Message, NativeRun, Tools},
        protocol::{Inbound, Outbound},
    };
    use serde::Deserialize;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        fs::{self, File, OpenOptions},
        io::{BufWriter, Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        descriptor: PathBuf,
        job: PathBuf,
        image_digest: PathBuf,
        case: usize,
        node_boot: String,
        attempt: u32,
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        evidence_directory: PathBuf,
        qemu: PathBuf,
        vsock: PathBuf,
        memory_bytes: u64,
        cpus: u32,
        seconds: u64,
        /// Additional actual source/build inputs retained as custody hashes only.
        inputs: BTreeMap<String, PathBuf>,
    }
    fn digest_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
    fn sha(bytes: &[u8]) -> String {
        digest_hex(&Sha256::digest(bytes))
    }
    fn file_hash(path: &Path) -> Result<Value> {
        if !path.is_absolute() {
            return Err("every custody input path must be absolute".into());
        }
        let mut file = File::open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err("custody input is not a regular file".into());
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        let after = file.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
        ) {
            return Err("custody input changed while hashing".into());
        }
        Ok(json!({"path":path,"bytes":after.len(),"sha256":digest_hex(&hash.finalize())}))
    }
    fn create(path: &Path) -> Result<File> {
        Ok(OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?)
    }
    fn persist(path: &Path, value: &Value) -> Result<()> {
        let mut file = create(path)?;
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer_pretty(&mut writer, value)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.sync_all()?;
        File::open(path.parent().ok_or("evidence path has no parent")?)?.sync_all()?;
        Ok(())
    }
    pub fn run() -> Result<()> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        if args.len() != 1 {
            return Err("usage: kd3-native-proof CONFIG.json".into());
        }
        let config_bytes = fs::read(&args[0])?;
        let cfg: Config = serde_json::from_slice(&config_bytes)?;
        if !(1..=5).contains(&cfg.case)
            || ![1, 2, 4].contains(&cfg.cpus)
            || !(10..=600).contains(&cfg.seconds)
        {
            return Err("explicit case1..5/profile1|2|4/deadline10..600 required".into());
        }
        for path in [
            &cfg.descriptor,
            &cfg.job,
            &cfg.image_digest,
            &cfg.state_directory,
            &cfg.runtime_directory,
            &cfg.evidence_directory,
            &cfg.qemu,
            &cfg.vsock,
        ] {
            if !path.is_absolute() {
                return Err("explicit absolute SDK proof paths required".into());
            }
        }
        let description = moruna_vmm::microkernel::inspect(&cfg.descriptor)?;
        if description.resources.cpus != cfg.cpus {
            return Err("actual descriptor CPU profile differs".into());
        }
        let digest = common::image_digest(&cfg.image_digest)?;
        let digest = digest.as_str();
        let spec = moruna_vmm::session::spec_value(&fs::read(&cfg.job)?)?;
        let (name, template) = &common::cases(digest)[cfg.case - 1];
        common::checked_job(template, &spec)?;
        let assigned = spec["run_id"]
            .as_str()
            .ok_or("explicit assigned job run_id required")?
            .to_owned();
        let admission = Admission {
            run_id: moruna_kernel::RunId::from_hex(&assigned).ok_or("bad assigned RunId")?,
            node_boot: moruna_kernel::RunId::from_hex(&cfg.node_boot)
                .filter(|id| id.to_hex() == cfg.node_boot)
                .ok_or("canonical node boot RunId required")?,
            attempt: cfg.attempt,
            directory: &cfg.state_directory,
        };
        let prepared = native::prepare(&admission, Inbound::Spec { spec, plan: None })?;
        fs::create_dir(&cfg.evidence_directory)?;
        fs::set_permissions(&cfg.evidence_directory, fs::Permissions::from_mode(0o700))?;
        // Durable exact engine key BEFORE the sole SDK dispatch call.
        let key = prepared.key().clone();
        persist(
            &cfg.evidence_directory.join("prepared-key.json"),
            &serde_json::to_value(&key)?,
        )?;
        let mut custody = BTreeMap::new();
        for (label, path) in [
            ("descriptor", &cfg.descriptor),
            ("job", &cfg.job),
            ("image_digest", &cfg.image_digest),
            ("qemu", &cfg.qemu),
            ("vsock", &cfg.vsock),
        ] {
            custody.insert(label.to_owned(), file_hash(path)?);
        }
        custody.insert("proof_binary".into(), file_hash(&std::env::current_exe()?)?);
        let base = cfg.descriptor.parent().ok_or("descriptor parent missing")?;
        for (label, path) in [
            ("kernel", &description.kernel.path),
            ("initramfs", &description.initramfs.path),
        ] {
            custody.insert(label.into(), file_hash(&base.join(path))?);
        }
        for (index, layer) in description.layers.iter().enumerate() {
            custody.insert(
                format!("layer:{index}"),
                file_hash(&base.join(&layer.artifact.path))?,
            );
        }
        for (index, disk) in description.writable_disks.iter().enumerate() {
            custody.insert(
                format!("mutable-before:{index}"),
                file_hash(&base.join(&disk.path))?,
            );
        }
        for (label, path) in &cfg.inputs {
            if custody
                .insert(format!("supplied:{label}"), file_hash(path)?)
                .is_some()
            {
                return Err("duplicate custody input".into());
            }
        }
        persist(
            &cfg.evidence_directory.join("inputs.json"),
            &json!({"mode":"explicit-test-tcg","custody":custody,"configuration_sha256":sha(&config_bytes),"compiled_driver_sha256":sha(include_bytes!("kd3-native-proof.rs")),"compiled_reducer_sha256":sha(include_bytes!("kd3/common.rs"))}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(cfg.seconds);
        let mut run = match NativeRun::start_prepared_tcg_proof(
            admission,
            &cfg.descriptor,
            moruna_vmm::Budget {
                memory_bytes: cfg.memory_bytes,
                memory_max_bytes: cfg.memory_bytes,
                cpus: cfg.cpus,
                cpus_max: cfg.cpus,
            },
            &cfg.runtime_directory,
            Tools {
                qemu: &cfg.qemu,
                vsock: &cfg.vsock,
                cgroup: &moruna_vmm::jail::Cgroup::Undelegated,
            },
            prepared,
            deadline,
        ) {
            Ok(run) => run,
            Err(error) => {
                persist(
                    &cfg.evidence_directory.join("start-error.json"),
                    &json!({"key":key,"disposition":format!("{:?}",error.disposition),"error":error.source.to_string(),"probe":format!("{:?}",launch_witness::probe(&cfg.state_directory,&key))}),
                )?;
                return Err(error.source.into());
            }
        };
        let mut transcript = create(&cfg.evidence_directory.join("messages.jsonl"))?;
        let mut first = true;
        let control = run.control();
        let result = common::converse(
            Some(assigned),
            cfg.case == 4,
            || {
                loop {
                    let message = run.next(deadline)?;
                    let evidence = match &message {
                        Message::Control(value) => json!({"control":value}),
                        Message::Event(value) => json!({"event":value.to_value()}),
                    };
                    serde_json::to_writer(&mut transcript, &evidence)?;
                    transcript.write_all(b"\n")?;
                    transcript.flush()?;
                    if first {
                        first = false;
                        if !matches!(message, Message::Control(Outbound::Hello { .. })) {
                            return Err("SDK did not deliver actual first Hello".into());
                        }
                        continue;
                    }
                    return Ok(match message {
                        Message::Control(value) => common::Message::Control(value),
                        Message::Event(value) => common::Message::Event(value),
                    });
                }
            },
            |message| {
                control.send(message)?;
                Ok(())
            },
        );
        let transcript_sync = transcript.sync_all();
        let owned = format!("{:?}", run.reconcile());
        drop(control);
        // The launcher removes the run's directory with the run: keep the guest console as
        // evidence while it still exists.
        let console = std::fs::copy(
            cfg.runtime_directory.join("serial.log"),
            cfg.evidence_directory.join("serial.log"),
        );
        // Cleanup is attempted before any fallible evidence write below. A disk-full
        // diagnostic must not prevent the real owner from draining/reaping children.
        let terminal = run.cancel();
        let observed = match &result {
            Ok(seen) => json!({"seen":seen}),
            Err(error) => json!({"error":error.to_string()}),
        };
        console?;
        persist(&cfg.evidence_directory.join("observed.json"), &observed)?;
        persist(
            &cfg.evidence_directory.join("owned-before-cleanup.json"),
            &json!({"reconcile":owned}),
        )?;
        let (record, clean) = match &terminal {
            CancelOutcome::Exited(record) => (record, true),
            CancelOutcome::Cancelled(record) => (record, false),
            CancelOutcome::Unknown { record, .. } => (record, false),
        };
        persist(
            &cfg.evidence_directory.join("terminal.json"),
            &json!({"record":record,"outcome":format!("{:?}",terminal),"probe":format!("{:?}",launch_witness::probe(&cfg.state_directory,&key))}),
        )?;
        transcript_sync?;
        let seen = result?;
        let why = common::verdict(cfg.case - 1, &seen, digest)?;
        if !clean
            || record.key != key
            || !matches!(&record.phase,Phase::Exited(code) if Some(*code)==seen.code)
            || !matches!(
                launch_witness::probe(&cfg.state_directory, &key),
                Probe::Exited(_)
            )
        {
            return Err("actual SDK terminal/reaps/durable witness not established".into());
        }
        for (index, disk) in description.writable_disks.iter().enumerate() {
            persist(
                &cfg.evidence_directory
                    .join(format!("mutable-after-{index}.json")),
                &file_hash(&base.join(&disk.path))?,
            )?;
        }
        persist(
            &cfg.evidence_directory.join("result.json"),
            &json!({"version":1,"mode":"explicit-test-tcg","case":cfg.case,"cpus":cfg.cpus,"key":key,"passed":true,"terminal":record,"verdict":why}),
        )?;
        println!("embedder: ok: run {} ({name}): {why}", cfg.case);
        Ok(())
    }
}
#[cfg(feature = "native-tcg-proof")]
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    proof::run()
}
#[cfg(not(feature = "native-tcg-proof"))]
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    Err("requires explicit native-tcg-proof feature; production SDK remains KVM-only".into())
}
