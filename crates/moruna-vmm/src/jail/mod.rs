//! The jail a run's VM monitor (QEMU) and its vsock back end start in, so that code which
//! escapes the VM lands in a jail holding nothing of the node or of any other run.
//!
//! Each jailed process is created by `clone3` directly into its own user, mount, PID, network,
//! IPC, UTS and cgroup namespaces, and, when the host delegates a cgroup, directly into the
//! run's cgroup leaf (`CLONE_INTO_CGROUP`). Before it executes its program it:
//!
//! - maps the one user and group it may map without a privileged helper (the host's
//!   `NoNewPrivileges` service has none): the launching user and group on the host become
//!   [`JAIL_ID`] inside, and nothing else is mapped;
//! - sets `user.max_user_namespaces` of its own user namespace to zero, so it cannot make a
//!   nested user namespace and the capabilities that come with one;
//! - mounts a small tmpfs as its root holding only the files and directories its [`Entry`]s
//!   name, each bound at its host path, read-only unless the entry says otherwise, with no
//!   set-user-ID and no device files (except `/dev/kvm` when asked), then `pivot_root`s into it,
//!   detaches the node's whole file system and makes the tmpfs read-only;
//! - keeps a new, empty network namespace (only `lo`, down): the Unix sockets bound into its
//!   root are its only channels;
//! - is PID 1 of a new PID namespace, so it sees no other process and no other process of the
//!   node sees into it except through its parent;
//! - sets `no_new_privs`, locks the securebits, empties its bounding and ambient sets and drops
//!   every capability, then executes its program with an empty environment.
//!
//! Identity on the host: a jailed process keeps the launching user's uid, gid and supplementary
//! groups (the `kvm` group opens `/dev/kvm`), because an unprivileged process can map only its
//! own identity. Every run has its own user namespace, but two runs of one service share that
//! host uid; they are kept apart by the mount, PID, network and IPC namespaces (neither can name
//! the other's files, processes, sockets or IPC objects) and by the system-call filters, not by
//! the uid. A distinct host uid per run needs `CAP_SETUID` and `CAP_SETGID` or a set-user-ID
//! `newuidmap`, which a `NoNewPrivileges` service without capabilities does not have.
//!
//! The jail holds no secret: it is given the run's artifacts, its disks and its sockets, and an
//! empty environment. The caller removes it, with the run's directory, when the run ends.

mod cgroup;
mod process;
#[cfg(target_os = "linux")]
pub mod seccomp;
#[cfg(target_os = "linux")]
mod spawn;
#[cfg(all(test, target_os = "linux"))]
mod tests;

pub(crate) use cgroup::{Leaf, Size};
pub(crate) use process::Jailed;
#[cfg(target_os = "linux")]
pub(crate) use spawn::{Spec, spawn};

/// One program to start in a jail (the jail is Linux's; elsewhere `spawn` refuses, and the
/// launcher has refused before it builds one).
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) struct Spec<'a> {
    pub program: &'a Path,
    pub args: Vec<std::ffi::OsString>,
    pub entries: Vec<Entry>,
    pub root: &'a Path,
    pub stdout: fs::File,
    pub stderr: fs::File,
    pub cgroup: Option<i32>,
}

/// The jail needs Linux's namespaces; no other host starts a run.
#[cfg(not(target_os = "linux"))]
pub(crate) fn spawn(spec: Spec<'_>) -> Result<Jailed> {
    Err(VmmError::Unsupported(format!(
        "the jail for {} needs Linux namespaces",
        spec.program.display()
    )))
}

use crate::error::{Result, VmmError};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

/// The user and group id a jailed process has inside its user namespace. It is not zero, so
/// executing a program never grants capabilities; on the host it is the launching user.
pub const JAIL_ID: u32 = 1000;

/// Where a run's processes are accounted. Chosen explicitly by the caller: a host that is
/// expected to delegate a cgroup and does not is refused, never run unaccounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cgroup {
    /// A cgroup v2 directory delegated to this process, with the `cpu`, `memory` and `pids`
    /// controllers enabled in its `cgroup.subtree_control`. Each run gets a new leaf under it
    /// sized to the run's budget; the leaf is removed when the run ends.
    Delegated(PathBuf),
    /// The host delegates no cgroup: the run is bounded only by its VM's fixed memory and CPUs.
    Undelegated,
}

impl Cgroup {
    /// Parse the command-line form: an absolute cgroup v2 directory, or `none`.
    pub fn parse(text: &str) -> Result<Self> {
        if text == "none" {
            return Ok(Cgroup::Undelegated);
        }
        let path = PathBuf::from(text);
        if !path.is_absolute() {
            return Err(VmmError::config(
                "--cgroup",
                "an absolute delegated cgroup v2 directory, or none",
            ));
        }
        Ok(Cgroup::Delegated(path))
    }
}

