//! A VM run's inputs, on the host (stream D, `architecture/arrow-core-0.7.md` §4 and §6): each
//! input the job names as `disk://NAME` is bound on the command line to a host file or a remote
//! object (`--input NAME=URL`). A local file is attached as it is; a remote object is fetched to
//! the run's staging directory on the host first, because the VM has no network. Each becomes one
//! read-only data disk (`moruna_vmm::data`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::{Allocator, ObjectMetadata, Reactor, Tier};
use moruna_micro::devices;

/// The most bytes one read of a remote object asks for.
pub const CHUNK_BYTES: usize = 8 << 20;

/// Where an input's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A file on this host.
    Local(PathBuf),
    /// An object in a store the reactor reaches (`s3://`, `gs://`, `az://` and their
    /// spellings), fetched before the VM boots.
    Remote(String),
}

/// `NAME=URL` of `--input`: the name a job's `disk://NAME` uses, and where its bytes are.
pub fn parse_binding(text: &str) -> Result<(String, String), String> {
    let (name, url) = text
        .split_once('=')
        .ok_or_else(|| format!("--input {text}: an input is NAME=URL"))?;
    if devices::input_slot(&format!("{}{name}", devices::SCHEME)).is_none() {
        return Err(format!(
            "--input {text}: `{name}` is not one [a-z0-9._-] word of 1 to {} bytes",
            devices::NAME_MAX
        ));
    }
    if url.is_empty() {
        return Err(format!("--input {text}: which file?"));
    }
    Ok((name.to_string(), url.to_string()))
}

/// Where `url` is: a path or `file://` URL on this host, or an object store's URL.
pub fn locate(url: &str) -> Result<Location, String> {
    match url.split_once("://") {
        None => std::path::absolute(url)
            .map(Location::Local)
            .map_err(|e| format!("{url}: {e}")),
        Some(("file", path)) if path.starts_with('/') => Ok(Location::Local(PathBuf::from(path))),
        Some(("file", _)) => Err(format!("{url}: a file URL names an absolute path")),
        Some(("s3" | "s3a" | "gs" | "az" | "abfs" | "azure", rest))
            if rest
                .split_once('/')
                .is_some_and(|(c, k)| !c.is_empty() && !k.is_empty()) =>
        {
            Ok(Location::Remote(url.to_string()))
        }
        Some((scheme, _)) => Err(format!(
            "{url}: an input is a host path, a file:// URL or an s3://, gs:// or az:// object, \
             not {scheme}://"
        )),
    }
}

/// Where the engine in the VM writes its outputs: the root of the output layer (stream C's
/// `OUTPUT_ROOT`, in the engine's scratch).
pub const OUTPUT_ROOT: &str = "/tmp/out";

/// A job document as a VM runs it, and what the host binds for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapted {
    /// The document's bytes as sent.
    pub bytes: Vec<u8>,
    /// Whether they differ from the bytes read.
    pub changed: bool,
    /// Each input name the document reads as `disk://NAME`, and where its bytes are.
    pub bindings: Vec<(String, String)>,
    /// The host directory the document's sink named, where the outputs go.
    pub output: Option<PathBuf>,
}

/// A `disk://` name for `location` (a path or an object's key): its file name, lowercase, the
/// `.parquet` or `.arrow` cut, every other character than `[a-z0-9._-]` a `-`, at most 24 bytes,
/// and unique among `taken`.
pub fn disk_name(location: &str, taken: &mut Vec<String>) -> String {
    let file = location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let file = file
        .strip_suffix(".parquet")
        .or_else(|| file.strip_suffix(".arrow"))
        .unwrap_or(&file);
    let cleaned: String = file
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = match cleaned.trim_matches('-') {
        "" => "input".to_string(),
        b => b.chars().take(devices::NAME_MAX).collect(),
    };
    let mut name = base.clone();
    let mut n = 1;
    while taken.contains(&name) {
        let suffix = format!("-{n}");
        name = format!(
            "{}{suffix}",
            &base[..base.len().min(devices::NAME_MAX - suffix.len())]
        );
        n += 1;
    }
    taken.push(name.clone());
    name
}

/// The files a local input names: a file, or a directory's `*.parquet` files in name order.
fn local_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "parquet") && p.is_file())
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("{}: holds no *.parquet file", path.display()));
    }
    Ok(files)
}

