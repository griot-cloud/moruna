//! The child's confinement, in the child, in the order of 17 f.5: the cgroup, a mount namespace
//! holding only its tree, `/tmp`, the interpreter's import roots and three devices; no
//! capabilities and `no_new_privs`; Landlock; seccomp.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};

/// Where the tree is inside the child's namespace.
pub const TREE: &str = "/pkg";
/// The devices the child's namespace holds.
pub const DEVICES: [&str; 3] = ["null", "zero", "urandom"];

/// What the child is confined to (17 f.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confinement {
    /// The stage's cgroup, which Moruna made.
    pub cgroup: PathBuf,
    /// An empty directory the namespace's root is mounted on.
    pub root: PathBuf,
    /// The package's tree, mounted read only at [`TREE`].
    pub tree: PathBuf,
    /// The interpreter's import roots, mounted read only at their own paths.
    pub roots: Vec<PathBuf>,
}

/// A step of f.5 that failed, named.
#[derive(Debug)]
pub struct Failed {
    /// The step.
    pub step: &'static str,
    /// What the system said.
    pub error: String,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}

fn step<T>(step: &'static str, r: io::Result<T>) -> Result<T, Failed> {
    r.map_err(|e| Failed {
        step,
        error: e.to_string(),
    })
}

fn c(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path with a NUL"))
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        let e = io::Error::last_os_error();
        Err(io::Error::new(e.kind(), format!("{what}: {e}")))
    }
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let source = source.map(c).transpose()?;
    let target_c = c(target)?;
    let fstype = fstype
        .map(|t| CString::new(t).map_err(io::Error::other))
        .transpose()?;
    let data = data
        .map(|d| CString::new(d).map_err(io::Error::other))
        .transpose()?;
    // SAFETY: every pointer is a NUL-terminated string that outlives the call, or null.
    let r = unsafe {
        libc::mount(
            source.as_ref().map_or(core::ptr::null(), |s| s.as_ptr()),
            target_c.as_ptr(),
            fstype.as_ref().map_or(core::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(core::ptr::null(), |s| s.as_ptr().cast()),
        )
    };
    check(r, &format!("mount {}", target.display()))
}

/// Bind `source` at `target`, `nosuid`: a directory read only and `nodev`, a device read and
/// write (`/dev/null` is written to).
fn bind(source: &Path, target: &Path, device: bool) -> io::Result<()> {
    mount(
        Some(source),
        target,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )?;
    let flags = if device {
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID
    } else {
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV
    };
    mount(None, target, None, flags, None)
}

/// The import roots with every root inside another one left out: it is visible already.
pub fn outermost(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = roots.iter().filter(|r| r.is_absolute()).cloned().collect();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for root in sorted {
        if !out.iter().any(|kept| root.starts_with(kept)) {
            out.push(root);
        }
    }
    out
}

/// The path of `absolute` under `root`.
fn under(root: &Path, absolute: &Path) -> PathBuf {
    root.join(absolute.strip_prefix("/").unwrap_or(absolute))
}

/// Join the cgroup: write this process's pid to its `cgroup.procs`, and open `memory.current`,
/// whose descriptor the child's sampler keeps after the namespace hides the file.
pub fn join_cgroup(cgroup: &Path) -> Result<File, Failed> {
    let procs = cgroup.join("cgroup.procs");
    step(
        "cgroup",
        fs::OpenOptions::new()
            .write(true)
            .open(&procs)
            .and_then(|mut f| f.write_all(std::process::id().to_string().as_bytes()))
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", procs.display()))),
    )?;
    step("cgroup", File::open(cgroup.join("memory.current")))
}

