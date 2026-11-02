//! The engine's ACTUAL source filesystem capability, delegated by its Linux task.
//! A pathname is only a request on that capability, never permission by itself.
use imagefs_protocol::{Bytes, imagefs};
use moruna_svc::{Channel, ObjectType, Rights};
use std::sync::{Mutex, OnceLock};
const TIMEOUT: u64 = 5_000_000_000;
/// Export one existing source directory as a real READ-only subtree session.
/// The producer retains source Scratch/Account/lifetime in the canonical service;
/// the receiving Linux task mounts that capability at its actor-local `/assets`.
pub fn export_directory(path: &std::path::Path) -> Result<Channel, String> {
    if !path.is_absolute() {
        return Err("asset source path must be absolute in the actual engine view".into());
    }
    let path = path
        .to_str()
        .ok_or("native asset source path is not UTF-8")?;
    if path.is_empty() || path.len() > 4096 || path.as_bytes().contains(&0) {
        return Err("invalid asset source path".into());
    }
    static SOURCE: OnceLock<Result<Mutex<imagefs::Client>, String>> = OnceLock::new();
    let client = SOURCE
        .get_or_init(|| {
            let source = crate::start::take("filesystem")
                .ok_or("missing actual source filesystem startup capability")?;
            let (kind, rights) = source
                .info()
                .map_err(|e| format!("filesystem capability info: {}", e.name()))?;
            if kind != ObjectType::Channel || !rights.contains(Rights::WRITE) {
                return Err("source filesystem startup capability is not a service Channel".into());
            }
            // This is the actual actor's unhandshaken session endpoint, NOT a
            // service listener: the generated protocol Hello is the only handshake.
            let channel = Channel(source);
            let mut client =
                imagefs::Client::connect(channel, moruna_svc::now().saturating_add(TIMEOUT))
                    .map_err(|e| format!("actual imagefs3 handshake: {e:?}"))?;
            client.core().set_timeout(TIMEOUT);
            Ok(Mutex::new(client))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let mut client = client
        .lock()
        .map_err(|_| "actual source filesystem client poisoned")?;
    client
        .export_directory(Bytes(path.as_bytes().to_vec()))
        .map_err(|e| format!("exporting actual readonly asset directory: {e:?}"))
}