/// A version 1 document (`moruna_spec: 1`) as a VM runs it: given a `run_id` (`mint`) when it
/// has none, as a host run mints one at its start; each input of its source that is not a
/// `disk://` device bound to a data disk of a name made from its file's; a sink on this host
/// written to the VM's output root instead, its directory the run's output directory. `sidecar`
/// (`JOB.inputs.json`, `{"NAME": "PATH or URL"}`) binds names the document already reads as
/// `disk://NAME`. A version 2 document is taken as it is; its inputs are bound by the sidecar.
pub fn adapt(
    bytes: &[u8],
    sidecar: Option<&[u8]>,
    mint: impl FnOnce() -> String,
) -> Result<Adapted, String> {
    let mut bindings: Vec<(String, String)> = Vec::new();
    if let Some(sidecar) = sidecar {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(sidecar)
            .map_err(|e| format!("the inputs file is not a JSON object of NAME: PATH: {e}"))?;
        for (name, value) in map {
            let url = value
                .as_str()
                .ok_or_else(|| format!("the inputs file's {name} is not a path"))?;
            let (name, url) = parse_binding(&format!("{name}={url}"))?;
            bindings.push((name, url));
        }
    }
    let unchanged = |bindings| Adapted {
        bytes: bytes.to_vec(),
        changed: false,
        bindings,
        output: None,
    };
    if bytes.get(4..8) == Some(b"MJB2".as_slice()) {
        return Ok(unchanged(bindings));
    }
    let mut doc: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the document is not JSON: {e}"))?;
    if doc.get("moruna_spec") != Some(&serde_json::json!(1)) {
        return Ok(unchanged(bindings));
    }
    let mut changed = false;
    if doc.get("run_id").is_none_or(serde_json::Value::is_null) {
        doc["run_id"] = serde_json::Value::String(mint());
        changed = true;
    }
    let mut taken: Vec<String> = bindings.iter().map(|(n, _)| n.clone()).collect();
    if let Some(url) = doc.pointer_mut("/source/url") {
        let urls: Vec<String> = match &*url {
            serde_json::Value::String(one) => vec![one.clone()],
            serde_json::Value::Array(many) => many
                .iter()
                .map(|u| u.as_str().map(str::to_string))
                .collect::<Option<_>>()
                .ok_or("the source's urls are not strings")?,
            _ => return Err("the source's url is not a string or a list".into()),
        };
        let mut devices_urls = Vec::new();
        for given in urls {
            if given.starts_with(devices::SCHEME) {
                devices_urls.push(given);
                continue;
            }
            let found: Vec<String> = match locate(&given)? {
                Location::Local(path) => local_files(&path)?
                    .into_iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                Location::Remote(url) => vec![url],
            };
            for location in found {
                let name = disk_name(&location, &mut taken);
                devices_urls.push(format!("{}{name}", devices::SCHEME));
                bindings.push((name, location));
            }
            changed = true;
        }
        *url = match devices_urls.as_slice() {
            [one] => serde_json::Value::String(one.clone()),
            _ => serde_json::Value::Array(
                devices_urls
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        };
    }
    let mut output = None;
    if let Some(url) = doc.pointer_mut("/sink/url")
        && let Some(given) = url.as_str().map(str::to_string)
    {
        let inside = |p: &Path| p.starts_with(OUTPUT_ROOT);
        match locate(&given) {
            Ok(Location::Local(path)) if !inside(&path) => {
                output = Some(path);
                *url = serde_json::Value::String(format!("file://{OUTPUT_ROOT}/"));
                changed = true;
            }
            Ok(Location::Local(_)) => {}
            _ => {
                return Err(format!(
                    "the sink writes to {given}; a run in a VM writes its outputs to its output \
                     disk, so its sink names a directory on this host"
                ));
            }
        }
    }
    if !changed {
        return Ok(unchanged(bindings));
    }
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    Ok(Adapted {
        bytes,
        changed,
        bindings,
        output,
    })
}

/// The inputs file beside a job document: `JOB.inputs.json` for `JOB.json`.
pub fn sidecar_of(spec: &Path) -> PathBuf {
    let name = spec
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".json").unwrap_or(&name);
    spec.with_file_name(format!("{stem}.inputs.json"))
}

