//! The S3 compatible half of the generator.
//!
//! Preamble 6.5 asks for every dataset on local disk and in an S3 compatible
//! store, MinIO in a container. The connection comes from the environment, which
//! is what the MinIO job of `.github/workflows/ci.yml` sets up:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `MORUNA_S3_ENDPOINT` | the store's base URL, for example `http://127.0.0.1:9000` |
//! | `MORUNA_S3_BUCKET` | the bucket to write into |
//! | `AWS_ACCESS_KEY_ID` | the access key |
//! | `AWS_SECRET_ACCESS_KEY` | the secret key |
//! | `MORUNA_S3_PREFIX` | optional key prefix, `bench` by default |
//! | `AWS_REGION` | optional region, `us-east-1` by default |
//!
//! When the four required variables are not all set the upload is skipped with a
//! printed note naming the ones that are missing. That is not an error: the
//! generator's local half is what most hosts can run, and the quality gate runs
//! `cargo test` on hosts with no object store at all.

use std::io::Write;
use std::path::Path;

use object_store::ObjectStoreExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;

use crate::error::{BenchError, Result};

/// The environment variables the upload needs.
pub const REQUIRED: [&str; 4] = [
    "MORUNA_S3_ENDPOINT",
    "MORUNA_S3_BUCKET",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
];

/// A configured S3 compatible destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Target {
    /// The store's base URL.
    pub endpoint: String,
    /// The bucket.
    pub bucket: String,
    /// The key prefix every object is written under.
    pub prefix: String,
    /// The region the signer uses.
    pub region: String,
    access_key_id: String,
    secret_access_key: String,
}

/// What `from_env` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// Every required variable was set.
    Configured(Box<S3Target>),
    /// One or more were missing; the upload is skipped and these are named.
    Missing(Vec<&'static str>),
}

/// Read the environment through `lookup`, which is `std::env::var` in a run and
/// a fixture in a test.
pub fn discover(lookup: impl Fn(&str) -> Option<String>) -> Discovery {
    let mut missing = Vec::new();
    let mut found = Vec::with_capacity(REQUIRED.len());
    for key in REQUIRED {
        match lookup(key).filter(|value| !value.trim().is_empty()) {
            Some(value) => found.push(value),
            None => missing.push(key),
        }
    }
    if !missing.is_empty() {
        return Discovery::Missing(missing);
    }
    Discovery::Configured(Box::new(S3Target {
        endpoint: found[0].clone(),
        bucket: found[1].clone(),
        access_key_id: found[2].clone(),
        secret_access_key: found[3].clone(),
        prefix: lookup("MORUNA_S3_PREFIX")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "bench".to_string()),
        region: lookup("AWS_REGION")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "us-east-1".to_string()),
    }))
}

/// The environment of the running process.
pub fn from_env() -> Discovery {
    discover(|key| std::env::var(key).ok())
}

impl S3Target {
    /// The full object key for a file written under `relative`.
    pub fn key(&self, relative: &str) -> String {
        let prefix = self.prefix.trim_matches('/');
        if prefix.is_empty() {
            relative.to_string()
        } else {
            format!("{prefix}/{relative}")
        }
    }

    /// The `s3://` URL an object lands at, for the printed report.
    pub fn url(&self, relative: &str) -> String {
        format!("s3://{}/{}", self.bucket, self.key(relative))
    }

    fn store(&self) -> Result<object_store::aws::AmazonS3> {
        Ok(AmazonS3Builder::new()
            .with_endpoint(&self.endpoint)
            .with_bucket_name(&self.bucket)
            .with_access_key_id(&self.access_key_id)
            .with_secret_access_key(&self.secret_access_key)
            .with_region(&self.region)
            // MinIO and every other self hosted store addresses buckets by path,
            // and the CI endpoint is plain HTTP on the loopback interface.
            .with_virtual_hosted_style_request(false)
            .with_allow_http(self.endpoint.starts_with("http://"))
            .build()?)
    }

    /// Upload the local files, one object per file, under the key prefix.
    pub fn upload(
        &self,
        files: &[(String, std::path::PathBuf)],
        out: &mut dyn Write,
    ) -> Result<()> {
        let store = self.store()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(BenchError::Runtime)?;
        for (relative, path) in files {
            let bytes = read_file(path)?;
            let key = StorePath::parse(self.key(relative))?;
            runtime.block_on(async {
                store
                    .put(&key, bytes::Bytes::from(bytes).into())
                    .await
                    .map(|_| ())
            })?;
            let _ = writeln!(out, "  uploaded {}", self.url(relative));
        }
        Ok(())
    }
}

