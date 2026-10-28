# Moruna SDD 19: An OCI image as a kind (`moruna-image`, `moruna-sandbox::image`, `moruna-runtime::kinds::image`)

**Document type:** software design document, component 19
**Status:** DRAFT · 2026-10-28 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `BOARD.md` EK, item KA1; `architecture/optimized-runtime.md` section 7 (images)
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.7 (`Kernel`, amended here with `finish`); **Cited:** 10 f.6 (the sink drive), 17 (the sandbox: the cgroup, the stage ceiling, the `.pex` child), 18 (the kinds registry)
**Component location:** `crates/moruna-image/` (the image port, the layout adapter, the unpacking, the cache), `crates/moruna-sandbox/src/image/` and `src/seccomp.rs` (the confined process, the filter, the stream), `crates/moruna-runtime/src/kinds/image.rs` (the kind), `crates/moruna-image/tests/` (the Linux tests and the program they run)
**Consumes:** the kinds registry (18), the sandbox's cgroup and output plumbing (17), the hardened Arrow IPC reader (contracts e.8). **Consumed by:** the job document (`"kind": "image"`), the facade's binding of packaged stages (12), the PM's board items KA2 and KA3.

**Decisions worth your eye:** (1) there is no container runtime: an image is a filesystem and a command, unpacked by Moruna's own code and run by the sandbox that already confines a `.pex` child; (2) the contract with the process is Arrow IPC over standard input and output, one stream each way, one batch back per batch in, so a program in any language needs only an Arrow library; (3) the process is in a user namespace (so Moruna needs no privilege Moruna does not already have), a mount, pid, network, ipc, uts and cgroup namespace, with no capabilities, Landlock and a seccomp allow-list that kills it on any call off the list; (4) the unpacked root is a cache entry made whole and renamed into place, so many stages and many runs share one unpacking and never see half of it; (5) `Kernel` gains `finish`, because an image's exit status is only knowable after its input ends and nothing in the contract said when that was.

---

## a. Purpose and boundary

Run any OCI image's command as a stage of a run, in any language, with no more access to the machine than a `.pex` child (17) has: no network, no disk but its own root and a scratch `/tmp`, no capability, a memory ceiling Moruna set. This component owns: reading and verifying an image from a source; unpacking its layers into a root; the cache that holds the roots; the process that is started in that root and everything that confines it; the stream contract with the process; how a process that does not do what the contract says ends the run; the `image` kind.

It refuses to know: how an image is built, a registry (a second adapter of the source port is a later item, o), a Python or JVM runtime inside an image (the process is a command), and the platform the image was built for beyond the CPU architecture it must match, which the kernel refuses by itself when it cannot execute the binary.

## b. Vocabulary

**Image.** A manifest, a config and layers, content addressed. **Layout.** An OCI image layout directory: `oci-layout`, `index.json`, `blobs/sha256/<hex>`. **Source.** The port an image is read through. **Root.** The unpacked layers, a directory. **Cache.** The directory roots are kept in. **Entry.** One image's directory in the cache. **The process.** The image's command, running in the sandbox, one per stage. **The middle.** The process Moruna forks, which makes the namespaces and waits for the process (f.5).

## c. Invariants

**IM-I1. Every byte is checked against its name as it is read.** The manifest, the config and every layer are hashed (sha256) while read, and compared to the digest they are named by and to the size their descriptor says; a mismatch is an error naming the digest, whatever else is wrong with the bytes (a gzip that is not gzip, named by another digest, is a digest error).

**IM-I2. An unpacking writes only inside the root.** No entry of a layer is written through a path that is absolute, that holds `..`, or that passes through a symbolic link whose target is absolute or leaves the root; a hard link names something inside the root that exists. Special mode bits are never set. Devices and fifos are not made.

**IM-I3. A cache entry is whole or absent.** An entry is made in a directory of its own, `complete` is written last, and the directory is renamed into place; two unpackings of one image end with one entry and both succeed. An entry that is there without `complete` is refused, never used.

**IM-I4. The process runs as the image's user, in a namespace that makes it the owner of what it can touch.** Root (or no user) is mapped to the user that runs Moruna; a numeric user (and group) is mapped likewise to that user; a user that is a name is refused when the kernel is built, naming it.

**IM-I5. The process has no network and no platform credential.** It has a network namespace with nothing in it and no system call that makes a socket; its environment is the image's and the settings' and nothing of Moruna's; every descriptor but its three is closed before it runs.