/// The input names the job's tasks use, each once, in the order they first appear: every
/// string in a task's settings that is a `disk://` URL.
pub fn names_used(job: &moruna_job::Job) -> Result<Vec<String>, String> {
    fn visit(value: &serde_json::Value, out: &mut Vec<String>) -> Result<(), String> {
        match value {
            serde_json::Value::String(s) if s.starts_with(devices::SCHEME) => {
                if devices::input_slot(s).is_none() {
                    return Err(format!(
                        "`{s}` names no input: a data disk is disk://NAME, NAME one [a-z0-9._-] \
                         word of 1 to {} bytes",
                        devices::NAME_MAX
                    ));
                }
                let name = s[devices::SCHEME.len()..].to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, out)?;
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    visit(item, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut out = Vec::new();
    for task in &job.tasks {
        for value in task.settings.values() {
            visit(value, &mut out)?;
        }
    }
    Ok(out)
}

/// Refuse a run whose job uses an input the image does not declare, before anything is
/// staged or booted.
pub fn check_used(used: &[String], declared: &[String]) -> Result<(), String> {
    match used.iter().find(|name| !declared.contains(name)) {
        Some(name) => Err(format!(
            "the job reads disk://{name}, and the image declares {}",
            if declared.is_empty() {
                "no inputs".to_string()
            } else {
                format!("only {}", declared.join(", "))
            }
        )),
        None => Ok(()),
    }
}

/// The object stores the job names, for fetching its remote inputs on the host: endpoints and
/// containers from the document, credentials from where the host runs (its environment, its
/// identity), as the engine's own reactor reads them.
pub fn object_store_of(doc: &moruna_job::ObjectStoreDoc) -> moruna_reactor::ObjectStoreConfig {
    moruna_reactor::ObjectStoreConfig {
        s3: doc.s3.as_ref().map(|s| moruna_reactor::S3Config {
            endpoint: s.endpoint.clone(),
            region: s.region.clone(),
            bucket: s.bucket.clone(),
            ..moruna_reactor::S3Config::default()
        }),
        gcs: doc.gcs.as_ref().map(|g| moruna_reactor::GcsConfig {
            bucket: g.bucket.clone(),
            ..moruna_reactor::GcsConfig::default()
        }),
        azure: doc.azure.as_ref().map(|a| moruna_reactor::AzureConfig {
            account: a.account.clone(),
            container: a.container.clone(),
            ..moruna_reactor::AzureConfig::default()
        }),
        local_root: doc.local_root.as_deref().map(PathBuf::from),
        allow_http: doc.allow_http,
    }
}

/// A reactor for fetching objects on the host, over its own small arena.
pub fn fetcher(
    store: moruna_reactor::ObjectStoreConfig,
) -> Result<(Arc<moruna_reactor::Reactor>, Arc<dyn Allocator>), String> {
    let arena = moruna_arena::Arena::new(moruna_arena::ArenaConfig {
        host_bytes: (4 * CHUNK_BYTES) as u64,
        ..moruna_arena::ArenaConfig::default()
    })
    .map_err(|e| format!("the staging arena: {e}"))?;
    let alloc: Arc<dyn Allocator> = arena;
    let reactor = moruna_reactor::Reactor::new(
        moruna_reactor::ReactorConfig {
            object_store: store,
            ..moruna_reactor::ReactorConfig::default()
        },
        alloc.clone(),
    )
    .map_err(|e| format!("the staging reactor: {e}"))?;
    Ok((reactor, alloc))
}

/// Fetch object `url` whole into `to`, a file that does not exist yet; its length.
pub fn fetch(
    reactor: &moruna_reactor::Reactor,
    alloc: &dyn Allocator,
    url: &str,
    to: &Path,
) -> Result<u64, String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let size = ObjectMetadata::head_object(reactor, url)
        .wait()
        .map_err(|e| format!("{url}: {e}"))?
        .size;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .map_err(|e| format!("{}: {e}", to.display()))?;
    let mut offset = 0u64;
    while offset < size {
        let want = (size - offset).min(CHUNK_BYTES as u64) as usize;
        let mut buffer = alloc
            .alloc(want, Tier::Host)
            .map_err(|e| format!("{url}: {e}"))?;
        if buffer.len() > want {
            buffer = buffer.split_at(want).0;
        }
        let read = Reactor::read_object(reactor, url, offset, buffer)
            .wait()
            .map_err(|e| format!("{url} at {offset}: {e}"))?;
        let read = Arc::new(read);
        let view = read.view();
        let bytes = view
            .as_host_slice()
            .ok_or_else(|| format!("{url}: a read landed off the host"))?;
        file.write_all(bytes)
            .map_err(|e| format!("{}: {e}", to.display()))?;
        offset += bytes.len() as u64;
        if bytes.is_empty() {
            return Err(format!(
                "{url}: the object ended at {offset} of {size} bytes"
            ));
        }
    }
    file.sync_all()
        .map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(size)
}

/// Every binding as the launcher takes it: local files in place, remote objects fetched into
/// `staging` (a private directory of this run, removed with it) under their input's name.
pub fn stage(
    bindings: &[(String, String)],
    staging: &Path,
    store: &moruna_job::ObjectStoreDoc,
) -> Result<Vec<moruna_vmm::data::Input>, String> {
    let mut fetcher_once = None;
    let mut inputs = Vec::with_capacity(bindings.len());
    for (name, url) in bindings {
        let path = match locate(url)? {
            Location::Local(path) => path,
            Location::Remote(url) => {
                if fetcher_once.is_none() {
                    fetcher_once = Some(fetcher(object_store_of(store))?);
                }
                let (reactor, alloc) = fetcher_once.as_ref().expect("made above");
                let to = staging.join(format!("input-{name}"));
                fetch(reactor, alloc.as_ref(), &url, &to)?;
                to
            }
        };
        inputs.push(moruna_vmm::data::Input {
            name: name.clone(),
            path,
        });
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher's copy of the data devices' contract is the frozen one (`devices.rs`).
    #[test]
    fn the_launcher_attaches_disks_as_the_engine_reads_them() {
        use moruna_vmm::data;
        assert_eq!(data::DATA_DISK_DEVICE_ID, devices::DATA_DISK_DEVICE_ID);
        assert_eq!(data::NAME_MAX, devices::NAME_MAX);
        assert_eq!(data::SERIAL_MAX, devices::SERIAL_MAX);
        for len in [0, 1, 511, 512, 123_456_789, u64::MAX] {
            assert_eq!(data::serial_of(len), devices::serial_of(len));
            assert_eq!(
                devices::length_of(data::serial_of(len).as_bytes()),
                Some(len)
            );
        }
        let (longest, longer) = ("x".repeat(24), "x".repeat(25));
        for name in [
            "nyc",
            "a.b_c-d",
            "",
            "Upper",
            "a/b",
            longest.as_str(),
            longer.as_str(),
        ] {
            assert_eq!(
                data::valid_name(name),
                devices::input_slot(&format!("{}{name}", devices::SCHEME)).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_disk_name_is_made_from_the_file_name() {
        let mut taken = Vec::new();
        assert_eq!(
            disk_name("/d/yellow_tripdata_2024-01.parquet", &mut taken),
            "yellow_tripdata_2024-01"
        );
        assert_eq!(disk_name("/d/x/Part 0.ARROW", &mut taken), "part-0");
        assert_eq!(disk_name("s3://b/k/data.parquet", &mut taken), "data");
        assert_eq!(disk_name("/d/data.parquet", &mut taken), "data-1");
        assert_eq!(disk_name("/d/???.parquet", &mut taken), "input");
        let long = format!("/d/{}.parquet", "a".repeat(40));
        assert_eq!(disk_name(&long, &mut taken), "a".repeat(24));
        assert_eq!(
            disk_name(&long, &mut taken),
            format!("{}-1", "a".repeat(22))
        );
        for name in &taken {
            assert!(parse_binding(&format!("{name}=/x")).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_document_is_adapted_to_the_vm() {
        let dir = std::env::temp_dir().join(format!(
            "moruna-adapt-{}-{}",
            std::process::id(),
            crate::run::mint_run_id().unwrap().to_hex()
        ));
        std::fs::create_dir_all(dir.join("in")).unwrap();
        for f in ["b.parquet", "a.parquet", "notes.txt"] {
            std::fs::write(dir.join("in").join(f), b"x").unwrap();
        }
        std::fs::write(dir.join("one.parquet"), b"x").unwrap();
        let doc = serde_json::json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": [
                dir.join("in").display().to_string(),
                format!("file://{}", dir.join("one.parquet").display()),
                "disk://kept",
            ]},
            "kernels": [],
            "sink": {"kind": "parquet", "url": dir.join("out").display().to_string()},
            "budget": {"memory_bytes": 1u64 << 30, "cpu": 2},
        });
        let adapted = adapt(
            &serde_json::to_vec(&doc).unwrap(),
            Some(br#"{"kept": "/data/kept.parquet"}"#),
            || "ab".repeat(16),
        )
        .unwrap();
        assert!(adapted.changed);
        let sent: serde_json::Value = serde_json::from_slice(&adapted.bytes).unwrap();
        assert_eq!(sent["run_id"], "ab".repeat(16));
        assert_eq!(
            sent["source"]["url"],
            serde_json::json!(["disk://a", "disk://b", "disk://one", "disk://kept"])
        );
        assert_eq!(sent["sink"]["url"], "file:///tmp/out/");
        assert_eq!(adapted.output.as_deref(), Some(dir.join("out").as_path()));
        let names: Vec<&str> = adapted.bindings.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["kept", "a", "b", "one"]);
        assert_eq!(
            adapted.bindings[1].1,
            dir.join("in/a.parquet").display().to_string()
        );
        // What the engine reads is what is bound.
        let job = crate::host::session::read_document(&adapted.bytes).unwrap();
        assert_eq!(names_used(&job).unwrap(), ["a", "b", "one", "kept"]);
        // A document a VM can run as it is stays as it is, byte for byte.
        let ready = serde_json::json!({"moruna_spec": 1, "run_id": "cd".repeat(16),
            "source": {"kind": "parquet", "url": "disk://x"}, "kernels": [],
            "sink": {"kind": "parquet", "url": "file:///tmp/out/r/"},
            "budget": {"memory_bytes": 1u64 << 30, "cpu": 1}});
        let bytes = serde_json::to_vec(&ready).unwrap();
        let same = adapt(&bytes, None, || unreachable!()).unwrap();
        assert!(!same.changed);
        assert_eq!(same.bytes, bytes);
        assert_eq!(same.output, None);
        // A version 2 document, or one in binary, is the sidecar's to bind.
        let v2 = adapt(
            br#"{"moruna_spec": 2}"#,
            Some(br#"{"x": "/x"}"#),
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(v2.bindings, [("x".to_string(), "/x".to_string())]);
        assert!(!v2.changed);
        let mut binary = vec![0u8; 16];
        binary[4..8].copy_from_slice(b"MJB2");
        assert!(!adapt(&binary, None, || unreachable!()).unwrap().changed);
        // Refusals: a sink the VM cannot reach, a missing input, a bad inputs file.
        let mut remote = ready.clone();
        remote["sink"]["url"] = serde_json::json!("s3://b/out/");
        assert!(
            adapt(
                &serde_json::to_vec(&remote).unwrap(),
                None,
                || unreachable!()
            )
            .unwrap_err()
            .contains("output disk")
        );
        let mut missing = ready.clone();
        missing["source"]["url"] =
            serde_json::json!(dir.join("absent.parquet").display().to_string());
        assert!(
            adapt(
                &serde_json::to_vec(&missing).unwrap(),
                None,
                || unreachable!()
            )
            .is_err()
        );
        let mut empty = ready.clone();
        empty["source"]["url"] = serde_json::json!(dir.display().to_string());
        std::fs::remove_file(dir.join("one.parquet")).unwrap();
        assert!(
            adapt(
                &serde_json::to_vec(&empty).unwrap(),
                None,
                || unreachable!()
            )
            .unwrap_err()
            .contains("no *.parquet")
        );
        for bad in [&b"[1]"[..], br#"{"Bad": "/x"}"#, br#"{"x": 1}"#] {
            assert!(adapt(&bytes, Some(bad), || unreachable!()).is_err());
        }
        assert!(adapt(b"not json", None, || unreachable!()).is_err());
        assert_eq!(
            sidecar_of(Path::new("/r/job.json")),
            Path::new("/r/job.inputs.json")
        );
        assert_eq!(
            sidecar_of(Path::new("/r/job")),
            Path::new("/r/job.inputs.json")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binding_names_an_input_and_where_it_is() {
        assert_eq!(
            parse_binding("nyc=/data/x.parquet").unwrap(),
            ("nyc".to_string(), "/data/x.parquet".to_string())
        );
        assert_eq!(parse_binding("a=b=c").unwrap().1, "b=c");
        for bad in ["nyc", "=x", "Upper=x", "a/b=x", "nyc="] {
            assert!(parse_binding(bad).is_err(), "{bad}");
        }
        let long = format!("{}=x", "a".repeat(devices::NAME_MAX + 1));
        assert!(parse_binding(&long).is_err());
    }

    #[test]
    fn an_input_is_local_or_a_remote_object() {
        assert_eq!(
            locate("/data/x").unwrap(),
            Location::Local("/data/x".into())
        );
        assert_eq!(
            locate("file:///data/x").unwrap(),
            Location::Local("/data/x".into())
        );
        match locate("relative/x").unwrap() {
            Location::Local(p) => assert!(p.is_absolute() && p.ends_with("relative/x")),
            other => panic!("{other:?}"),
        }
        for url in [
            "s3://b/k",
            "s3a://b/k/l",
            "gs://b/k",
            "az://c/k",
            "abfs://c/k",
            "azure://c/k",
        ] {
            assert_eq!(locate(url).unwrap(), Location::Remote(url.into()), "{url}");
        }
        for bad in [
            "file://relative",
            "https://h/x",
            "s3://bucket",
            "s3:///k",
            "ftp://h/x",
        ] {
            assert!(locate(bad).is_err(), "{bad}");
        }
    }

    fn job(source_url: serde_json::Value) -> moruna_job::Job {
        let doc = serde_json::json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": source_url},
            "kernels": [],
            "sink": {"kind": "parquet", "url": "file:///tmp/out/result/"},
            "budget": {"memory_bytes": 1u64 << 30, "cpu": 1.0},
        });
        crate::host::session::read_document(&serde_json::to_vec(&doc).unwrap()).unwrap()
    }

    #[test]
    fn the_job_says_which_inputs_it_reads() {
        assert_eq!(
            names_used(&job(serde_json::json!("disk://nyc"))).unwrap(),
            ["nyc"]
        );
        assert_eq!(
            names_used(&job(serde_json::json!([
                "disk://b", "disk://a", "disk://b"
            ])))
            .unwrap(),
            ["b", "a"]
        );
        assert!(
            names_used(&job(serde_json::json!("file:///x")))
                .unwrap()
                .is_empty()
        );
        let e = names_used(&job(serde_json::json!("disk://Bad"))).unwrap_err();
        assert!(e.contains("names no input"), "{e}");
        check_used(&["a".into()], &["a".into(), "b".into()]).unwrap();
        let e = check_used(&["c".into()], &["a".into()]).unwrap_err();
        assert!(e.contains("disk://c") && e.contains("only a"), "{e}");
        let e = check_used(&["c".into()], &[]).unwrap_err();
        assert!(e.contains("no inputs"), "{e}");
    }

    #[test]
    fn local_inputs_are_taken_in_place_and_objects_are_fetched() {
        let dir = std::env::temp_dir().join(format!(
            "moruna-inputs-{}-{}",
            std::process::id(),
            crate::run::mint_run_id().unwrap().to_hex()
        ));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("in.bin");
        let body: Vec<u8> = (0..CHUNK_BYTES + 4097).map(|i| (i % 253) as u8).collect();
        std::fs::write(&file, &body).unwrap();
        let store = moruna_job::ObjectStoreDoc::default();
        let staged = stage(&[("a".into(), file.display().to_string())], &dir, &store).unwrap();
        assert_eq!(staged[0].path, file);
        // A fetch through the reactor, as an object store's would go: a file URL here.
        let (reactor, alloc) = fetcher(object_store_of(&store)).unwrap();
        let to = dir.join("fetched");
        let url = format!("file://{}", file.display());
        assert_eq!(
            fetch(&reactor, alloc.as_ref(), &url, &to).unwrap(),
            body.len() as u64
        );
        assert_eq!(std::fs::read(&to).unwrap(), body);
        assert!(
            fetch(&reactor, alloc.as_ref(), &url, &to).is_err(),
            "never over a file"
        );
        let gone = format!("file://{}", dir.join("absent").display());
        assert!(fetch(&reactor, alloc.as_ref(), &gone, &dir.join("g")).is_err());
        assert!(stage(&[("a".into(), "ftp://h/x".into())], &dir, &store).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
