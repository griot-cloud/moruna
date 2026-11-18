# relibc on Moruna

Redox's C library, vendored as the C library of native functions on Moruna (system design
`architecture/moruna-os-system.md`, section 5B; work item `architecture/work/5.2b-relibc.md`).
relibc is written in Rust, `no_std`, and reaches its system through one trait, `Pal`; Moruna's
`Pal` is `src/platform/moruna`, over `kernel/relibc-moruna` and `moruna-sys`.

- **Upstream:** <https://gitlab.redox-os.org/redox-os/relibc>, commit
  `06a7b25d1874bdf0e4c6783fc5a84062d60aebad` (2026-11-17), MIT (`LICENSE`, upstream's, byte for
  byte).
- **Its two submodules,** at the commits relibc pins:
  - `dlmalloc-rs/`: <https://gitlab.redox-os.org/redox-os/dlmalloc-rs>, commit
    `40c10f6182cb1d2a78b29f85492936dfbeded02e`, MIT or Apache-2.0 (`dlmalloc-rs/LICENSE-*`).
    relibc's `malloc`.
  - `openlibm/`: <https://gitlab.redox-os.org/redox-os/openlibm>, commit
    `f5ed774593458e116e69731a3ffa59e2a6408d4e`, MIT, BSD and ISC (`openlibm/LICENSE.md`). The
    C math library; built with the C toolchain for the user target (item 5.2b, step 4).
- **Fetched** 2026-11-18 by `git clone` and a checkout of each commit. Nothing in the clones was
  built or run before it was vendored.
- **Its own workspace,** as upstream's is; `kernel/Cargo.toml` excludes it. Its third-party
  crates come from crates.io through `Cargo.lock` (upstream's, pruned), and `cargo deny` passes
  with `kernel/deny.toml`.

`upstream-files.json` records every file vendored with the SHA-256 of upstream's bytes;
`provenance.json` records the commits, what was left out, and each file Moruna changed or added,
with both hashes. The first commit of this directory is upstream's bytes; every change since is
in git's history.

## Left out

Redox's own crates (`redox-rt`, `redox-ioctl`, and the Redox-only dependencies of the manifest:
`redox_syscall`, `libredox`, `redox-path`, `redox_event`, `ioslice`, `redox-rings`,
`redox-buffer-pool`); Linux's system call crate (`sc`); the dynamic linker's binary (`ld_so`);
the `crt0`, `crti` and `crtn` objects (std's `_start` is the one entry, 5B); upstream's build
scripts (`Makefile`, `renamesyms.sh`, `stripcore.sh`: one crate graph needs no renaming);
dlmalloc-rs's fuzzer; openlibm's other architectures. Upstream's fork of `object`
(`andypython/object`, branch `new`) is replaced by crates.io's `object` 0.36.7, the release it
branches from.

## Changes

- **`Cargo.toml`:** an rlib, in one crate graph with std, the SDK and the C library
  (`kernel/function/c`, system design 5B); the dependencies above removed, `object` from
  crates.io, `relibc-moruna` for `target_os = "moruna"`, versions on the path dependencies; the
  feature `runtime` for the panic, unwind and allocation-error items, which std has in the graph
  and which Moruna's build leaves off; `check_against_libc_crate` kept empty (the `libc` crate it
  compares layouts with knows no Moruna). `Cargo.lock` pruned to match, every other version as
  upstream locked it. `generic-rt` gains its licence field; dlmalloc-rs's manifest loses its
  fuzzer.
- **`src/platform/moruna/`,** added: `Sys`, relibc's `Pal`, `PalSignal`, `PalSocket`,
  `PalEpoll` and `PalPtrace` for Moruna, over `relibc-moruna`, which reaches the core through
  std's own copy of `moruna-sys`: memory, futexes, the clocks (the realtime base and
  `thread_yield` of ABI 2.8), randomness, exit through std's, descriptors 1 and 2 through std's
  writer (the task's log), 0 empty; paths answer `ENOENT`; processes, sockets, epoll, ptrace and
  timers `ENOSYS`; signals stay within the process.
- **One runtime with std (5B):**
  - `src/ld_so/tcb.rs`: relibc's `Tcb` is the start of each thread's one control block, std's
    runtime's (`moruna_sys::rt::tls::Control`), checked at compile time to begin with its
    `GenericTcb` and to fit the bytes the block keeps for the C library; `Tcb::adopt_current`
    writes relibc's own fields into a block the runtime made. A thread's id is kept there
    (`OsSpecific`), which no system call gives.
  - `src/ld_so/mod.rs`: an FS base already set, the control block std's `_start` made, is
    adopted rather than a TLS block made.
  - `src/start.rs`: `moruna_libc_init`, which std's `_start` calls after the first thread's block
    and the log and before the init arrays and `main` (relibc's start without those, argv the
    process's name, no environment), and `moruna_libc_exit`, `exit` for a `main` that returned.
  - `src/pthread/mod.rs`: `pthread_create` takes the runtime's one thread path (its stack, its
    block of TLS, made from the program's template), and keeps the runtime's handle of the thread
    in its `Tcb`; a joinable thread ends keeping its block, in which its `pthread_t` lies, and its
    join or its detach frees it only once the kernel says the thread has ended (the join waits on
    the handle; a detach after the end hands the block to the runtime, which frees it with the
    stack). A thread's end is the runtime's (`end_current`): relibc's part, `moruna_thread_end`
    (cleanup handlers, `thread_local` and key destructors), then std's thread-locals', on
    `pthread_create`'s threads and std's alike. `src/platform/moruna/mod.rs`'s `ThreadId` holds
    the handle.
- **The `target_os` sites:** 59 `target_os = "linux"` conditions outside Linux's platform also
  take `target_os = "moruna"`, so Moruna has Linux's ABI constants (errno, flags, modes, signal
  numbers, the layouts of `stat` and the others). Not the four that make Linux system calls:
  `extern crate sc`, `unistd::syscall`, and FS's set and get. `cbindgen.globdefs.toml` maps
  `target_os = "moruna"` to `__moruna__`, which the C compiler defines, so the headers take the
  same constants.
- **`src/ld_so/dso.rs`:** no object is loaded at run time on Moruna, whose functions are each
  one static program; the lookups in a loaded object's hash tables, which take the fork's
  private fields, are left out of its build.
- **`src/lib.rs`:** the runtime items behind `runtime`.

## Building

`kernel/function/c/build-core.sh` builds it for the core's user target, in the C library's
crate graph with std (`kernel/targets/x86_64-moruna.json`, std from
`kernel/vendor/rust-std/library`): `libmoruna_function.a`, relibc's headers, openlibm's
`libopenlibm.a` from a copy of `openlibm/`, and the C functions linked as static native
programs. The one C file of relibc (`src/c/stdlib.c`) is compiled by the host's clang for
`x86_64-unknown-none-elf`.

The headers upstream's build writes with `cbindgen` (from each `src/header/*/cbindgen.toml` and
`cbindgen.globdefs.toml`) are committed in `include-generated/`, so a build needs no tool of its
own; `include-generated/provenance.json` records cbindgen's version (0.29.4), the relibc commit
and each header's SHA-256. `regen-headers.sh` (with `CBINDGEN` naming cbindgen 0.29.4) makes
them again and fails if they differ from what is committed; `--write` replaces them. It is run
when relibc's pin moves or Moruna's changes reach a header.
