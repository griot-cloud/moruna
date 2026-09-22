//! The manifest a run leaves beside the data.
//!
//! It records what was written, from which seed, by which generator version and
//! on which machine, with a BLAKE3 digest per file so that a later run can be
//! compared without keeping both copies. The manifest is not itself a dataset:
//! it names the host, so it moves between machines while the datasets do not.

use std::path::Path;

use serde_json::{Value, json};

use crate::dataset::Written;
use crate::error::{BenchError, Result};
use crate::host;

/// The manifest file name inside the output directory.
pub const FILE_NAME: &str = "manifest.json";

/// Build the manifest document.
pub fn build(seed: u64, scale: &str, written: &[Written]) -> Value {
    let files: Vec<Value> = written
        .iter()
        .map(|file| {
            json!({
                "dataset": file.dataset,
                "file": file.relative,
                "bytes": file.bytes,
                "blake3": file.hash,
                "detail": file.detail,
            })
        })
        .collect();
    json!({
        "generator": "amoru-bench",
        "version": crate::GENERATOR_VERSION,
        "format": crate::GENERATOR_FORMAT,
        "traces_to": "architecture/sdd/00-preamble.md section 6.5; AMB1 from 01-contracts.md section e.4",
        "seed": seed,
        "scale": scale,
        "machine": host::machine(),
        "files": files,
    })
}

/// Write the manifest into `out_dir` and return its path.
pub fn write(
    out_dir: &Path,
    seed: u64,
    scale: &str,
    written: &[Written],
) -> Result<std::path::PathBuf> {
    let path = out_dir.join(FILE_NAME);
    let text = serde_json::to_string_pretty(&build(seed, scale, written))?;
    std::fs::write(&path, format!("{text}\n")).map_err(|e| BenchError::io("write", &path, e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory unique to this process and call; see the note in
    /// `dataset.rs`, several gates run at once on one machine.
    fn tmp(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "amoru-bench-manifest-{}-{}-{}",
            std::process::id(),
            n,
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn written() -> Vec<Written> {
        vec![Written {
            dataset: "identity-mixed".into(),
            relative: "identity-mixed.parquet".into(),
            path: "out/identity-mixed.parquet".into(),
            bytes: 4096,
            hash: "abc".into(),
            detail: "parquet: 10 rows".into(),
        }]
    }

    #[test]
    fn the_manifest_names_the_generator_the_seed_and_the_machine() {
        let value = build(7, "small", &written());
        assert_eq!(value["generator"], "amoru-bench");
        assert_eq!(value["version"], crate::GENERATOR_VERSION);
        assert_eq!(value["format"], crate::GENERATOR_FORMAT);
        assert_eq!(value["seed"], 7);
        assert_eq!(value["scale"], "small");
        assert_eq!(value["machine"], host::machine());
        assert!(
            value["traces_to"]
                .as_str()
                .unwrap_or_default()
                .contains("6.5")
        );
        let files = value["files"].as_array().expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["file"], "identity-mixed.parquet");
        assert_eq!(files[0]["blake3"], "abc");
        assert_eq!(files[0]["bytes"], 4096);
    }

    #[test]
    fn the_manifest_is_written_as_json_with_a_trailing_newline() {
        let dir = tmp("manifest");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write(&dir, 1, "full", &written()).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.ends_with("\n"));
        let parsed: Value = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed["seed"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_into_a_missing_directory_is_an_error() {
        let dir = tmp("manifest-missing").join("deeper");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            write(&dir, 1, "full", &written()),
            Err(BenchError::Io { .. })
        ));
    }
}
