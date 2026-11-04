//! The jail's properties, each observed from inside a real jail (this test binary re-run as
//! the jailed program, `probe`) and from outside it (`/proc` of the jailed process).

use super::*;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("moruna-jail-{name}-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn errno_of(rc: i64) -> String {
    if rc >= 0 {
        "ok".into()
    } else {
        let e = std::io::Error::last_os_error();
        format!("errno{}", e.raw_os_error().unwrap_or(0))
    }
}

fn walk(dir: &Path, out: &mut Vec<String>) {
    let Ok(items) = fs::read_dir(dir) else { return };
    for item in items.flatten() {
        let path = item.path();
        out.push(path.display().to_string());
        if out.len() > 5000 {
            return;
        }
        if item.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, out);
        }
    }
}

/// Run only as the jailed program (`jail_shows_only_its_entries_and_no_node`): reports what it
/// can see and do, one `PROBE` line each, then waits for the outside to have looked at it.
#[test]
#[ignore = "the jailed half of jail_shows_only_its_entries_and_no_node"]
fn probe() {
    // SAFETY: a plain getter. Outside a jail this probe does nothing.
    if unsafe { libc::getpid() } != 1 {
        return;
    }
    let say = |key: &str, value: String| println!("PROBE {key} {value}");
    // The test tells the probe its paths through the one read-only file it is given, found by
    // walking its root: the only file named `facts`.
    let mut tree = Vec::new();
    walk(Path::new("/"), &mut tree);
    let facts_path = tree
        .iter()
        .find(|p| p.ends_with("/facts"))
        .cloned()
        .unwrap_or_default();
    let facts: BTreeMap<String, String> = fs::read_to_string(&facts_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            l.split_once(' ')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let fact = |k: &str| facts.get(k).cloned().unwrap_or_default();
    for path in &tree {
        say("tree", path.clone());
    }
    // SAFETY: plain getters and system calls with valid arguments throughout this probe.
    unsafe {
        say("pid", libc::getpid().to_string());
        say("ppid", libc::getppid().to_string());
        say("uid", format!("{} {}", libc::getuid(), libc::geteuid()));
        say("gid", format!("{} {}", libc::getgid(), libc::getegid()));
        say(
            "nnp",
            libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0).to_string(),
        );
        let mut bounding = 0;
        for cap in 0..64 {
            if libc::prctl(libc::PR_CAPBSET_READ, cap, 0, 0, 0) == 1 {
                bounding += 1;
            }
        }
        say("bounding", bounding.to_string());
        let header = [0x2008_0522u32, 0];
        let mut data = [0u32; 6];
        libc::syscall(libc::SYS_capget, header.as_ptr(), data.as_mut_ptr());
        say("caps", format!("{:?}", data));
        // Every process it could signal besides itself (its own threads are its own).
        let me = libc::getpid();
        let visible: Vec<i32> = (1..4096)
            .filter(|p| {
                *p != me
                    && libc::kill(*p, 0) == 0
                    && libc::syscall(libc::SYS_tgkill, me, *p, 0) != 0
            })
            .collect();
        say("visible", format!("{visible:?}"));
        let open = |path: &str| {
            let c = std::ffi::CString::new(path).unwrap();
            let fd = libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            let r = errno_of(i64::from(fd));
            if fd >= 0 {
                libc::close(fd);
            }
            r
        };
        for path in [
            fact("neighbour").as_str(),
            "/etc/passwd",
            "/proc/self/status",
            "/sys/kernel",
            "/dev/null",
            facts_path.as_str(),
        ] {
            say("open", format!("{path} {}", open(path)));
        }
        let write = |path: &str| {
            let c = std::ffi::CString::new(path).unwrap();
            let fd = libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
                0o600,
            );
            let r = errno_of(i64::from(fd));
            if fd >= 0 {
                libc::close(fd);
            }
            r
        };
        say("write-ro", write(&facts_path));
        say("write-root", write("/new"));
        say("write-rw", write(&format!("{}/made-inside", fact("rw"))));
        let mut names = Vec::new();
        let index = libc::if_nameindex();
        if !index.is_null() {
            let mut at = index;
            while (*at).if_index != 0 {
                names.push(
                    std::ffi::CStr::from_ptr((*at).if_name)
                        .to_string_lossy()
                        .into_owned(),
                );
                at = at.add(1);
            }
            libc::if_freenameindex(index);
        }
        say("interfaces", names.join(","));
        let inet = |addr: [u8; 4], port: u16| {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return format!("socket-{}", errno_of(-1));
            }
            let sa = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(addr),
                },
                sin_zero: [0; 8],
            };
            let rc = libc::connect(
                fd,
                (&sa as *const libc::sockaddr_in).cast(),
                std::mem::size_of_val(&sa) as u32,
            );
            let r = errno_of(i64::from(rc));
            libc::close(fd);
            r
        };
        let port: u16 = fact("port").parse().unwrap_or(0);
        say("inet-loopback", inet([127, 0, 0, 1], port));
        say("inet-outside", inet([192, 0, 2, 1], 80));
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        let mut sa: libc::sockaddr_un = std::mem::zeroed();
        sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let name = fact("abstract");
        for (i, b) in name.bytes().enumerate() {
            sa.sun_path[i + 1] = b as libc::c_char;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + name.len()) as u32;
        let rc = libc::connect(fd, (&sa as *const libc::sockaddr_un).cast(), len);
        say("abstract", errno_of(i64::from(rc)));
        libc::close(fd);
        let key: i32 = fact("shm").parse().unwrap_or(0);
        say("shm", errno_of(i64::from(libc::shmget(key, 0, 0))));
        let target = std::ffi::CString::new(fact("rw")).unwrap();
        let tmpfs = std::ffi::CString::new("tmpfs").unwrap();
        let rc = libc::mount(
            tmpfs.as_ptr(),
            target.as_ptr(),
            tmpfs.as_ptr(),
            0,
            std::ptr::null(),
        );
        say("mount", errno_of(i64::from(rc)));
        say("setuid0", errno_of(i64::from(libc::setuid(0))));
        // A nested user namespace (the one namespace an unprivileged process may make), by
        // clone and by clone3, and unshare of a mount namespace.
        let pid = libc::syscall(
            libc::SYS_clone,
            libc::CLONE_NEWUSER | libc::SIGCHLD,
            0,
            0,
            0,
            0,
        );
        if pid == 0 {
            libc::_exit(0);
        }
        say("userns", errno_of(pid));
        let mut args = [0u64; 11];
        args[0] = libc::CLONE_NEWUSER as u64;
        args[4] = libc::SIGCHLD as u64;
        let pid = libc::syscall(
            libc::SYS_clone3,
            args.as_mut_ptr(),
            std::mem::size_of_val(&args),
        );
        if pid == 0 {
            libc::_exit(0);
        }
        say("clone3", errno_of(pid));
        say(
            "unshare",
            errno_of(i64::from(libc::unshare(libc::CLONE_NEWNS))),
        );
    }
    let rw = PathBuf::from(fact("rw"));
    let _ = File::create(rw.join("ready"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !rw.join("go").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn status_field(pid: i32, name: &str) -> String {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| {
            l.strip_prefix(&format!("{name}:"))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_else(|| panic!("no {name} in /proc/{pid}/status"))
}

fn namespace(pid: &str, kind: &str) -> PathBuf {
    fs::read_link(format!("/proc/{pid}/ns/{kind}")).unwrap()
}

#[test]
fn jail_shows_only_its_entries_and_no_node() {
    let scratch = Scratch::new("probe");
    let run = scratch.0.join("run");
    let neighbour = scratch.0.join("other-run");
    for dir in [
        &run,
        &neighbour,
        &run.join("rw"),
        &run.join("root"),
        &run.join("ro"),
    ] {
        fs::DirBuilder::new().mode(0o700).create(dir).unwrap();
    }
    fs::write(neighbour.join("secret"), b"another run's bytes").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let abstract_name = format!("moruna-jail-test-{}", std::process::id());
    let abstract_listener = {
        use std::os::linux::net::SocketAddrExt;
        let addr =
            std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
        std::os::unix::net::UnixListener::bind_addr(&addr).unwrap()
    };
    let shm_key = 0x4d4a_0000 | (std::process::id() as i32 & 0xffff);
    // SAFETY: a private SysV segment, removed below.
    let shm = unsafe { libc::shmget(shm_key, 4096, libc::IPC_CREAT | 0o600) };
    assert!(shm >= 0, "host shm segment");
    let facts = run.join("ro").join("facts");
    let rw = run.join("rw");
    fs::write(
        &facts,
        format!(
            "neighbour {}\nport {port}\nabstract {abstract_name}\nshm {shm_key}\nrw {}\n",
            neighbour.join("secret").display(),
            rw.display()
        ),
    )
    .unwrap();
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let mut entries = program_entries(&exe).unwrap();
    entries.push(Entry::File {
        path: facts.clone(),
        access: Access::Read,
        exec: false,
    });
    entries.push(Entry::Dir {
        path: rw.clone(),
        access: Access::Write,
    });
    let entries = settle(entries).unwrap();
    let mut jailed = spawn(Spec {
        program: &exe,
        args: [
            "--exact",
            "jail::tests::probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(Into::into)
        .to_vec(),
        entries: entries.clone(),
        root: &run.join("root"),
        stdout: File::create(scratch.0.join("out")).unwrap(),
        stderr: File::create(scratch.0.join("err")).unwrap(),
        cgroup: None,
    })
    .unwrap();
    let pid = jailed.pid();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !rw.join("ready").exists() {
        assert!(
            jailed.try_wait().unwrap().is_none(),
            "probe ended early: {}",
            fs::read_to_string(scratch.0.join("err")).unwrap()
        );
        assert!(Instant::now() < deadline, "probe never ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    // From outside: identity, capabilities, namespaces.
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let uids = status_field(pid, "Uid");
    let gids = status_field(pid, "Gid");
    let (u, g) = (uid.to_string(), gid.to_string());
    assert_eq!(uids.split_whitespace().collect::<Vec<_>>(), [u.as_str(); 4]);
    assert_eq!(gids.split_whitespace().collect::<Vec<_>>(), [g.as_str(); 4]);
    for cap in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert_eq!(status_field(pid, cap), "0000000000000000", "{cap}");
    }
    assert_eq!(status_field(pid, "NoNewPrivs"), "1");
    assert_eq!(status_field(pid, "Seccomp"), "2");
    assert_eq!(
        status_field(pid, "NSpid").split_whitespace().nth(1),
        Some("1")
    );
    assert_eq!(
        fs::read_to_string(format!("/proc/{pid}/uid_map"))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        [JAIL_ID.to_string(), uid.to_string(), "1".into()]
    );
    for kind in ["user", "mnt", "pid", "net", "ipc", "uts", "cgroup"] {
        assert_ne!(
            namespace("self", kind),
            namespace(&pid.to_string(), kind),
            "{kind} namespace shared"
        );
    }
    let out_so_far = fs::read_to_string(scratch.0.join("out")).unwrap();
    println!(
        "identity: Uid {uids}; Gid {gids}; Groups {}",
        status_field(pid, "Groups")
    );
    File::create(rw.join("go")).unwrap();
    let status = jailed.wait().unwrap();
    assert!(
        status.success(),
        "probe failed: {status}\n{out_so_far}\n{}",
        fs::read_to_string(scratch.0.join("err")).unwrap()
    );
    // SAFETY: removes the segment made above.
    unsafe { libc::shmctl(shm, libc::IPC_RMID, std::ptr::null_mut()) };
    drop((listener, abstract_listener));

    let out = fs::read_to_string(scratch.0.join("out")).unwrap();
    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for line in out.lines().filter_map(|l| l.strip_prefix("PROBE ")) {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        seen.entry(key).or_default().push(value);
    }
    println!("{out}");
    let one = |key: &str| {
        seen.get(key)
            .and_then(|v| v.first().copied())
            .unwrap_or_default()
    };
    // PID: alone, PID 1, its parent outside.
    assert_eq!(one("pid"), "1");
    assert_eq!(one("ppid"), "0");
    assert_eq!(one("visible"), "[]");
    // Identity and capabilities from inside.
    assert_eq!(one("uid"), format!("{JAIL_ID} {JAIL_ID}"));
    assert_eq!(one("gid"), format!("{JAIL_ID} {JAIL_ID}"));
    assert_eq!(one("nnp"), "1");
    assert_eq!(one("bounding"), "0");
    assert_eq!(one("caps"), "[0, 0, 0, 0, 0, 0]");
    assert_eq!(one("setuid0"), format!("errno{}", libc::EINVAL));
    assert_eq!(one("mount"), format!("errno{}", libc::EPERM));
    assert_eq!(one("userns"), format!("errno{}", libc::EPERM));
    assert_eq!(one("clone3"), format!("errno{}", libc::ENOSYS));
    assert_eq!(one("unshare"), format!("errno{}", libc::EPERM));
    // File system: exactly the entries, their parent directories, and what is in a bound
    // directory; nothing of the neighbour or the node.
    let allowed = |path: &Path| {
        entries.iter().any(|e| {
            e.path() == path
                || e.path().starts_with(path)
                || matches!(e, Entry::Dir { path: d, .. } if path.starts_with(d))
        })
    };
    let tree = seen.get("tree").cloned().unwrap_or_default();
    for path in &tree {
        assert!(allowed(Path::new(path)), "the jail shows {path}");
    }
    for entry in &entries {
        assert!(
            tree.contains(&entry.path().to_str().unwrap()),
            "{} missing",
            entry.path().display()
        );
    }
    let opens: BTreeMap<&str, &str> = seen["open"]
        .iter()
        .filter_map(|l| l.rsplit_once(' '))
        .collect();
    let enoent = format!("errno{}", libc::ENOENT);
    for path in [
        neighbour.join("secret").to_str().unwrap(),
        "/etc/passwd",
        "/proc/self/status",
        "/sys/kernel",
        "/dev/null",
    ] {
        assert_eq!(opens[path], enoent, "{path}");
    }
    assert_eq!(opens[facts.to_str().unwrap()], "ok");
    assert_eq!(one("write-ro"), format!("errno{}", libc::EROFS));
    assert_eq!(one("write-root"), format!("errno{}", libc::EROFS));
    assert_eq!(one("write-rw"), "ok");
    assert!(rw.join("made-inside").exists());
    // Network: a new namespace with only a loopback that is down; no host socket reachable,
    // not even an abstract Unix socket; no host IPC object.
    assert_eq!(one("interfaces"), "lo");
    assert_eq!(one("inet-loopback"), format!("errno{}", libc::ENETUNREACH));
    assert_eq!(one("inet-outside"), format!("errno{}", libc::ENETUNREACH));
    assert_eq!(one("abstract"), format!("errno{}", libc::ECONNREFUSED));
    assert_eq!(one("shm"), enoent);
}

#[test]
fn a_failed_jail_step_is_named_with_its_cause() {
    let scratch = Scratch::new("fail");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(scratch.0.join("root"))
        .unwrap();
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    // The program is not among the entries: the jail cannot execute it.
    let error = spawn(Spec {
        program: &exe,
        args: Vec::new(),
        entries: Vec::new(),
        root: &scratch.0.join("root"),
        stdout: File::create(scratch.0.join("out")).unwrap(),
        stderr: File::create(scratch.0.join("err")).unwrap(),
        cgroup: None,
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("execve") && error.contains("No such file"),
        "{error}"
    );
    // An entry that does not exist on the host is refused before any process is made.
    let error = spawn(Spec {
        program: &exe,
        args: Vec::new(),
        entries: vec![Entry::File {
            path: scratch.0.join("absent"),
            access: Access::Read,
            exec: false,
        }],
        root: &scratch.0.join("root"),
        stdout: File::create(scratch.0.join("out")).unwrap(),
        stderr: File::create(scratch.0.join("err")).unwrap(),
        cgroup: None,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("absent"), "{error}");
}

#[test]
fn entries_are_settled_and_programs_resolved_like_execvp() {
    assert_eq!(
        normalise(Path::new("/usr/bin/../share/./qemu")).unwrap(),
        Path::new("/usr/share/qemu")
    );
    assert!(normalise(Path::new("relative")).is_err());
    let dir = Entry::Dir {
        path: "/run/x".into(),
        access: Access::Write,
    };
    let inner = Entry::File {
        path: "/run/x/y".into(),
        access: Access::Read,
        exec: false,
    };
    assert!(
        settle(vec![dir.clone(), inner])
            .unwrap_err()
            .to_string()
            .contains("inside the bound directory")
    );
    assert_eq!(settle(vec![dir.clone(), dir.clone()]).unwrap(), vec![dir]);
    let twice = |access| Entry::File {
        path: "/a".into(),
        access,
        exec: false,
    };
    assert!(settle(vec![twice(Access::Read), twice(Access::Write)]).is_err());
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let entries = program_entries(&exe).unwrap();
    assert_eq!(entries[0].path(), exe);
    assert!(
        interpreter(&exe).unwrap().is_some(),
        "the test binary is dynamically linked"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.path().to_string_lossy().contains("libc.so"))
    );
    assert!(resolve_program(Path::new("moruna-no-such-program")).is_err());
    assert_eq!(resolve_program(&exe).unwrap(), exe);
}

#[test]
fn a_cgroup_that_is_not_delegated_is_refused_by_name() {
    let scratch = Scratch::new("cgroup");
    let size = Size::for_vm(1 << 30, 2).unwrap();
    assert_eq!(
        size.memory_bytes,
        (1 << 30) + cgroup::MONITOR_OVERHEAD_BYTES
    );
    assert_eq!(size.tasks, cgroup::TASKS_BESIDE_VCPUS + 2);
    let error = Leaf::create(&scratch.0, size).err().unwrap().to_string();
    assert!(
        error.contains("not a delegated cgroup v2 directory"),
        "{error}"
    );
    fs::write(scratch.0.join("cgroup.subtree_control"), "cpu io\n").unwrap();
    let error = Leaf::create(&scratch.0, size).err().unwrap().to_string();
    assert!(error.contains("does not enable memory, pids"), "{error}");
    assert_eq!(Cgroup::parse("none").unwrap(), Cgroup::Undelegated);
    assert!(Cgroup::parse("relative/dir").is_err());
}

/// The leaf a run is born in, against a real delegated cgroup v2 directory named by
/// `MORUNA_JAIL_CGROUP` (cpu, memory and pids enabled in its `cgroup.subtree_control`, and
/// this process inside the delegated subtree).
#[test]
#[ignore = "requires a delegated cgroup v2 directory in MORUNA_JAIL_CGROUP"]
fn a_jailed_process_is_born_in_its_sized_leaf_and_the_leaf_is_removed() {
    let parent = PathBuf::from(std::env::var_os("MORUNA_JAIL_CGROUP").expect("MORUNA_JAIL_CGROUP"));
    let scratch = Scratch::new("leaf");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(scratch.0.join("root"))
        .unwrap();
    let mut leaf = Leaf::create(&parent, Size::for_vm(512 << 20, 2).unwrap()).unwrap();
    let path = leaf.path().to_path_buf();
    let read = |f: &str| fs::read_to_string(path.join(f)).unwrap().trim().to_string();
    assert_eq!(
        read("memory.max"),
        ((512u64 << 20) + cgroup::MONITOR_OVERHEAD_BYTES).to_string()
    );
    assert_eq!(read("memory.swap.max"), "0");
    assert_eq!(read("cpu.max"), "200000 100000");
    assert_eq!(
        read("pids.max"),
        (cgroup::TASKS_BESIDE_VCPUS + 2).to_string()
    );
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let mut jailed = spawn(Spec {
        program: &exe,
        args: ["--list"].map(Into::into).to_vec(),
        entries: settle(program_entries(&exe).unwrap()).unwrap(),
        root: &scratch.0.join("root"),
        stdout: File::create(scratch.0.join("out")).unwrap(),
        stderr: File::create(scratch.0.join("err")).unwrap(),
        cgroup: Some(leaf.fd()),
    })
    .unwrap();
    let pid = jailed.pid();
    let procs = read("cgroup.procs");
    let membership = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    let status = jailed.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(
        procs.lines().any(|l| l == pid.to_string())
            || membership
                .trim()
                .ends_with(path.file_name().unwrap().to_str().unwrap()),
        "{pid} not born in {}: procs {procs:?}, /proc cgroup {membership:?}",
        path.display()
    );
    leaf.remove().unwrap();
    assert!(!path.exists());
}

/// Run `action` in a forked child under the vsock back end's filter; its wait status.
fn filtered(action: fn() -> i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    let filter = seccomp::program();
    // SAFETY: the child makes only system calls (prctl, the action's, _exit) and returns
    // nowhere; the filter was built before the fork.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr().cast_mut(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program as *const libc::sock_fprog,
                ) != 0
            {
                libc::_exit(99);
            }
            libc::_exit(action());
        }
        let mut raw = 0;
        libc::waitpid(pid, &mut raw, 0);
        std::process::ExitStatus::from_raw(raw)
    }
}

#[test]
fn the_vsock_back_end_filter_allows_its_calls_and_kills_any_other() {
    let killed = |status: std::process::ExitStatus| {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGSYS)
    };
    // SAFETY (all actions): raw system calls on valid arguments in a forked child.
    let unix_socket =
        || unsafe { i32::from(libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) < 0) };
    assert_eq!(filtered(unix_socket).code(), Some(0));
    let nonblocking = || unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let mut on: libc::c_int = 1;
        i32::from(libc::ioctl(fd, libc::FIONBIO, &mut on) != 0)
    };
    assert_eq!(filtered(nonblocking).code(), Some(0));
    let data_map = || unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        i32::from(p == libc::MAP_FAILED)
    };
    assert_eq!(filtered(data_map).code(), Some(0));
    let inet = || unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(killed(filtered(inet)), "an IP socket");
    let open = || unsafe { libc::open(c"/".as_ptr(), libc::O_RDONLY) };
    assert!(killed(filtered(open)), "opening a file");
    let exec_map = || unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        0
    };
    assert!(killed(filtered(exec_map)), "executable memory");
    let fork = || unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) as i32 };
    assert!(killed(filtered(fork)), "a new process");
    let exec = || unsafe {
        let argv = [std::ptr::null()];
        libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), argv.as_ptr())
    };
    assert!(killed(filtered(exec)), "a program");
    let tty = || unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::ioctl(0, libc::TCGETS, &mut t)
    };
    assert!(killed(filtered(tty)), "another ioctl");
    let ptrace = || unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) as i32 };
    assert!(killed(filtered(ptrace)), "ptrace");
}