/// Whether a jailed process may write what an [`Entry`] shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Read-only.
    Read,
    /// Read and write.
    Write,
}

/// One thing a jail shows, bound at the same absolute path inside as on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A regular file (followed if the host path is a symbolic link); `exec` keeps it
    /// executable and mappable as code, every other file is `noexec`.
    File {
        /// Absolute, normalised path, the same inside the jail and on the host.
        path: PathBuf,
        /// Read-only or writable.
        access: Access,
        /// Executable (a program or a shared library).
        exec: bool,
    },
    /// A directory and everything under it, `noexec`.
    Dir {
        /// Absolute, normalised path.
        path: PathBuf,
        /// Read-only or writable.
        access: Access,
    },
    /// A character device (only `/dev/kvm`); the one entry without `nodev`.
    Device {
        /// Absolute path of the device node.
        path: PathBuf,
    },
}

impl Entry {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Entry::File { path, .. } | Entry::Dir { path, .. } | Entry::Device { path } => path,
        }
    }
}

fn bad(message: impl Into<String>) -> VmmError {
    VmmError::config("jail", message)
}

/// `path` with `.` and `..` removed lexically; it must be absolute.
pub(crate) fn normalise(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            Component::Prefix(_) => return Err(bad("path prefix unsupported")),
        }
    }
    if !path.is_absolute() {
        return Err(bad(format!("{} is not absolute", path.display())));
    }
    Ok(out)
}

/// Resolve a program as `execvp` would: a name without a slash is searched for in `PATH`,
/// anything else is taken as given. The result is canonical.
pub(crate) fn resolve_program(program: &Path) -> Result<PathBuf> {
    let found = if program.components().count() == 1 && !program.is_absolute() {
        let search = std::env::var_os("PATH").ok_or_else(|| {
            bad(format!(
                "{} named without a path and PATH unset",
                program.display()
            ))
        })?;
        std::env::split_paths(&search)
            .map(|dir| dir.join(program))
            .find(|candidate| {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(candidate)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| bad(format!("{} is not on PATH", program.display())))?
    } else {
        program.to_path_buf()
    };
    found
        .canonicalize()
        .map_err(|e| VmmError::io("resolve", found.display().to_string(), &e))
}

/// The program interpreter an ELF executable names (`PT_INTERP`), or `None` for a static one.
pub(crate) fn interpreter(program: &Path) -> Result<Option<PathBuf>> {
    let what = || program.display().to_string();
    let mut file = fs::File::open(program).map_err(|e| VmmError::io("open", what(), &e))?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header)
        .map_err(|e| VmmError::io("read", what(), &e))?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err(bad(format!(
            "{} is not a little-endian ELF64 executable",
            what()
        )));
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32_at =
        |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]));
    let u64_at =
        |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]));
    let phoff = u64_at(&header, 32);
    let phentsize = usize::from(u16_at(&header, 54));
    let phnum = usize::from(u16_at(&header, 56));
    if phentsize < 56 || phnum > 128 {
        return Err(bad(format!("{} has malformed program headers", what())));
    }
    let bytes = fs::read(program).map_err(|e| VmmError::io("read", what(), &e))?;
    for index in 0..phnum {
        let at = usize::try_from(phoff)
            .ok()
            .and_then(|o| o.checked_add(index * phentsize))
            .filter(|at| at + 56 <= bytes.len())
            .ok_or_else(|| bad(format!("{} program header out of bounds", what())))?;
        if u32_at(&bytes, at) != 3 {
            continue;
        }
        let offset = usize::try_from(u64_at(&bytes, at + 8)).unwrap_or(usize::MAX);
        let size = usize::try_from(u64_at(&bytes, at + 32)).unwrap_or(usize::MAX);
        let raw = offset
            .checked_add(size)
            .and_then(|end| bytes.get(offset..end))
            .ok_or_else(|| bad(format!("{} interpreter out of bounds", what())))?;
        let name = raw.split(|b| *b == 0).next().unwrap_or_default();
        let name = std::str::from_utf8(name)
            .map_err(|_| bad(format!("{} interpreter is not UTF-8", what())))?;
        if !name.starts_with('/') {
            return Err(bad(format!(
                "{} interpreter {name} is not absolute",
                what()
            )));
        }
        return Ok(Some(PathBuf::from(name)));
    }
    Ok(None)
}