impl S3Target {
    /// Read one object back, which is how a test proves the upload landed.
    pub fn download(&self, relative: &str) -> Result<Vec<u8>> {
        let store = self.store()?;
        let key = StorePath::parse(self.key(relative))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(BenchError::Runtime)?;
        let bytes = runtime.block_on(async { store.get(&key).await?.bytes().await })?;
        Ok(bytes.to_vec())
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| BenchError::io("read", path, e))
}

/// The note printed when the upload is skipped.
pub fn skip_note(missing: &[&'static str]) -> String {
    format!(
        "S3 upload skipped: {} not set. Set {} to write the same datasets to an S3 compatible store (MinIO).",
        missing.join(", "),
        REQUIRED.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn full() -> Vec<(&'static str, &'static str)> {
        vec![
            ("MORUNA_S3_ENDPOINT", "http://127.0.0.1:9000"),
            ("MORUNA_S3_BUCKET", "moruna-ci"),
            ("AWS_ACCESS_KEY_ID", "moruna"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]
    }

    #[test]
    fn a_complete_environment_configures_the_target() {
        match discover(env(&full())) {
            Discovery::Configured(target) => {
                assert_eq!(target.bucket, "moruna-ci");
                assert_eq!(target.prefix, "bench");
                assert_eq!(target.region, "us-east-1");
                assert_eq!(target.endpoint, "http://127.0.0.1:9000");
                assert!(target.store().is_ok());
            }
            Discovery::Missing(missing) => panic!("missing {missing:?}"),
        }
    }

    #[test]
    fn the_optional_variables_override_their_defaults() {
        let mut pairs = full();
        pairs.push(("MORUNA_S3_PREFIX", "/runs/2026/"));
        pairs.push(("AWS_REGION", "eu-west-1"));
        match discover(env(&pairs)) {
            Discovery::Configured(target) => {
                assert_eq!(target.region, "eu-west-1");
                assert_eq!(target.key("a/b.parquet"), "runs/2026/a/b.parquet");
                assert_eq!(
                    target.url("a/b.parquet"),
                    "s3://moruna-ci/runs/2026/a/b.parquet"
                );
            }
            Discovery::Missing(missing) => panic!("missing {missing:?}"),
        }
    }

    #[test]
    fn an_empty_prefix_writes_at_the_bucket_root() {
        let mut pairs = full();
        pairs.push(("MORUNA_S3_PREFIX", "/"));
        match discover(env(&pairs)) {
            Discovery::Configured(target) => assert_eq!(target.key("x.mrb1"), "x.mrb1"),
            Discovery::Missing(missing) => panic!("missing {missing:?}"),
        }
    }

    #[test]
    fn a_missing_or_blank_variable_skips_the_upload_and_is_named() {
        let mut pairs = full();
        pairs[2] = ("AWS_ACCESS_KEY_ID", "   ");
        match discover(env(&pairs)) {
            Discovery::Missing(missing) => {
                assert_eq!(missing, vec!["AWS_ACCESS_KEY_ID"]);
                let note = skip_note(&missing);
                assert!(note.contains("AWS_ACCESS_KEY_ID"), "{note}");
                assert!(note.contains("MinIO"), "{note}");
            }
            Discovery::Configured(_) => panic!("should not be configured"),
        }
        match discover(env(&[])) {
            Discovery::Missing(missing) => assert_eq!(missing.len(), REQUIRED.len()),
            Discovery::Configured(_) => panic!("should not be configured"),
        }
    }

    #[test]
    fn an_upload_of_no_files_builds_the_store_and_does_nothing() {
        match discover(env(&full())) {
            Discovery::Configured(target) => {
                let mut out: Vec<u8> = Vec::new();
                target.upload(&[], &mut out).expect("no files, no requests");
                assert!(out.is_empty());
            }
            Discovery::Missing(missing) => panic!("missing {missing:?}"),
        }
    }

    #[test]
    fn the_process_environment_is_read_without_panicking() {
        // The result depends on the host: on a developer machine with no store
        // it is Missing, in the CI MinIO job it is Configured. Both are correct.
        let _ = from_env();
    }
}
