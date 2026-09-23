//! Where a sink's files go and when they become visible (08 f.5, f.8).
//!
//! Two destinations. An object destination is a URL prefix: a file is one `write_object`, and
//! the reactor splits it into parts when it is large (06 f.4), so the object exists at its final
//! name only when the completion resolves. A local destination is a directory: a file is written
//! through `write_file` under a `.tmp` name and becomes visible by `fsync` and `rename`, the same
//! rule the placement manifest follows (09 f.12). Either way no reader ever sees a partial final
//! file (SI-I3).

use std::path::{Path, PathBuf};

use moruna_kernel::{MorunaError, BufferView, Completion, Reactor, Result};

/// The suffix an incomplete local file carries until it is committed.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// A URL prefix a sink writes objects under (f.1, f.5).
#[derive(Clone, Debug)]
pub(crate) struct ObjectPrefix {
    prefix: String,
    /// The directory a `file://` prefix names, which is the only destination this crate can
    /// list and delete under; `None` for every other scheme (f.8).
    local: Option<PathBuf>,
}

impl ObjectPrefix {
    /// Parse a sink URL. A `file://` URL or a bare path is a local directory as well as an
    /// object prefix, so `resume` can list it; every other scheme is a store this crate reaches
    /// through the reactor alone.
    pub(crate) fn parse(url: &str) -> Result<ObjectPrefix> {
        if url.is_empty() {
            return Err(MorunaError::Config {
                name: "sink.url",
                msg: "the sink URL is empty".into(),
            });
        }
        let prefix = url.trim_end_matches('/').to_string();
        let local = match prefix.split_once("://") {
            Some(("file", path)) => Some(PathBuf::from(path)),
            Some(_) => None,
            None => Some(PathBuf::from(&prefix)),
        };
        Ok(ObjectPrefix { prefix, local })
    }

    /// The URL of one file under this prefix.
    pub(crate) fn url_of(&self, name: &str) -> String {
        format!("{}/{}", self.prefix, name)
    }

    /// Submit the whole file as one object write. Submission returns at once (RE-I6); the
    /// object is committed when the completion resolves (f.5).
    pub(crate) fn put(&self, reactor: &dyn Reactor, name: &str, src: BufferView) -> Completion<()> {
        reactor.write_object(&self.url_of(name), src)
    }

    /// The names under this prefix, or `Resume` when the destination cannot be listed, which is
    /// the honest answer for a write-only credential or a store this crate cannot enumerate.
    pub(crate) fn list(&self) -> Result<Vec<String>> {
        match &self.local {
            Some(dir) => list_dir(dir),
            None => Err(MorunaError::Resume("cannot list destination".into())),
        }
    }

    /// Remove one name under this prefix; only a local destination can.
    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        match &self.local {
            Some(dir) => remove_file(&dir.join(name)),
            None => Err(MorunaError::Resume("cannot list destination".into())),
        }
    }
}

/// A directory a sink writes files into through `write_file` (f.2, f.3, f.5).
#[derive(Clone, Debug)]
pub(crate) struct LocalDir {
    dir: PathBuf,
}

impl LocalDir {
    /// Name a directory and create it if it is not there yet.
    pub(crate) fn new(dir: &Path) -> Result<LocalDir> {
        std::fs::create_dir_all(dir).map_err(|e| io_err("create_dir_all", dir, &e))?;
        Ok(LocalDir {
            dir: dir.to_path_buf(),
        })
    }

    /// The path an incomplete file is written at.
    pub(crate) fn tmp_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}{TMP_SUFFIX}"))
    }

    /// The path a committed file has.
    pub(crate) fn final_of(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Make an incomplete file visible under its final name: `fsync` then `rename`, two
    /// syscalls of no payload size, on the thread the last completion resolved on (f.5).
    ///
    /// A reactor that keeps its files somewhere other than this filesystem (contracts d.15:
    /// `FakeReactor` holds them in memory) leaves nothing at the temporary path, and there is
    /// then nothing to rename; the file is committed in the ledger either way. A real reactor
    /// has already resolved the write, so the path is always there and the rename always runs.
    pub(crate) fn commit(&self, name: &str) -> Result<()> {
        let tmp = self.tmp_of(name);
        let file = match std::fs::File::open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err("open", &tmp, &e)),
        };
        file.sync_all().map_err(|e| io_err("fsync", &tmp, &e))?;
        drop(file);
        let dst = self.final_of(name);
        std::fs::rename(&tmp, &dst).map_err(|e| io_err("rename", &dst, &e))
    }

    /// The names in this directory, committed and temporary alike (f.8).
    pub(crate) fn list(&self) -> Result<Vec<String>> {
        list_dir(&self.dir)
    }

    /// Remove one committed file.
    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        remove_file(&self.final_of(name))
    }

    /// Remove every temporary file in the directory, which is what an aborted or resumed sink
    /// leaves behind (SI-I3, f.8).
    pub(crate) fn remove_tmps(&self) -> Result<u64> {
        let mut removed = 0;
        for name in self.list()? {
            if name.ends_with(TMP_SUFFIX) {
                remove_file(&self.dir.join(&name))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn list_dir(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("read_dir", dir, &e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err("read_dir", dir, &e))?;
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove_file", path, &e)),
    }
}

fn io_err(op: &'static str, path: &Path, e: &std::io::Error) -> MorunaError {
    MorunaError::Io {
        op,
        target: path.display().to_string(),
        msg: e.to_string(),
    }
}