/// The mount namespace (f.5 steps 2 to 4).
fn namespace(c: &Confinement) -> io::Result<()> {
    // SAFETY: unshare takes flags only.
    check(unsafe { libc::unshare(libc::CLONE_NEWNS) }, "unshare")?;
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;
    let root = &c.root;
    mount(
        Some(Path::new("tmpfs")),
        root,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755,size=1m"),
    )?;
    let pkg = under(root, Path::new(TREE));
    fs::create_dir_all(&pkg)?;
    bind(&c.tree, &pkg, false)?;
    for import_root in outermost(&c.roots) {
        if !import_root.is_dir() {
            continue;
        }
        let at = under(root, &import_root);
        fs::create_dir_all(&at)?;
        bind(&import_root, &at, false)?;
    }
    let dev = root.join("dev");
    fs::create_dir_all(&dev)?;
    for device in DEVICES {
        let at = dev.join(device);
        File::create(&at)?;
        bind(&Path::new("/dev").join(device), &at, true)?;
    }
    let tmp = root.join("tmp");
    fs::create_dir_all(&tmp)?;
    mount(
        Some(Path::new("tmpfs")),
        &tmp,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777"),
    )?;
    let old = root.join(".old");
    fs::create_dir_all(&old)?;
    let new_root = c_path(root)?;
    let put_old = c_path(&old)?;
    // SAFETY: two NUL-terminated paths; pivot_root has no libc wrapper.
    check(
        unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) } as i32,
        "pivot_root",
    )?;
    std::env::set_current_dir("/")?;
    let old_c = CString::new("/.old").map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated path.
    check(
        unsafe { libc::umount2(old_c.as_ptr(), libc::MNT_DETACH) },
        "umount /.old",
    )?;
    fs::remove_dir("/.old")?;
    // The root itself is read only from here: the kernel makes nothing at `/`.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )
}

fn c_path(p: &Path) -> io::Result<CString> {
    c(p)
}

/// No capabilities in any set, and `no_new_privs` (f.5 step 5).
fn drop_privileges() -> io::Result<()> {
    // SAFETY: prctl with integer arguments.
    check(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        "no_new_privs",
    )?;
    for cap in 0..64 {
        // SAFETY: prctl with integer arguments; a capability the kernel does not know is EINVAL,
        // which ends the walk.
        let r = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        if r != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            return Err(io::Error::new(
                e.kind(),
                format!("drop capability {cap}: {e}"),
            ));
        }
    }
    // SAFETY: prctl with integer arguments.
    check(
        unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        },
        "clear ambient capabilities",
    )?;
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    // _LINUX_CAPABILITY_VERSION_3: two data words.
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capset with a version-3 header and the two data words that version reads.
    check(
        unsafe { libc::syscall(libc::SYS_capset, &mut header as *mut Header, data.as_ptr()) }
            as i32,
        "capset",
    )
}

/// Landlock over the namespace's paths (f.5 step 6): read and execute under the tree and the
/// import roots, read and write under `/tmp` and on the devices, nothing else. A kernel without
/// Landlock refuses the child.
fn landlock(roots: &[PathBuf]) -> Result<(), String> {
    let abi = ABI::V1;
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);
    let mut readable: Vec<PathBuf> = vec![PathBuf::from(TREE)];
    readable.extend(outermost(roots).into_iter().filter(|r| r.is_dir()));
    let mut ruleset = Ruleset::default()
        .handle_access(all)
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;
    for path in &readable {
        let fd = PathFd::new(path).map_err(|e| format!("{}: {e}", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, read))
            .map_err(|e| e.to_string())?;
    }
    let tmp = PathFd::new("/tmp").map_err(|e| format!("/tmp: {e}"))?;
    ruleset = ruleset
        .add_rule(PathBeneath::new(tmp, all))
        .map_err(|e| e.to_string())?;
    for device in DEVICES {
        let path = Path::new("/dev").join(device);
        let fd = PathFd::new(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                fd,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
            .map_err(|e| e.to_string())?;
    }
    let status = ruleset.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "Landlock is not available ({:?}); a .pex child is not started without it",
            status.ruleset
        ));
    }
    Ok(())
}

/// The whole of f.5 after the cgroup: the namespace, the privileges, Landlock, seccomp.
pub fn confine(c: &Confinement) -> Result<(), Failed> {
    step("namespace", namespace(c))?;
    step("capabilities", drop_privileges())?;
    landlock(&c.roots).map_err(|error| Failed {
        step: "landlock",
        error,
    })?;
    crate::seccomp::apply(std::process::id()).map_err(|error| Failed {
        step: "seccomp",
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_roots_are_left_out() {
        let roots = vec![
            PathBuf::from("/opt/py/lib/python3.14/lib-dynload"),
            PathBuf::from("/opt/py/lib/python3.14"),
            PathBuf::from("relative"),
            PathBuf::from("/opt/py/lib/python3.14"),
            PathBuf::from("/srv/site"),
        ];
        assert_eq!(
            outermost(&roots),
            vec![
                PathBuf::from("/opt/py/lib/python3.14"),
                PathBuf::from("/srv/site")
            ]
        );
        assert_eq!(
            under(Path::new("/r"), Path::new("/a/b")),
            PathBuf::from("/r/a/b")
        );
        let f = Failed {
            step: "x",
            error: "y".into(),
        };
        assert_eq!(f.to_string(), "x: y");
    }
}