**IM-I6. A process that does not do what the contract says ends the run, naming how.** Output that is not Arrow, more or fewer batches than inputs, an exit status that is not 0, a signal: each is a kernel error with its cause, and a nonzero exit or a signal quotes the last lines of the process's standard error.

**IM-I7. The process's memory is the stage's.** Its cgroup's `memory.max` is the stage ceiling (17 f.7) and its `memory.current` is what the run's sampler adds to the process's own (17 SB-I9); a process the kernel kills for it is reported as that.

**IM-I8. A process goes when its owner does.** Dropping a stage's state kills the process, whatever the path; the process also dies when Moruna's wait for it ends, and the namespace's first process dying ends every other.

## d. Interfaces

### d.1 The port and the unpacking (`moruna-image`)

```rust
pub trait ImageSource: Send + Sync {
    fn manifest(&self, reference: &str) -> Result<Manifest>;   // verified (IM-I1)
    fn blob(&self, digest: &Digest) -> Result<Box<dyn Read>>;  // not trusted: the caller hashes it
}
pub struct OciLayout { /* a directory */ }  // open(root); `reference` is a digest or the annotation
                                            // org.opencontainers.image.ref.name of index.json
pub struct Image { pub digest: Digest, pub config: Config, pub rootfs: PathBuf, pub skipped: u64 }
pub struct Config { pub entrypoint: Vec<String>, pub cmd: Vec<String>, pub env: Vec<String>,
                    pub working_dir: String, pub user: String }
pub fn unpack(source: &dyn ImageSource, reference: &str, cache: &Path) -> Result<Image>;
pub const MOUNT_POINTS: [&str; 3] = ["proc", "dev", "tmp"];
```

`Config` holds the five fields of the image config's `config` object it names and nothing else; `skipped` counts the devices and fifos left out of the layers.

### d.2 The kind (`moruna-runtime::kinds::image`)

```json
{ "kind": "image", "layout": "/disk/images/scorer", "reference": "scorer:1",
  "command": ["/usr/bin/scorer"], "env": {"MODEL": "small"}, "cache": "/disk/cache/images" }
```

`layout` and `reference` are required. `command` replaces the image's entrypoint and cmd. `env` is added to the image's environment, a name replacing the image's. `cache` defaults to `images` under the run's output when that is a directory on this machine (`file://` or a plain path) and is otherwise required. Building the stage unpacks the image and makes the kernel; off Linux it is refused with the sandbox's `unavailable()` error; on Linux without `--sandbox-cgroup` (`PackageHost::sandbox_cgroup`, 17 e.6) it says that is what it needs.

### d.3 The kernel (`moruna-sandbox::ImageKernel`, Linux)

```rust
impl ImageKernel {
    pub fn new(name: &str, image: &Image, command: Option<Vec<String>>,
               env: &BTreeMap<String, String>, cgroup: PathBuf) -> Result<ImageKernel>;
    pub fn bind_run(&self, sampler: Arc<dyn Sampler>, ceiling: u64);   // as SandboxKernel
    pub fn outside(&self) -> Arc<dyn OutsideMemory>;
    pub fn limits_changed(&self);
}
impl Kernel for ImageKernel { /* f.6, f.7: stateful, one instance, apply, finish */ }
```

The runtime holds it as `Packaged::Image`, so the facade binds it to the run's sampler, adds its memory to every sample and moves its ceiling with the run's, as for a `.pex`. `Packaged::package()` is `None` for an image.

### d.4 Consumed

`tar` (entries are read; `default-features = false`), `flate2` and `zstd` (layer compression), added to the preamble 6.2 table with this document; `sha2`, `serde`, `serde_json`, `thiserror`, `getrandom`, `tracing`; `landlock`, `seccompiler` and `libc` as in 17.

## e. Data model, formats and state machines

### e.1 The layout and the documents