/// The files a dynamically linked program loads, as its own interpreter resolves them: the
/// interpreter is run in list mode (`ld.so --list`, which maps the libraries and runs none of
/// the program's code) with an empty environment, the environment the jailed program gets.
fn libraries(interpreter: &Path, program: &Path) -> Result<Vec<PathBuf>> {
    let output = std::process::Command::new(interpreter)
        .arg("--list")
        .arg(program)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| VmmError::io("run", format!("{} --list", interpreter.display()), &e))?;
    if !output.status.success() {
        return Err(bad(format!(
            "{} --list {} failed ({}): {}",
            interpreter.display(),
            program.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| bad("dynamic loader listing is not UTF-8"))?;
    let mut found = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let path = match line.split_once(" => ") {
            Some((name, target)) => {
                if target.starts_with("not found") {
                    return Err(bad(format!(
                        "{} needs {name}, which is not found",
                        program.display()
                    )));
                }
                target.split(" (").next().unwrap_or_default()
            }
            None => line.split(" (").next().unwrap_or_default(),
        };
        // The vDSO and similar are listed without a path: the kernel provides them.
        if path.starts_with('/') {
            found.push(PathBuf::from(path));
        }
    }
    Ok(found)
}

/// The entries that let `program` (canonical) start in a jail: itself, its interpreter, its
/// libraries at the paths the interpreter opens them by, and the interpreter's cache of library
/// paths when the host has one. Everything is read-only.
pub(crate) fn program_entries(program: &Path) -> Result<Vec<Entry>> {
    let exec = |path: PathBuf| Entry::File {
        path,
        access: Access::Read,
        exec: true,
    };
    let mut entries = vec![exec(program.to_path_buf())];
    if let Some(interpreter) = interpreter(program)? {
        for library in libraries(&interpreter, program)? {
            entries.push(exec(normalise(&library)?));
        }
        entries.push(exec(normalise(&interpreter)?));
        let cache = Path::new("/etc/ld.so.cache");
        if cache.is_file() {
            entries.push(Entry::File {
                path: cache.to_path_buf(),
                access: Access::Read,
                exec: false,
            });
        }
    }
    Ok(entries)
}

/// QEMU's firmware directories, as QEMU itself lists them (`qemu -L help`, run with the empty
/// environment the jailed QEMU gets; a distribution may add its own, such as SeaBIOS's), each
/// read-only, with every file a symbolic link in them points to outside them. A listed
/// directory or a link that does not exist on the host does not exist in the jail either.
pub(crate) fn qemu_data_entries(qemu: &Path) -> Result<Vec<Entry>> {
    let output = std::process::Command::new(qemu)
        .args(["-L", "help"])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| VmmError::io("run", format!("{} -L help", qemu.display()), &e))?;
    if !output.status.success() {
        return Err(bad(format!(
            "{} -L help failed ({}): {}",
            qemu.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| bad("QEMU data directories are not UTF-8"))?;
    let mut dirs = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let dir = normalise(Path::new(line))?;
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if dirs.is_empty() {
        return Err(bad(format!(
            "{} lists no existing data directory",
            qemu.display()
        )));
    }
    let mut entries: Vec<Entry> = dirs
        .iter()
        .map(|dir| Entry::Dir {
            path: dir.clone(),
            access: Access::Read,
        })
        .collect();
    let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        for item in
            fs::read_dir(&dir).map_err(|e| VmmError::io("list", dir.display().to_string(), &e))?
        {
            let item = item.map_err(|e| VmmError::io("list", dir.display().to_string(), &e))?;
            seen += 1;
            if seen > 20_000 {
                return Err(bad("QEMU data directories exceed twenty thousand entries"));
            }
            let path = item.path();
            let kind = item
                .file_type()
                .map_err(|e| VmmError::io("stat", path.display().to_string(), &e))?;
            if kind.is_dir() && depth < 8 {
                stack.push((path, depth + 1));
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|e| VmmError::io("readlink", path.display().to_string(), &e))?;
                let inside = normalise(&dir.join(target))?;
                if dirs.iter().any(|d| inside.starts_with(d)) {
                    continue;
                }
                if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                    entries.push(Entry::File {
                        path: inside,
                        access: Access::Read,
                        exec: false,
                    });
                }
            }
        }
    }
    Ok(entries)
}

/// Remove duplicates and refuse an entry inside a bound directory (its mount point would be
/// made in the host's directory) or a file bound twice in different ways.
pub(crate) fn settle(entries: Vec<Entry>) -> Result<Vec<Entry>> {
    let mut out: Vec<Entry> = Vec::new();
    let mut paths = BTreeSet::new();
    for entry in entries {
        if !entry.path().is_absolute() || normalise(entry.path())? != entry.path() {
            return Err(bad(format!(
                "{} is not an absolute normalised path",
                entry.path().display()
            )));
        }
        if !paths.insert(entry.path().to_path_buf()) {
            if out.contains(&entry) {
                continue;
            }
            return Err(bad(format!(
                "{} is bound twice in different ways",
                entry.path().display()
            )));
        }
        out.push(entry);
    }
    for entry in &out {
        for dir in &out {
            if let Entry::Dir { path, .. } = dir
                && entry.path() != path
                && entry.path().starts_with(path)
            {
                return Err(bad(format!(
                    "{} lies inside the bound directory {}",
                    entry.path().display(),
                    path.display()
                )));
            }
        }
    }
    Ok(out)
}
