//! Explicit ephemeral encrypted disk scratch. Shares the cache envelope, key and recycle
//! authority; no separate crypto parser, implicit bucket fallback or retention promise.
#![forbid(unsafe_code)]
use crate::{
    Access, Action, Error, Metadata, Object, Result, Scope, Version,
    cache::{Cache, Lookup},
};
use moruna_kernel::Buffer;
use std::sync::Arc;

/// Immutable ephemeral fact; producer identity is provenance, not an authorization grant.
#[derive(Clone, Debug)]
pub struct Handle {
    /// Requester that produced this spill/scratch object.
    pub producer: Scope,
    /// Exact identity/version/length, reusable only while this node cache epoch exists.
    pub metadata: Metadata,
}
/// Explicitly selected disk scratch on the same encrypted cache epoch and byte ledger.
pub struct Scratch {
    cache: Arc<Cache>,
}
impl Scratch {
    /// Bind the existing private encrypted cache; no new key provider or permissive verifier.
    pub fn new(cache: Arc<Cache>) -> Self {
        Self { cache }
    }
    /// Encrypt and publish immutable scratch under a fresh version. Returns the original
    /// charged plaintext buffer after encryption/write/decryption, plus truthful metadata.
    pub async fn write(
        &self,
        access: &Access,
        object: &Object,
        buffer: Buffer,
    ) -> Result<(Handle, Buffer)> {
        if buffer.host_ptr().is_none() {
            return Err(Error::Invalid("scratch host source"));
        }
        let bytes = buffer.as_ref();
        let hash = blake3::hash(bytes).to_hex().to_string();
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce).map_err(|_| Error::Unavailable("OS entropy"))?;
        let version = Version {
            etag: Some(hash),
            generation: Some(blake3::hash(&nonce).to_hex().to_string()),
        };
        let metadata = Metadata {
            object: object.clone(),
            version: version.clone(),
            bytes: bytes.len() as u64,
        };
        let buffer = self
            .cache
            .store(access, object, &version, 0, buffer, Action::Write)
            .await?;
        Ok((
            Handle {
                producer: Scope {
                    workspace: access.workspace.clone(),
                    run: access.run.clone(),
                },
                metadata,
            },
            buffer,
        ))
    }
    /// Read the exact immutable scratch object, freshly authorized on every hit. Missing
    /// state cannot be regenerated silently or fetched from an unrelated durable home.
    pub async fn read(
        &self,
        access: &Access,
        handle: &Handle,
        destination: Buffer,
    ) -> Result<Buffer> {
        handle.producer.validate()?;
        if destination.len() as u64 != handle.metadata.bytes {
            return Err(Error::Invalid("scratch destination length"));
        }
        match self
            .cache
            .read(
                access,
                &handle.metadata.object,
                &handle.metadata.version,
                0,
                destination,
            )
            .await?
        {
            Lookup::Hit(buffer) => Ok(buffer),
            Lookup::Miss(_) => Err(Error::Missing),
        }
    }
}
