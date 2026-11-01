# Moruna SDD 28: The Linux personality (`kernel/linux`), and the kernel calls it needs

**Document type:** software design document, component 28
**Status:** DRAFT · 2026-10-29, amended by KD1 the same day (`moruna.linux` 1.1 `run_plane`, a data-plane task's descriptors 0, 1 and 2, `poll` served; SDD 29 f.7, f.8) and by KD2 the same day (`moruna.linux` 1.2 `run_granted`; 1.3 `layers`, the seals of the layers its programs start on; a granted program's start page in its auxiliary vector, a program ending with its connection; eventfd, epoll, rename, `sysinfo`, `/proc/meminfo` and `/sys/devices/system/cpu/online`; calls answered later; SDD 30) (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KC1 (Brackly, 2026-10-28): "the Linux personality server: the 60 syscalls; static musl, then glibc, then CPython", with the brief of 2026-10-29: the personality is a server, not kernel code; a generic per-process syscall forward; the hot calls answered in the kernel; partial unmap and `mprotect`, mapping at a chosen address, the FS base and `PT_TLS`, a futex; `PT_INTERP`; files on the image filesystem's protocol; the table of `architecture/optimized-runtime.md` section 3, every other call `ENOSYS` and counted; the three channel calls left to KD1
**Decisions:** none new; MK1 (OSTD) stands
**Component location:** `kernel/linux/core` (`moruna-linux`: the table, Linux's structures, the ELF loader's plan, the first stack, file mappings, descriptors, paths, the refused calls; `no_std`, host tested), `kernel/linux/protocol` (`linux-protocol`: `moruna.linux`), `kernel/linux/server` (`linux`, the personality server), `kernel/linux/test` (`linux-test`, the boot's client), `kernel/linux/programs` (the C test programs); the kernel additions in `kernel/core/src/{foreign,futex,vm,process,syscall,usercopy,clock,boot}.rs`, `kernel/abi` (ABI 2.0), `kernel/sys`, `kernel/loader` (`PT_TLS`), `kernel/programs/src/bin/{vm,supervisor,isolation}.rs`; `crates/moruna-image` (`layerfile::tree`, `moruna-layer stack`); `kernel/tools/{build,boot}.sh`, `kernel/tools/{python-layout.sh,oci-manifest.py}`, `kernel/initramfs/boot.image`
**Consumes:** `moruna-svc` (the dispatcher, watches, deferred answers), `moruna-sys` (ABI 2.5), `imagefs-protocol` 3.0 (SDD 27 d.1), the supervisor's process resource. **Consumed by:** KD1 (the data plane, whose calls a Linux task makes beside its Linux ones), KD2 (the engine starts image tasks through this personality), KD3 (conformance of every kind on the kernel).

**Decisions worth your eye:** (1) the kernel knows no Linux number: a process's forward carries a table, given by its server, naming which foreign numbers reach which of the kernel's fast calls, so another personality is another server and another table; (2) the fast calls speak the x86-64 System V convention (arguments, `-errno`), because they exist to answer a foreign program in its own ABI, and each takes only what it can take whole (an `mmap` of a file, a futex operation it does not know, an `arch_prctl` code other than the FS base go to the server); (3) the native calls moved to `0x1_0000 + n` (ABI 2.0) so the whole low range is the foreign program's; (4) the server builds a process's memory from outside (`process_map` at an address, `process_write`), as Linux's `execve` does, and a file's read-only pages are the image filesystem's own memory object, shared by every task that maps them; (5) a signal is never delivered: a signal sent to itself with a handler installed ends the process, saying so, and is counted; (6) zero pages are filled on first touch in the kernel (SDD 20 f.12), so a program's heap, stack and arena reservations cost only what it touches, while a file mapping is still read whole when made, because the kernel has no pager (m.1).

---

## a. Purpose and boundary

An OCI image's program is an unmodified Linux ELF. This component runs it as a process on the Moruna microkernel: its system calls are answered with Linux's meaning, its files are its image's (the image filesystem, SDD 27) and its writes go to its own scratch directory. It owns: the generic forward of a process's foreign calls to a server, and the fast calls the kernel answers itself; the kernel calls a server needs to build and serve another process (`process_map`, `process_unmap`, `process_protect`, `process_read`, `process_write`, `process_launch`, `forward_clone`); the system call table and every call's translation; the ELF loader with `PT_INTERP`; the protocol that runs a program; the counts of refused calls.

It refuses to know: the data plane's mechanism (`batch_get`, `batch_put`, `port_call`, SDD 29: a Linux task makes them itself as native calls; this server only reads and writes a `run_plane` task's slots for its descriptors 0 and 1, f.7), how an image is fetched or attached (SDD 19, KD4), the engine (KD2), networking (no `socket`, by design: optimized-runtime section 3), and new processes from a Linux program (`fork`, `vfork`, a `clone` without `CLONE_VM`).

## b. Vocabulary

**Forward.** The kernel's end of a channel and a table, given to a process before it starts (`process_forward`): its foreign calls go there. **Foreign call.** A system call outside the native range. **Fast call.** A foreign call the kernel answers itself, as the table names (SDD 20 f.14). **Personality server.** The process holding the other end, `linux`. **Task.** One Linux program run: its process, its forward, its connection to the image filesystem, its descriptors. **Token.** The thread id a request carries and its reply returns. **Refused call.** A call answered `ENOSYS`, or taken only in part, counted by number.

## c. Invariants

- **LX-I1.** The kernel holds no Linux call number: which foreign number is which fast call is the forward's table, and every other foreign call is a message to the server (SDD 20 KK-I15).
- **LX-I2.** A Linux process holds no handle unless its starter granted it one with `run_granted` (1.2, SDD 30 EN-I2): every native call that takes one answers `BadHandle` for every value (LX-T15), and nothing it can name reaches another process (SDD 20 KK-I1). A granted program holds exactly what it was granted, named on its start page (KD2-T4); in the CI boot that is the engine and `granted.c`, and no task.
- **LX-I3.** A Linux process's files are its image's, read only, and its own scratch directory: each task has its own connection to the image filesystem, so another task's `/tmp` is not reachable (SDD 27 IF-I8, IF-I9; LX-T15).
- **LX-I4.** Every call outside the table is answered `ENOSYS` and counted against the process under its number, and the process goes on; a call the table takes only in part (`clone` without `CLONE_VM`, an unknown `prctl` option or `fcntl` command, an `open` flag it does not take, a handler on a signal sent to itself) is counted too. The counts are reported when the process ends, on the console and in the answer to its `run` (LX-T14).
- **LX-I5.** A file is mapped by the range asked for, never whole: read-only pages are the image filesystem's mapping of that range, a private writable mapping is a copy of that range in zero pages, and pages past the file's end are zero (SDD 27 f.4; `moruna_linux::mmap`).
- **LX-I6.** A fast call acts only on the calling thread and its process: its own memory, its own futex words, its own FS base, its clocks (SDD 20 f.14).
- **LX-I7.** A thread's FPU registers and FS base are its own across switches, so the threads of a C library share CPUs as on Linux (SDD 20 KK-I16; LX-T11 on one, two and four CPUs).

## d. Interfaces

### d.1 The forward (ABI 2.0; SDD 20 d.2)

`process_forward(process, endpoint, table, 520)` on a created process. The table: `u32` magic `MKF1` (`0x31464b4d`), `u32` zero, then 512 bytes, byte `n` the fast call foreign number `n` reaches (0: forwarded). The fast calls (`moruna_abi::forward::fast`): 1 `futex`, 2 `clock_gettime`, 3 `nanosleep`, 4 `clock_nanosleep`, 5 `mmap` (anonymous), 6 `munmap`, 7 `mprotect`, 8 `brk`, 9 `madvise`, 10 `sched_yield`, 11 `arch_prctl` (`ARCH_SET_FS`, `ARCH_GET_FS`), 12 `set_tid_address`, 13 `getrandom`, 14 `exit`, 15 `gettid`, 16 `gettimeofday`, 17 `time`, 18 `clock_getres`.

A request, the kernel to the server, 80 bytes: `u32` kind (1, a call), `u32` the process's live threads, `u64` token, `u64` number, `[u64; 6]` arguments, `u64` the instruction pointer after the call. A reply, 40 bytes: `u64` token, `u64` value, `u64` action (0 return the value; 1 end the thread; 2 end the process with code `value`; 3 restart at entry `value` with stack pointer and heap start, an `execve`), `u64` stack pointer, `u64` heap start.

### d.2 The table

`moruna_linux::table::TABLE`, 85 calls, the brief's (optimized-runtime section 3) and what glibc, musl and CPython's start asks of them. Fast: the 18 above (`futex`, `clock_gettime`, `clock_getres`, `gettimeofday`, `time`, `nanosleep`, `clock_nanosleep`, `mmap` of anonymous memory, `munmap`, `mprotect`, `brk`, `madvise`, `sched_yield`, `arch_prctl`, `set_tid_address`, `getrandom`, `exit`, `gettid`). Since KD2 (SDD 30 f.3, f.4, f.9) the server also answers `eventfd`, `eventfd2`, `epoll_create`, `epoll_create1`, `epoll_ctl`, `epoll_wait`, `epoll_pwait` and `epoll_pwait2` (a call that would block is answered later, when a later call of the task readies it or at its deadline, while every other task is served), `rename`, `renameat` and `renameat2` (in the scratch directory, `moruna.imagefs` 1.1; `RENAME_NOREPLACE` taken, the other flags refused and counted), and `sysinfo` (the granted budget, refused without one); `/proc/meminfo` reads as the granted budget and `/sys/devices/system/cpu/online` as the CPUs online. Server: files (`read`, `pread64`, `poll` (KD1: every descriptor is ready for what it is opened for, so each entry is answered at once and a closed one is `POLLNVAL`; Rust's runtime asks it of 0, 1 and 2 at its start and aborts on `ENOSYS`), `readv`, `write`, `pwrite64`, `writev`, `open`, `openat`, `close`, `close_range`, `stat`, `lstat`, `fstat`, `newfstatat`, `lseek`, `getdents64`, `readlink`, `readlinkat`, `access`, `faccessat`, `faccessat2`, `getcwd`, `chdir`, `mkdir`, `mkdirat`, `unlink`, `unlinkat`, `rmdir`, `ftruncate`, `fsync`, `fdatasync`, `fadvise64`, `dup`, `dup2`, `dup3`, `fcntl`, `ioctl`, `mmap` of a file); the process (`getpid`, `getppid`, `getpgrp`, `getuid`, `geteuid`, `getgid`, `getegid`, `getgroups`, `getresuid`, `getresgid`, `umask`, `uname`, `getrlimit`, `setrlimit`, `prlimit64`, `sched_getaffinity`, `sched_setaffinity`, `prctl`, `clone` of a thread, `execve`, `exit_group`, `wait4`, `waitid`); signals (`rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `kill`, `tkill`, `tgkill`, to the process itself). Everything else is refused (LX-I4); the report names `pipe`, `pipe2`, `mremap`, `socket`, `connect`, `fork`, `vfork`, `getdents`, `set_robust_list`, `getcpu`, `statx`, `rseq`, `clone3` and `rt_sigreturn`, and gives any other by number.

How each server call reads Linux: a path is taken from the working directory or a directory descriptor (`*at`, `AT_FDCWD`, `AT_EMPTY_PATH`, `AT_SYMLINK_NOFOLLOW`, `AT_REMOVEDIR`) and handed to the image filesystem, which resolves it (SDD 27 f.6). `open`'s access mode, `O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_DIRECTORY`, `O_NOFOLLOW` become the image filesystem's flags; `O_APPEND` and `O_NONBLOCK` are kept for `F_GETFL`; `O_CLOEXEC` marks the descriptor; `O_TMPFILE` and unknown flags are refused. `/dev/null`, `/dev/zero` and `/dev/urandom` are the server's own; `/proc/self/exe` reads as the program's path; any other `/proc` path is not found (the image filesystem's `/proc` is empty). Descriptors 0, 1 and 2 are the console's streams: reading 0 is at its end, writing 1 or 2 goes to the task's output (kept for the answer, 16 KiB) and to the console, a line at a time, prefixed `linux NAME:`. `stat` gives the image filesystem's kind, mode, size and inode, device 0, owner root, times zero; a console stream is a character device. `getdents64` lists a directory as the image filesystem's `readdir` does, after `.` and `..`, with `d_off` the next position. `ioctl` answers `ENOTTY` (no file is a terminal) except `FIOCLEX` and `FIONCLEX`. Record locks are granted (no other process shares a file). `uname` says `Linux`, `moruna`, `6.12.0-moruna`, `x86_64`. Every id is 0 but the process's, which is its first thread's id. `sched_getaffinity` gives every CPU online (the start page's count, SDD 20 d.4). `getrlimit` gives an 8 MiB stack, 1024 files, the rest unlimited. A signal sent to the process itself: nothing for 0 or an ignored one, the end of the process for one whose action is the default (code `128 + signal`); with a handler installed the process ends too, saying so on the console, and `rt_sigreturn` is counted (LX-I4). `clone` with `CLONE_VM`, `CLONE_SIGHAND` and `CLONE_THREAD` is `forward_clone` (`CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `CLONE_CHILD_CLEARTID`, `CLONE_CHILD_SETTID`). `execve` is taken by a process of one thread.

### d.3 `moruna.linux` 3.0 (`linux-protocol`)

```rust
1 => fn run(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>) -> Ended;
2 => fn run_plane(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, plane: Plane) -> Ended;
3 => fn run_granted(path: Bytes, argv: Vec<Bytes>, env: Vec<Bytes>, grant: Grant) -> Ended;
4 => fn layers() -> Vec<Bytes>;
Plane { input: Option<Handle>, output: Option<Handle>, port: Option<Channel>,
        log: Handle, log_bytes: u64, account: Handle, assets: Option<Channel> }
Grant { handles: Vec<(Bytes, Handle)>, account: Handle, scratch: Option<Handle> }
errors { 1 NotFound; 2 NotExecutable; 3 NoProcess; 4 BadRequest; 5 Busy; 6 BadGrant; 7 NoImage; }
Ended { how: u8 /* 1 exited, 2 signaled, 3 faulted */, code: i64, output: Bytes,
        dropped: u64, refused: Vec<(u64, u64)>, served: u64 }
```

`Plane` carries its actual process Account and optional real imagefs3 assets Channel.
`Grant` carries named startup handles plus actual process and scratch Accounts.
There is no unaccounted request decoder; majors 1 and 2 are refused at hello.
Granted runtimes and plane children receive the reserved `filesystem` readonly
source capability; plane children also receive READ|TRANSFER metadata access to
THE SAME Account used by `Process::create_in`. The actor-local `/assets` mount
uses the actual exported directory, preserves private `/tmp`, refuses mutation
and writable mappings, and confines source links/parent traversal. Readonly
scratch-backed file mappings use the existing bounded charged private-copy path.
No host namespace alias or new kernel ABI is involved (SDD33).

`run_plane` grants the task its actual slot capabilities: descriptor 0 reads the input slot as Arrow IPC and descriptor 1 writes the output slot (SDD29 f.8). Writes to descriptor 2, and to 1 without an output slot, become `moruna-record` entries in `Plane.log`, with the task process name and `stdout` or `stderr` stream header. They do not also enter the console or `Ended.output`. `run_granted` names startup handles on the actual start page, whose address the auxiliary vector carries under `AT_MORUNA_START` (`0x4d4f_5255`). Its process is charged to `Grant.account`; `sysinfo` and `/proc/meminfo` report that actual Account limit. The optional scratch Account controls scratch allocations. Closing the requesting connection ends its process (SDD30 EN-I4). `layers` returns the image filesystem's actual immutable layer seals through the current imagefs3 `layers` request; `NoImage` is returned if unavailable.

The answer comes when the process has ended. At most 24 programs run at once (two waited handles each in one `wait` of 64). The path, arguments and environment fit in 48 KiB and hold no NUL.

### d.4 The server's start (`linux`)

| slot | what |
|---|---|
| `console` | the debug console |
| `supervisor` | a channel on which the supervisor sends a process resource (or `root.process` itself, for a server that is the first process) |
| `fs` | a listener of the image filesystem that serves the Linux image; one connection a task |
| `listen*` | listeners of `moruna.linux` |

## e. Data model, formats and state machines

### e.1 Linux's structures (`moruna_linux::types`)

`struct stat` (x86-64, 144 bytes), `linux_dirent64` (padded to 8), `struct utsname` (6 fields of 65 bytes), `struct iovec`, `struct timespec`, `struct rlimit`, `struct kernel_sigaction` (32 bytes with an 8-byte mask), the errno values, `open`'s flags and the `*at` flags.

### e.2 A task

A process (its handle, with a `WAIT` duplicate the server waits on), the server's end of its forward, its connection to the image filesystem, its descriptors (`moruna_linux::fd`: the lowest free number, `O_CLOEXEC`, `dup` sharing one open file description and its offset; an image filesystem file is closed when its last descriptor is), its working directory, its program's path, its signal actions, each thread's signal mask and alternate stack, its umask and name, its output, and its refused calls. It ends when its process does; the client's `run` is answered then.

### e.3 The address space a program starts with

A position-independent program at `0x5555_5555_4000` (Linux's `ELF_ET_DYN_BASE`), a fixed one where it says; its interpreter at `0x7f00_0000_0000`; its heap from the end of its program's segments (`process_launch`'s heap start); an 8 MiB stack of zero pages below `0x7fff_f000_0000`; anything a fast `mmap` or a file mapping places, from `0x1000_0000_0000` up (where the kernel chooses, SDD 20 f.5).

## f. Algorithms and policies

### f.1 A call, forwarded

The thread's foreign call: if the table names a fast call that takes it, the kernel answers. Otherwise the kernel keeps the thread's registers (for `forward_clone`), queues the request to the server's end, and waits on its own end for the reply with its token; the server reads the request, answers it (reading and writing the caller's memory with `process_read` and `process_write`), and writes the reply. The server is single threaded: a dispatcher over its listeners, each task's forward channel and each task's process handle. A request that needs the image filesystem waits for it inside the request (SDD 27 g).

The round trip, measured by LX-T16 under TCG (QEMU 10.2.1, `-cpu max`, emulating x86-64 in an arm64 Linux VM on an Apple host; KVM is not there, and a KVM host runs these paths at native speed). The CI boot runs every test at once, so each figure is the least of its samples (the mechanism's own cost) and the median (what CPUs shared with the other tests gave):

| CPUs | forwarded `getppid`, least (median) | fast `clock_gettime` | fast `gettid` | fast `futex` wake, no waiter |
|---|---|---|---|---|
| 1 | 82 µs (269 µs) | 656 ns (949 ns) | 399 ns (554 ns) | 483 ns (663 ns) |
| 2 | 45 µs (89 µs) | 688 ns (1.5 µs) | 405 ns (696 ns) | 488 ns (1.1 µs) |
| 4 | 52 µs (97 µs) | 783 ns (1.2 µs) | 464 ns (656 ns) | 558 ns (769 ns) |

A forwarded call costs some hundred times a fast one: two channel messages, two wakes and the server's turn. That is why every call a program makes in a loop (its clocks, its locks, its memory, its thread id) is a fast call, and the forwarded ones are the ones a program makes when it opens, reads or stats a file, each of which costs an image filesystem request anyway.

### f.2 Loading (`execve`, `run`)

The program is opened (a regular file with an execute bit, `EACCES` otherwise); its ELF header and program headers are read (`moruna_linux::elf`: ELF64, little endian, x86-64, `ET_EXEC` or `ET_DYN`, `PT_LOAD` offsets congruent with their addresses, at most 128 headers). Each `PT_LOAD` is mapped as a private file mapping at its page (f.3), its file part limited to its file bytes, so a writable segment's bytes past `p_filesz` are zero. With `PT_INTERP` the interpreter is opened and mapped the same way at its base, and the process starts at its entry; `AT_BASE` is its base. The stack is built as Linux lays it out (`moruna_linux::stack`): `argc`, the argument and environment pointers, the auxiliary vector (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_FLAGS`, `AT_ENTRY`, the ids, `AT_HWCAP` and `AT_HWCAP2` zero, `AT_CLKTCK` 100, `AT_SECURE` 0, `AT_EXECFN`, `AT_PLATFORM` `x86_64`, `AT_RANDOM` sixteen bytes of `random_read`), the strings, 16-byte aligned; there is no vDSO, so the C library makes its clock calls as system calls, which are fast calls. `run` launches the process (`process_launch`); `execve` unmaps the old program, closes the `O_CLOEXEC` descriptors, resets every handled signal to its default, loads, and restarts the thread (reply action 3), after checking the new program loads, so a program that cannot be loaded leaves the old one running with the errno.

### f.3 A file mapped

`moruna_linux::mmap::plan`: the length in whole pages, the pages the file holds from the offset, and whether to copy (a private writable mapping, or a scratch file the image filesystem cannot map). The range is reserved as zero pages (at the address with `MAP_FIXED`, refused when taken with `MAP_FIXED_NOREPLACE`, at the hint when free, else where the kernel chooses). A copy reads the file part through the image filesystem's `read` into those pages with `process_write`, and then takes the protection asked for. Otherwise the image filesystem's `map` of exactly the file part is mapped over the reservation with the protection asked for; its handle carries no `WRITE`, so the pages can never be made writable (SDD 20 KK-I14), which is why a writable private mapping is a copy. `MAP_SHARED` with `PROT_WRITE` is `EACCES` (the image is read only). `/dev/zero` maps zero pages.

### f.4 Threads

`clone` of a thread: `forward_clone` starts a thread with the caller's registers (result register zero), the new stack, the new FS base with `CLONE_SETTLS`, the clear-tid word with `CLONE_CHILD_CLEARTID`, and its id written to the parent's word with `CLONE_PARENT_SETTID` before it runs. A thread that ends with `exit` (a fast call) clears and wakes its clear-tid word, which is how `pthread_join` learns it has gone (SDD 20 f.14). `exit_group` ends every thread. `clone3` is refused, and glibc falls back to `clone`.

### f.5 Time, randomness, memory

The fast calls answer the clocks from the kernel's monotonic clock and the realtime clock read from the CMOS clock at boot (SDD 20 f.16), the process and thread CPU clocks from the kernel's accounting (SDD 20 f.15); `getrandom` from the CPU's random source; anonymous `mmap` with zero pages (`MAP_FIXED`, `MAP_FIXED_NOREPLACE`, a hint taken when free; `PROT_WRITE` with `PROT_EXEC` is `EACCES`); `brk` from the heap start the server gave; `madvise` `MADV_DONTNEED` and `MADV_FREE` drop zero pages (they read zero again), other advice changes nothing.

### f.6 Refused calls

The server counts each refused call by number in the task and reports the counts when the process ends: `linux: NAME (pid N) exited with code C; S calls served; refused: name(nr) xcount, ...`, and in `Ended::refused`. CPython 3.14's `python3 -c "import json, math; print(json.dumps(math.factorial(20)))"` made 577 forwarded calls and was refused four: `mremap(25) x1` (glibc's `realloc` of a large block, which falls back to copying), `sysinfo(99) x1` (glibc asking for the physical memory, which falls back), `set_robust_list(273) x1` and `rseq(334) x1` (glibc's thread start, which goes on without either). The glibc program was refused the last two; the musl programs nothing.

## g. Concurrency within the component

The server is single threaded (f.1). In the kernel, a forwarded thread's request and its wait are under the forward's channel lock and then the forward's set of waiting tokens (a leaf); a futex wait and wake are under the futex queues' lock, the word read under it already populated (SDD 20 f.13); an address space's areas are under its own lock, a leaf (SDD 20 f.5). Threads of one Linux process run on every CPU (LX-T11 on two and four CPUs).

## h. Behaviour

The CI boot: a second image filesystem (`imagefs-linux`) serves the Linux image (the python:3.14-slim image's four layers and one of the test programs above them, on `block3` to `block7`); the supervisor gives the personality server a process resource; `linux-test` runs LX-T10 to LX-T16 through `moruna.linux` and exits; the server ends when its last client has gone, printing `linux: served: N programs run` (eight of `linux-test`, two of SDD 29's `plane`, and with a vsock SDD 30's four runs of the engine and their six tasks: 20 with a vsock, 10 without), and the second image filesystem after it.

## i. Configuration

None at run time. Constants: 1024 descriptors a task; 1 MiB a `read` or `write`; 24 tasks at once; 16 KiB of output kept a task; the addresses of e.3; the identities of d.2.

## j. Observability

The server prints its start (`linux: up: L listeners, C CPUs, 84 calls in the table`), each program's output lines, each program's end with its counts (f.6), a program it could not start and why, and a signal handler it would not run. The kernel prints each process's end as for any process, and a forwarded process whose server went or broke its protocol ends with the reason.

## k. Tests

Host tests run with `cargo test -p moruna-linux -p linux-protocol` in `kernel/`. Boot tests run in the kernel job's boots on 1, 2 and 4 CPUs (`build.sh` builds the C programs with musl and glibc and writes the Linux image; `kernel/tools/python-layout.sh` writes the python:3.14-slim layout it starts from), judged by `check-boot.sh`.

| id | test | where | asserts |
|---|---|---|---|
| LX-T1 | `lx_t1_*` | `moruna-linux` | the table in number order, every entry named and answered, at least 60 calls; refused names not in it; the forward's table maps exactly the fast calls and reaches every fast call the kernel has |
| LX-T2 | `lx_t2_*` | `moruna-linux` | `struct stat`, `linux_dirent64` padding, `utsname`, `iovec`, `timespec`, `rlimit`, the limits, the default-ignored signals, the flags |
| LX-T3 | `lx_t3_*` | `moruna-linux` | a static and a position-independent dynamic program's plans (segments, interpreter, program headers' address, span); every refusal; the interpreter's path |
| LX-T4 | `lx_t4_*` | `moruna-linux` | the first stack as Linux lays it out, aligned, its vector and strings; arguments past `ARGS_MAX`, a NUL in one, and a stack that does not fit refused |
| LX-T5 | `lx_t5_*` | `moruna-linux` | descriptors: the lowest free number, sharing through `dup`, `dup2` replacing, `O_CLOEXEC`, `close_range`, the bound |
| LX-T6 | `lx_t6_*` | `moruna-linux` | refused calls counted by number and rendered by name |
| LX-T7 | `lx_t7_*` | `moruna-linux` | paths from a directory; empty and too long refused; the base name |
| LX-T8 | `lx_t8_*` | `moruna-linux` | file mapping plans: read-only pages its own, a private writable mapping a copy, past the end zero, a scratch file copied, a shared writable mapping refused, bad arguments |
| LX-T9 | `lx_t9_*` | `linux-protocol` | `Ended` round trips through a frame |
| LX-T10 | static musl | boot: `linux-test`, `programs/hello.c` | `KC1 musl hello: ok:`: it prints its arguments, reads `/kc1/greeting.txt` of the image, writes `/tmp/out.txt` and reads it back, exits 0, refuses nothing |
| LX-T11 | musl threads | boot: `programs/threads.c` | `KC1 musl threads: ok:`: eight `pthread_create` threads (`clone`, `futex`) sum 1 to 4,000,000 in slices under a mutex, signal a condition variable, are joined, and the total is 8,000,002,000,000, on 1, 2 and 4 CPUs |
| LX-T12 | dynamic glibc | boot: `programs/dynamic.c` | `KC1 glibc dynamic: ok:`: a program built against Debian bookworm's glibc runs through `/lib64/ld-linux-x86-64.so.2` and the python image's glibc 2.41 (its Debian trixie layer), prints the version and its pid |
| LX-T13 | CPython | boot: `/usr/local/bin/python3` | `KC1 cpython: ok:`: CPython 3.14 of the official python:3.14-slim image, converted with KC2's writer, runs `-c "import json, math; print(json.dumps(math.factorial(20)))"` and prints `2432902008176640000`; the line gives its refused calls |
| LX-T14 | a refused call | boot: `programs/refuse.c` | `KC1 refused call: ok:`: `socket()` returns `-1` with `ENOSYS`, the process goes on and exits 0, and its counts say `socket(41) x1` |
| LX-T15 | isolation | boot: `programs/isolate.c` | `KC1 isolation: ok:`: thirteen handle-taking native calls on every value 0 to 4096 are `BadHandle`; another task's `/tmp/out.txt` is not found; its own anonymous memory is zero and its own |
| LX-T16 | the forward's round trip | boot: `programs/bench.c` | `KC1 forward: ok:`: the least and the median of 2000 forwarded `getppid` calls, and of 40 batches of 500 each of `clock_gettime`, `gettid` and a futex wake with no waiter, printed |
| LX-T17 | `lx_t17_*` | `moruna-image` | a host directory written as one layer reads back through the kernel's reader: files with their modes, directories, links; a path that is not a directory refused |

The native calls of ABI 2.0 have their own tests in SDD 20: KK-T13 (`programs/vm`) and KK-T14 (the supervisor); KK-T8 probes the new handle-taking calls with every value.

Results, booted under TCG on 1, 2 and 4 CPUs (`kernel/tools/boot.sh`, judged by `check-boot.sh`): LX-T10 to LX-T16, KK-T13, KK-T14 and KK-T8 (29 calls) pass on every count; CPython took 13 s on one CPU and 4 s on two and four. Two lines of the boot fail on the unchanged base as well, and are not this component's: KB1's `wait-deadline` misses its 50 ms bound under emulation (on one CPU; it passed on two and four), and KC2's `tree` finds `/etc/passwd` with the lower layer's bytes when `build.sh` runs on ext4, whose freed inode the fixture's unpacking reuses for the replacing file, so the conversion sees an unchanged inode and writes no entry for it (SDD 27 f.1 assumes a removed and remade file is a new inode; on the Mac's APFS `if_t13` passes).

Coverage on the host (`cargo llvm-cov`, lines): `moruna-linux` 99.8% (`elf.rs` 99.2%, the rest 100%); `linux-protocol` 100%; `moruna-image`'s `layerfile::tree` by LX-T17 (`lx_t17_a_directory_as_a_layer`: a directory's files, modes and links read back through the kernel's reader). The server and the test client are covered by the boot.

## l. Implementation notes for the agent

- The C programs are built by `build.sh` with `x86_64-linux-gnu-gcc` (and musl's `musl-gcc.specs`) on a host that is not x86_64, with `gcc` and `musl-gcc` on one that is.
- The server reads a task's memory a page at a time for strings, so a string that ends just before an unmapped page is read (`memory::string`).
- `container image save --platform linux/amd64` writes an OCI layout whose index names an image index; `kernel/tools/oci-manifest.py` finds the platform's manifest through it.

## m. Open items

1. **A pager.** A file mapping is read whole when it is made (SDD 27 m.2), so a program's libraries cost what their mapped segments hold, not what it touches. A memory object whose pages the image filesystem fills on a fault is a kernel change of its own (SDD 20 m.1).
2. **Signals are not delivered.** A handler is recorded and returned, never run; a signal the process sends itself with a handler installed ends it (d.2). Delivery (a frame on the thread's stack, `rt_sigreturn`) is the next part of the table that real programs will ask for.
3. **No new processes.** `fork`, `vfork`, `posix_spawn` (a `clone` with `CLONE_VM` and `CLONE_VFORK`) and `wait4` on a child are refused; a task is one process.
4. **One request at a time.** The server waits for the image filesystem inside a request (f.1), so one task's slow read delays the others.
5. **`execve` under one thread.** A process with more than one thread is refused `execve` with `EAGAIN`, and the refusal counted.

## n. Traceability

| requirement (BOARD EK KC1 brief) | here |
|---|---|
| the personality is a server; a generic per-process forward; the thread waits for the reply | d.1, f.1; SDD 20 f.14 (`process_forward`); LX-I1 |
| the hot calls in the kernel, measured | d.1 (fast calls), f.1, f.5; LX-T16 |
| partial unmap and `mprotect`; mapping at a chosen address; the FS base and `PT_TLS`; a futex | SDD 20 d.2 rows 30 to 41, f.5, f.12, f.13, f.15, f.2 (`PT_TLS`); KK-T13, KK-T14 |
| `PT_INTERP`: the interpreter loaded from the image filesystem | f.2; LX-T12, LX-T13 |
| files translated onto KC2's protocol, `/tmp` the scratch directory, a file's `mmap` of the range only | d.2, f.3; LX-I3, LX-I5; LX-T10, LX-T13 |
| the table, `ENOSYS` counted per process, reported at its end | d.2, f.6; LX-I4; LX-T14 |
| the three channel calls left to KD1, their numbers reserved | a; SDD 20 d.1 |
| static musl, threads on 1, 2 and 4 CPUs, dynamic glibc, CPython 3.14 | LX-T10 to LX-T13 |
| isolation: no handles beyond the personality's | LX-I2, LX-I3; LX-T15 |

## o. Deferred

**LX-O1. The data plane on a Linux task.** Resolved by KD1 (SDD 29): the three calls at their native numbers, and the two pre-opened descriptors of optimized-runtime section 7 for an image brought without a shim (`run_plane`, f.8 of SDD 29). **LX-O2. A vDSO**, so the clock calls cost no system call at all. **LX-O3. Signals, processes, pipes** (m.2, m.3). **LX-O4. A server per CPU** (m.4).