`oci-layout` is `{"imageLayoutVersion": "1.0.0"}`. `index.json` lists manifests; one is chosen by digest or by the annotation `org.opencontainers.image.ref.name` (a name held by two is refused, a name held by none is refused listing the names the layout has). A manifest is an image manifest (`application/vnd.oci.image.manifest.v1+json` or Docker's `application/vnd.docker.distribution.manifest.v2+json`; an index of images is refused naming its media type) with a config descriptor and layer descriptors, each with a media type, a `sha256:` digest and a size. A layer's media type is one of `application/vnd.oci.image.layer.v1.tar`, `...tar+gzip`, `...tar+zstd` or `application/vnd.docker.image.rootfs.diff.tar.gzip`; any other is refused naming it before any blob is read.

### e.2 The cache entry

```
<cache>/<manifest digest hex>/
    config.json       the image config document, as the blob held it
    rootfs/           the root
    complete          the number of devices and fifos left out, in decimal
<cache>/.tmp-<16 hex digits>/        an unpacking in progress
```

### e.3 The root inside the sandbox

The sandbox's mount namespace, after `pivot_root`, holds the cache entry's `rootfs/` bind-mounted read only (`nosuid`, `nodev`) and over it: a `tmpfs` at `/tmp` of `TMP_BYTES` (64 MiB, mode 1777, `nosuid`, `nodev`); a new procfs at `/proc` (the pid namespace limits it to the process's own processes; `nosuid`, `nodev`, `noexec`); a `tmpfs` at `/dev` (64 KiB, `nosuid`, `noexec`) holding `null`, `zero` and `urandom` bind-mounted from the host (`confine::DEVICES`). There is nothing else: no host path is reachable.

### e.4 The system-call filter

Two filters, installed in this order. The first answers `clone3` with `ENOSYS` and allows every other call, so the C library falls back to `clone`. The second allows the calls below and kills the process (`SECCOMP_RET_KILL_PROCESS`) on every other, so the parent sees `SIGSYS`. Three calls are conditional: `clone` is allowed only when its flags make no namespace (`CLONE_NEWNS`, `NEWCGROUP`, `NEWUTS`, `NEWIPC`, `NEWUSER`, `NEWPID`, `NEWNET` all clear); `socketpair` only with the family `AF_UNIX`; `clone3` as above. On every architecture:

```text
read write readv writev pread64 pwrite64 preadv pwritev preadv2 pwritev2 lseek close
close_range openat openat2 newfstatat fstat statx statfs fstatfs getdents64 readlinkat
faccessat faccessat2 getcwd chdir fchdir fcntl ioctl dup dup3 pipe2 mkdirat unlinkat renameat
renameat2 linkat symlinkat ftruncate truncate fallocate fadvise64 fsync fdatasync
sync_file_range syncfs sync flock fchmod fchmodat fchown fchownat utimensat umask
copy_file_range sendfile splice tee vmsplice readahead inotify_init1 inotify_add_watch
inotify_rm_watch getxattr lgetxattr fgetxattr listxattr llistxattr flistxattr setxattr
lsetxattr fsetxattr removexattr lremovexattr fremovexattr mmap munmap mremap mprotect madvise
mincore brk mlock mlock2 munlock mlockall munlockall msync membarrier memfd_create
get_mempolicy set_mempolicy mbind pkey_alloc pkey_free pkey_mprotect shmget shmat shmctl shmdt
semget semop semctl semtimedop msgget msgsnd msgrcv msgctl futex futex_waitv set_robust_list
get_robust_list rseq set_tid_address clone3 exit exit_group wait4 waitid execve execveat
restart_syscall prctl capget capset getpid gettid getppid getpgid setpgid getsid setsid getuid
geteuid getgid getegid getresuid getresgid getgroups setuid setgid setreuid setregid setresuid
setresgid setgroups setfsuid setfsgid getpriority setpriority sched_yield sched_getaffinity
sched_setaffinity sched_getparam sched_setparam sched_getscheduler sched_setscheduler
sched_get_priority_max sched_get_priority_min sched_rr_get_interval sched_getattr sched_setattr
getcpu prlimit64 getrusage sysinfo uname times personality ioprio_get ioprio_set rt_sigaction
rt_sigprocmask rt_sigreturn rt_sigtimedwait rt_sigsuspend rt_sigpending rt_sigqueueinfo
rt_tgsigqueueinfo sigaltstack kill tkill tgkill signalfd4 setitimer getitimer pidfd_send_signal
pidfd_open clock_gettime clock_getres clock_nanosleep nanosleep gettimeofday timerfd_create
timerfd_settime timerfd_gettime timer_create timer_settime timer_gettime timer_getoverrun
timer_delete ppoll pselect6 epoll_create1 epoll_ctl epoll_pwait epoll_pwait2 eventfd2
socketpair sendmsg recvmsg sendto recvfrom sendmmsg recvmmsg shutdown getsockopt setsockopt
getsockname getpeername getrandom
```

On x86_64 only, the older forms its C library still uses:

```text
open stat lstat access readlink poll select pipe dup2 mkdir rmdir unlink rename link symlink
chmod chown lchown creat getdents utime utimes futimesat arch_prctl epoll_wait epoll_create
eventfd signalfd inotify_init time getrlimit setrlimit getpgrp fork vfork pause alarm
```

Not on the list, and so fatal: `socket`, `connect`, `bind`, `listen`, `accept`, `accept4`, `ptrace`, `mount`, `umount2`, `pivot_root`, `chroot`, `unshare`, `setns`, `bpf`, `perf_event_open`, `kexec_load`, `kexec_file_load`, `init_module`, `finit_module`, `delete_module`, `reboot`, `swapon`, `swapoff`, `add_key`, `request_key`, `keyctl`, `userfaultfd`, `process_vm_readv`, `process_vm_writev`, `open_by_handle_at`, `name_to_handle_at`, `io_uring_setup`, `io_uring_enter`, `io_uring_register`, `seccomp`, `landlock_create_ruleset`, `landlock_add_rule`, `landlock_restrict_self`, the new mount API (`fsopen`, `fsmount`, `fspick`, `move_mount`, `open_tree`, `mount_setattr`) and every call not named above. `execve` and `execveat` are allowed: the next call of the process is its exec.

### e.5 The declaration

`image`, runnable kind: inputs batches, outputs batches; needs no network, no egress, no GPU, no model port, no contracts; deterministic no; preserves columns no; isolation confined. The summary is "An OCI image's command in a sandboxed process, Arrow IPC over its standard input and output", with one example.

## f. Algorithms and policies

**f.1 Unpacking.** `unpack`: read the manifest (verified by `Manifest::parse`, IM-I1); refuse a layer media type it does not read; make the cache; return the entry when it is whole (f.4) without reading the config or a layer; otherwise make `.tmp-<random>/`, read the config (checked against its descriptor, at most 16 MiB), write `config.json`, apply every layer in manifest order (f.2), make the mount points (f.3), write `complete`, and rename the directory to the entry. A failure removes the temporary directory.

**f.2 A layer.** The blob is read through a hasher, decompressed (gzip with every member, zstd with every frame, or not) and read as a tar. For each entry, in order: the path is split into components, `.` and doubled slashes dropped; an absolute path or a `..` is refused naming the layer and the entry (an entry `.` is the root and makes nothing). The parent directory is resolved under the root one component at a time without leaving it: a missing directory is made (0755); a symbolic link is followed, its relative target resolved lexically inside the root, up to forty links; a link with an absolute target, or whose `..` climbs out of the root, or a component that is not a directory, is refused. Then by type: a directory (merged, its mode `| 0700`, replacing what is not a directory); a regular file (what is there is removed, the file written with the mode `& 0777`); a symbolic link (written as it is: its target is resolved only in the sandbox's root); a hard link (its target normalised as a path and required to exist already; a link to a symbolic link is that link made again; a link to a directory is refused); a character device, a block device or a fifo (skipped and counted); any other type refused. `.wh.<name>` removes `<name>` and everything under it from what the layers below left, unless this layer made it; `.wh..wh..opq` removes from its directory everything this layer did not make; names that begin `.wh..wh.` are the layer's own bookkeeping and are ignored. Ownership, times and extended attributes are not applied: everything is owned by the user that unpacks. After the tar ends, the rest of the blob is read, and its digest and size are checked (IM-I1); when anything failed before that, the blob is read to the end anyway so that a digest error is the one reported.

**f.3 Mount points.** `proc`, `dev` and `tmp` are made at the root of the root when missing; one that exists and is not a directory is refused naming it. (The root is read only in the sandbox, so the mount points have to exist in it.)

**f.4 The cache and races.** The entry is `<cache>/<manifest digest hex>`. `unpack` of an entry that holds `complete` reads its `config.json` and returns, reading no layer. A second unpacking running at the same time works in its own temporary directory; whichever renames first wins; the other finds the target present and whole, removes its own directory and returns the entry. A target that exists without `complete` is an error naming it, since only a finished unpacking is ever renamed there. Stale `.tmp-*` directories of a process that died are left for the operator to remove.

**f.5 Starting the process.** In the parent, before the fork: the three pipes (close-on-exec), the cgroup (17 SB-I6: `<cgroup>/image-<pid>-<time>-<n>`, `memory.max` the stage ceiling, swap off, `cpu.max` the run's quota), the environment and arguments as C strings, the paths the command is tried at (itself when it holds a `/`, else one per `PATH` entry of the process's environment, defaulting to `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`), and both seccomp programs, so what runs after the fork is system calls. Then `fork`, and in the middle child: descriptors 0, 1 and 2 are the pipes and every other descriptor is closed (`close_range(3, ~0, 0)`); core dumps off; `SIGPIPE` and `SIGXFSZ` back to default and the signal mask empty (Moruna ignores the first and its threads block signals, and both survive an exec); the child joins the stage's cgroup (`confine::join_cgroup`); `unshare` of the user, mount, pid, network, ipc, uts and cgroup namespaces at once; `setgroups` is denied and the image's user and group (root, or the numeric ones) are mapped to Moruna's own user and group, one id each, which is all an unprivileged process may map. A pid namespace takes effect for the children of the process that makes it, so the middle forks the second child, which is process 1 of the new namespace, and the middle waits for it and ends as it ended (its status, or by the signal that killed it, so Moruna sees one process and one status). The second child, in order: asks to be killed when the middle goes; makes `/` and everything under it private; bind-mounts the unpacked root onto itself and remounts it read only, `nosuid` and `nodev`, keeping the flags its mount already has (a user namespace may not clear them); mounts the `tmpfs` at `/tmp`, procfs at `/proc` and the `tmpfs` with the three devices at `/dev` (e.3); `pivot_root(".", ".")` from inside the new root and unmounts the old one; drops every capability and sets `no_new_privs` (`confine::drop_privileges`); changes to the working directory (the image's, or `/`; one the image lacks is an error naming it); applies Landlock, which handles every filesystem access of ABI 1 and allows read and execute under `/`, read and write on the three devices, and everything under `/tmp`, refusing the process when the kernel has no Landlock; installs the filters (e.4); and executes the command with exactly the environment of f.6's first sentence. A step that fails writes `moruna-image: <step>: <cause>` to the process's standard error and ends with status 125; a command not found ends with 127, one found and not executable with 126.

**f.6 The contract with the process.** The environment is the image's `Env` and the settings' `env` added to it, a name replacing an earlier one, and nothing from Moruna's. The process reads one Arrow IPC stream from its standard input and writes one to its standard output: Moruna writes a schema message, then one record batch for each `apply`, then the end-of-stream marker at `finish`; the process writes a schema message, then exactly one record batch for each record batch it read, in order. When its input ends it writes the end-of-stream marker and exits with status 0. A run that applied no batch ends the input without a byte; the process writes nothing and exits with status 0. Its standard error is its stage's output (MH 4.3 `log`), forwarded as the `.pex` child's is (17), and its last twenty lines are kept for f.7. The stage is stateful with one instance (one process), and its output schema is its input's for planning; the sink adopts the first reply's. `apply` writes a batch on a thread of its own while it reads the reply on the calling one, so a process that begins to answer before it has read the whole of a large batch does not wait for a reader that waits on the writer. The reply is read message by message from the pipe, each message and body no larger than the stage ceiling, through the hardened decoder (contracts e.8), so a length the process writes is never allocated before it is checked; the batch is landed into the arena (05 f.3).

**f.7 How it ends, and what is a failure.** `finish` (contracts d.7, 10 f.6): the input stream is ended, the output is read to its end (the marker, or the pipe closed at a message boundary), and the process is waited for up to 60 seconds. Every one of these is a `Kernel` error, with the image named:

- output that is not an Arrow IPC stream ("its output is not an Arrow IPC stream: <why>"), at the first message that is not one;
- fewer batches than inputs: the output ends, or the process ends, before a batch is answered ("its output ended after N batches, for M sent");
- more batches than inputs: found at `finish`, when what follows the last answer is a batch rather than the end;
- a nonzero exit: "exited with status N", with the last twenty lines of its standard error;
- death by a signal: "was killed by SIGNAL (n)"; for `SIGSYS`, "made a system call it is not allowed (the process was killed by SIGSYS)"; for `SIGKILL` when the cgroup recorded an `oom_kill`, "was killed for using more memory than its stage's limit";
- a process that does not exit within the 60 seconds: killed, and said so.

When a call fails because the pipe closed, the process is waited for (up to two seconds) so its exit is the cause; when the output is not Arrow and the process is alive, it is killed and the cause is the output. A process that never answers is waited on without a bound: there is no per-call timeout, as for a `.pex` (17), and the run's cancel does not reach a call in progress.

**f.8 The stage's memory.** At start and before every call the stage ceiling is `ceiling_in_force - (sample.anon_bytes - own)` from a fresh sample (17 f.7), written to `memory.max` when it moved, with the run's CPU quota. The cgroup's `memory.current` is the process's memory for the sampler (IM-I7). A resumed run starts a new process (`ResumePolicy::Reinit`): an image that keeps state between batches is not resumable.

## g. Concurrency within the component

`unpack` may run on any number of threads and processes at once (f.4). A stage's process is used by one worker at a time (a stateful stage with one instance, 10 f.4); `apply`'s writer thread is joined before `apply` returns. The stderr reader is a thread per process, ended when the pipe is.

## h. Behaviour

An image with no command (no entrypoint, no cmd and no `command` setting) is refused when the stage is built, as is a user that is a name, an `Env` entry without `=`, a working directory that is not absolute and a command or environment holding a NUL. A missing command ends the run on its first call with status 127 and the paths it was tried at. A layout that is not one, an image the layout does not name, a digest that does not match and a layer that is refused each fail the stage's build, at `kernels[i].layout` or `kernels[i].reference`, with the cause.

## i. Configuration

None beyond the settings, `--sandbox-cgroup` (17 e.6) and the constants `TMP_BYTES` (64 MiB), `EXIT_WAIT` (60 s) and the twenty lines of standard error.

## j. Observability

The run's notes say how many devices and fifos an image's layers held that its root does not. A process's standard error is its stage's output. A failure carries the cause of f.7.

## k. Tests

Run on macOS and Linux (`cargo test -p moruna-image --lib`, the kind's test in `moruna-runtime`):

**IM-T1 a_layout_resolves_its_references.** A digest and a name resolve to the manifest; a name the layout lacks lists the names it has; a layout of another version, one with no marker, two images of one name and an index with no names are refused. A manifest that is an index of images is refused; a digest that is not one is refused. IM-I1, e.1.
**IM-T2 layers_lay_over_one_another.** Four layers (tar, gzip, zstd, Docker gzip) in order: replaced files, merged directories, a file for a directory and the reverse, the mount points made, a parent the archive never made made, `./` and doubled-slash paths, a read-only directory the layer made. f.1, f.2, f.3.
**IM-T3 a_whiteout_removes_a_file.** A whiteout removes a lower file, not the same layer's file of that name; a whiteout of nothing and of a missing directory are nothing; the layer's own `.wh..wh.` names are ignored. f.2.
**IM-T4 a_whiteout_removes_a_directory.** A tree goes, links and unwritable directories included. f.2.
**IM-T5 an_opaque_directory_keeps_only_this_layers.** Entries before and after the marker survive; lower ones go; the directory stays; a missing directory is nothing. f.2.
**IM-T6 a_parent_component_is_refused.** Three paths with `..` and a hard link's target with one are refused naming the layer and the entry; the cache is left empty. IM-I2.
**IM-T7 an_absolute_path_is_refused.** A file, a directory, a symbolic link and a hard link's target. IM-I2.
**IM-T8 a_symlink_escape_is_refused.** `a -> /etc` then `a/passwd`; a link that climbs out; one that climbs out in steps; a chain; a loop; a file used as a directory; and, inside the root, links that are followed. IM-I2, f.2.
**IM-T9 special_mode_bits_are_dropped.** setuid, setgid and sticky on files and directories. IM-I2.
**IM-T10 a_digest_mismatch_is_refused.** A layer, a gzip layer that is not gzip, a size, a broken tar, the config and the manifest each fail naming the digest (or, for the broken tar, the tar). IM-I1.
**IM-T11 an_unknown_media_type_is_refused.** Refused naming it before a blob is read; an entry type no image holds is refused. e.1.
**IM-T12 a_second_unpack_reads_no_layer.** A source that counts `blob` calls sees none and no config the second time, nor when the image is named by digest; the count of skipped files is kept; an entry without `complete` or with a `complete` that is no count is refused. IM-I3, f.4.
**IM-T13 racing_unpacks_end_with_one_entry.** Two threads, eight rounds: one entry, no temporary directory, equal results. IM-I3.
**IM-T14 special_files_and_hard_links.** Devices and fifos skipped and counted; hard links share an inode; a link to a link is a link; a link to nothing or to a directory is refused. f.2.
**IM-T15 the_config_is_read.** The five fields, nulls and absences, a document that is not a config; a mount point that is a link is refused. e.1, f.3.
**IM-T16 the_image_kind_is_declared.** The declaration is one the registry accepts and is the one of e.5; the kind is in the standard registry; settings are refused at the entry before anything is unpacked; off Linux the stage is refused by the sandbox's error; on Linux without a cgroup it says what it needs. e.5, d.2.

Linux only (`cargo test -p moruna-image --test image_kind -- --ignored` with `MORUNA_SANDBOX_CGROUP`, run by CI as root), over `arrow_echo`, a program that reads an Arrow IPC stream on standard input and writes it back with a column `n` (the row's index) added, packed in a layout with the libraries `ldd` names:
**IM-T17 an_image_is_a_stage.** Parquet in, the image, Parquet out: every row arrives with `n`; the stage reports every row in and out; one root is in the cache. f.6.
**IM-T18 a_socket_kills_the_image.** `--socket`: the run ends with the sentence of f.7 and `SIGSYS`. e.4, IM-I5.
**IM-T19 the_root_is_read_only.** `--write-root`: `/tmp` was writable, `/etc` is not; the run names status 1 and quotes the standard error. e.3, f.5.
**IM-T20 output_that_is_not_arrow.** `--garbage`: "is not an Arrow IPC stream". f.7.
**IM-T21 a_nonzero_exit_is_named.** `--exit 3`: every batch was answered and the run still ends naming status 3. f.7, 10 f.6.
**IM-T22 the_kind_passes_its_suite.** `kinds::conformance::kernel` over a real layout. 18 f.4, KD-T4.
**IM-T23 the_process_is_alone.** `--report`: user 0 in its namespace, process 1, exactly the image's and the settings' environment, a root of the image's entries, three devices, no process but itself. IM-I4, IM-I5, e.3.
**IM-T24 a_missing_command_is_named.** Status 127 and where it was looked for; a user that is a name is refused when the kernel is built. h, IM-I4.
**IM-T25 the_filter_is_the_documents.** (unit, Linux) The lists in code are the lists of e.4, name for name, and no call named fatal there is on them. e.4.
**IM-T26 the_output_is_read_with_care.** (unit, Linux) The reader of f.6 over bytes: batches, dictionaries, the marker and the pipe closed at a boundary end the stream; a declared length over the limit, a stream that ends inside a message and bytes that are no message are refused with their reason. f.6.
**IM-T27 the_words_of_a_failure.** (unit, Linux) The identity, the environment, the paths of a command, the description of every way a process ends and the quoting of its standard error. f.7, h.
**SC-T27 a_kernel_finish_ends_the_run.** (unit, scheduler) A kernel whose `finish` fails ends the run as `Terminated`, with the stage named and the sink not finished; a kernel whose `finish` succeeds is finished once per instance, after the last apply and before the sink. 10 f.6, contracts d.7.

## l. Implementation notes for the agent

The process's first fork is `libc::fork`, not a raw `clone`: the C library's fork handlers make the allocator safe in the child of a multi-threaded process, which is all the first child needs. Nothing in the child takes a lock another thread of Moruna may hold: no logging, no environment access. A change to the filter updates e.4 in the same commit (IM-T25).

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KA1: unpack with whiteouts and a digest cache | IM-I1, IM-I2, IM-I3 | IM-T1 to IM-T15 |
| KA1: run the command under the sandbox | IM-I4, IM-I5, IM-I7, IM-I8 | IM-T17, IM-T18, IM-T19, IM-T23 |
| KA1: Arrow IPC over stdin and stdout | f.6 | IM-T17, IM-T20, IM-T26 |
| KA1: failures name their cause | IM-I6 | IM-T18 to IM-T21, IM-T24, IM-T27, SC-T27 |
| 18 (the kinds registry) | e.5 | IM-T16, IM-T22 |

## o. Deferred (post-v1)

**IM-O1. A registry adapter** of `ImageSource`. **IM-O2. Layers as shared read-only devices** and an overlay, so a root is not unpacked per image (the microkernel's KC2). **IM-O3. A per-call timeout** and a cancel that reaches a call in progress. **IM-O4. The `user` as a name,** looked up in the image's own passwd. **IM-O5. Image indexes,** with a platform chosen.
