//! Durable staging on an object-store prefix (MH 4.7): the run works in its staging directory as
//! always, and every manifest it writes there is copied, as one object, to
//! `<staging.url>/moruna-<run_id>/manifest.json` before the write counts as done. A resume on any
//! machine reads the copies back into its own staging directory and resumes from the newest one
//! of the same job, as from a manifest it had written itself.
//!
//! The copy is the manifest with its staging segments left out: they stay on the machine that
//! wrote them. Its morsels that had a copy in a segment are read again from the source, which the
//! lineage always allows (a repeatable source, CT-I12), and everything the sink committed stays
//! committed. Its state bytes, the sink's and each `Checkpoint` stage's, travel with it.

use std::path::{Path, PathBuf};

use moruna_kernel::{MorunaError, Result, RunId};
use moruna_reactor::ObjectFiles;
use moruna_reactor::files::join;

/// Where a run's manifest is copied under `prefix`.
pub fn manifest_url(prefix: &str, run_id: &RunId) -> String {
    join(prefix, &format!("moruna-{}/manifest.json", run_id.to_hex()))
}

fn refused(msg: String) -> MorunaError {
    MorunaError::Resume(msg)
}

/// The copy of a manifest the store keeps: no segments, no lineage row pointing into one, and
/// durable, because the store outlives the machine.
pub fn store_copy(manifest: &[u8]) -> Result<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(manifest)
        .map_err(|e| refused(format!("the manifest to copy is not JSON: {e}")))?;
    let serde_json::Value::Object(map) = &mut value else {
        return Err(refused("the manifest to copy is not an object".into()));
    };
    map.insert("durable_staging".into(), serde_json::Value::Bool(true));
    map.insert("segments".into(), serde_json::Value::Array(Vec::new()));
    if let Some(serde_json::Value::Array(rows)) = map.get_mut("lineage") {
        for row in rows {
            if let serde_json::Value::Object(row) = row {
                row.insert("disk".into(), serde_json::Value::Null);
            }
        }
    }
    serde_json::to_vec_pretty(&value).map_err(|e| refused(format!("the copy: {e}")))
}

/// Copy the manifest at `path` to `url`.
pub fn copy_manifest(files: &ObjectFiles, path: &Path, url: &str) -> Result<()> {
    let bytes = std::fs::read(path).map_err(|e| MorunaError::Io {
        op: "read_file",
        target: path.display().to_string(),
        msg: e.to_string(),
    })?;
    files.put(url, store_copy(&bytes)?)
}

fn written_ns(bytes: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("written_ns")?
        .as_u64()
}

/// Read every manifest under `prefix` into `staging_dir`, where a resume looks, unless the
/// directory already holds the same run's manifest written as late or later. Returns what was
/// read, as notes.
pub fn fetch(files: &ObjectFiles, prefix: &str, staging_dir: &Path) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for name in files.prefixes(prefix)? {
        let Some(hex) = name.strip_prefix("moruna-") else {
            continue;
        };
        if RunId::from_hex(hex).is_none() {
            continue;
        }
        let url = join(prefix, &format!("{name}/manifest.json"));
        let Some(remote) = files.get(&url)? else {
            continue;
        };
        let Some(remote_ns) = written_ns(&remote) else {
            return Err(refused(format!("{url}: not a manifest (no written_ns)")));
        };
        let dir: PathBuf = staging_dir.join(&name);
        let local = dir.join("manifest.json");
        let local_ns = std::fs::read(&local).ok().as_deref().and_then(written_ns);
        if local_ns.is_some_and(|ns| ns >= remote_ns) {
            continue;
        }
        let io = |op: &'static str, target: &Path, e: std::io::Error| MorunaError::Io {
            op,
            target: target.display().to_string(),
            msg: e.to_string(),
        };
        std::fs::create_dir_all(&dir).map_err(|e| io("create_dir", &dir, e))?;
        let tmp = dir.join("manifest.json.fetched");
        std::fs::write(&tmp, &remote).map_err(|e| io("write_file", &tmp, e))?;
        std::fs::rename(&tmp, &local).map_err(|e| io("rename", &local, e))?;
        notes.push(format!("staging.url: {url} read into {}", local.display()));
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copy_has_no_segments_and_is_durable() {
        let manifest = serde_json::json!({
            "version": 1, "written_ns": 5, "durable_staging": false,
            "segments": [{"stage": 1, "segment": 0, "bytes": 10}],
            "lineage": [{"seq": 3, "disk": {"segment": 0, "offset": 0, "len": 10}}],
            "kernel_states": [{"stage": 1, "instance": 0, "state": "AQ=="}]
        });
        let copy: serde_json::Value =
            serde_json::from_slice(&store_copy(manifest.to_string().as_bytes()).unwrap()).unwrap();
        assert_eq!(copy["durable_staging"], true);
        assert_eq!(copy["segments"], serde_json::json!([]));
        assert_eq!(copy["lineage"][0]["disk"], serde_json::Value::Null);
        assert_eq!(copy["kernel_states"], manifest["kernel_states"]);
        assert!(store_copy(b"[").is_err());
    }

    #[test]
    fn fetch_reads_the_newer_copy_and_keeps_a_newer_local_one() {
        let root = std::env::temp_dir().join(format!("moruna-durable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (store, staging) = (root.join("store"), root.join("staging"));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        let prefix = format!("file://{}", store.display());
        let files = ObjectFiles::new(Default::default()).unwrap();
        let id = RunId([7; 16]);
        let url = manifest_url(&prefix, &id);
        files
            .put(&url, br#"{"written_ns": 10, "run": "store"}"#.to_vec())
            .unwrap();
        files
            .put(&join(&prefix, "other/manifest.json"), b"{}".to_vec())
            .unwrap();
        let notes = fetch(&files, &prefix, &staging).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let local = staging
            .join(format!("moruna-{}", id.to_hex()))
            .join("manifest.json");
        assert!(std::fs::read_to_string(&local).unwrap().contains("store"));
        std::fs::write(&local, br#"{"written_ns": 20, "run": "local"}"#).unwrap();
        assert!(fetch(&files, &prefix, &staging).unwrap().is_empty());
        assert!(std::fs::read_to_string(&local).unwrap().contains("local"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
