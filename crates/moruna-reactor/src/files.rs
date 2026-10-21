//! Whole objects, read and written by a thread that may wait: the copy of a run's manifest that
//! durable staging on an object-store prefix keeps (MH 4.7).
//!
//! A manifest is small and is written by the scheduler's checkpoint thread, which may wait (it is
//! not a worker and not a reactor thread, 10 f.12); the reactor's operations land in arena
//! buffers and complete through a `Completion` that the engine never waits on (PL-I14), so the
//! copy has its own client here: the same URL rules, the same backends and the same socket
//! endpoints as every other object operation (MH 4.6), on a runtime of its own.

use std::sync::Mutex;

use moruna_kernel::{MorunaError, Result};

use crate::ObjectStoreConfig;
use crate::object::{ObjectLayer, io};

/// Whole-object reads and writes on the configuration's stores, each call blocking its caller.
pub struct ObjectFiles {
    layer: ObjectLayer,
    runtime: tokio::runtime::Runtime,
    /// One call at a time: a current-thread runtime is driven by the caller of `block_on`.
    serial: Mutex<()>,
}

impl ObjectFiles {
    /// A client over `cfg`. A backend the configuration does not have is refused at the first
    /// call that names it, as for every object operation.
    pub fn new(cfg: ObjectStoreConfig) -> Result<ObjectFiles> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| MorunaError::Config {
                name: "object_store",
                msg: format!("the object-store client has no runtime: {e}"),
            })?;
        Ok(ObjectFiles {
            layer: ObjectLayer::new(cfg, None),
            runtime,
            serial: Mutex::new(()),
        })
    }

    fn block<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        let _one = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        self.runtime.block_on(future)
    }

    /// Write `bytes` as the object at `url`, replacing what was there in one put.
    pub fn put(&self, url: &str, bytes: Vec<u8>) -> Result<()> {
        let (backend, path) = self.layer.resolve(url)?;
        self.block(backend.put(&path, bytes.into()))
            .map_err(|e| io("put_object", url, &e))
    }

    /// The object at `url`, or `None` when there is none.
    pub fn get(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let (backend, path) = self.layer.resolve(url)?;
        let meta = match self.block(backend.head(&path)) {
            Ok(meta) => meta,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(io("head_object", url, &e)),
        };
        if meta.size == 0 {
            return Ok(Some(Vec::new()));
        }
        self.block(backend.get_range(&path, 0..meta.size))
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| io("read_object", url, &e))
    }

    /// Remove the object at `url`; one that is not there is already removed.
    pub fn delete(&self, url: &str) -> Result<()> {
        let (backend, path) = self.layer.resolve(url)?;
        match self.block(backend.delete(&path)) {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(io("delete_object", url, &e)),
        }
    }

    /// The names directly under the prefix `url` that are prefixes themselves (a directory's
    /// subdirectories, a bucket's `a/` under `url/a/b`), in the store's order.
    pub fn prefixes(&self, url: &str) -> Result<Vec<String>> {
        let (backend, path) = self.layer.resolve(url)?;
        let prefix = (!path.as_ref().is_empty()).then_some(path);
        let (_, prefixes) = self
            .block(backend.list_one_level(prefix))
            .map_err(|e| io("list_prefix", url, &e))?;
        Ok(prefixes
            .iter()
            .filter_map(|p| p.filename().map(str::to_string))
            .collect())
    }
}

/// `prefix` and `name` joined with one `/`.
pub fn join(prefix: &str, name: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        name.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_objects_round_trip_through_a_file_store() {
        let dir = std::env::temp_dir().join(format!("moruna-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let root = format!("file://{}", dir.display());
        let files = ObjectFiles::new(ObjectStoreConfig::default()).expect("client");
        let url = join(&root, "moruna-ab/manifest.json");
        assert_eq!(files.get(&url).expect("absent"), None);
        files.put(&url, b"{\"a\":1}".to_vec()).expect("put");
        files
            .put(&join(&root, "moruna-cd/manifest.json"), b"{}".to_vec())
            .expect("put");
        assert_eq!(
            files.get(&url).expect("present").as_deref(),
            Some(&b"{\"a\":1}"[..])
        );
        let mut names = files.prefixes(&root).expect("listed");
        names.sort();
        assert_eq!(
            names,
            vec!["moruna-ab".to_string(), "moruna-cd".to_string()]
        );
        files.delete(&url).expect("deleted");
        files.delete(&url).expect("deleting what is gone is done");
        assert_eq!(files.get(&url).expect("gone"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
