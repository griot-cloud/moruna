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

- **`Cargo.toml`:** an rlib, in one crate graph with std (`cargo rustc --crate-type staticlib
  --features runtime` makes the archive a program without std links); the dependencies above
  removed, `object` from crates.io, `relibc-moruna` for `target_os = "moruna"`, versions on
  the path dependencies; the feature `runtime` for the panic, unwind and allocation-error items,
  which std has in its graph (5B); `check_against_libc_crate` kept empty (the `libc` crate it
  compares layouts with knows no Moruna). `Cargo.lock` pruned to match, every other version as
  upstream locked it. `generic-rt` gains its licence field; dlmalloc-rs's manifest loses its
  fuzzer.
- **`src/platform/moruna/`,** added: `Sys`, relibc's `Pal`, `PalSignal`, `PalSocket`,
  `PalEpoll` and `PalPtrace` for Moruna. Memory, threads, futexes, clocks, randomness, exit and
  descriptors 0 to 2 are `relibc-moruna`'s; paths answer `ENOENT`; processes, sockets, epoll,
  ptrace and timers `ENOSYS`; signals stay within the process.
- **The `target_os` sites:** 59 `target_os = "linux"` conditions outside Linux's platform also
  take `target_os = "moruna"`, so Moruna has Linux's ABI constants (errno, flags, modes, signal
  numbers, the layouts of `stat` and the others). Not the four that make Linux system calls:
  `extern crate sc`, `unistd::syscall`, and FS's set and get, which Moruna does with
  `THREAD_SET_FS` and `rdfsbase` (`src/ld_so/tcb.rs`, `src/ld_so/mod.rs`). A Moruna thread's
  control block keeps its id (`OsSpecific`), which no system call gives.
- **`src/ld_so/dso.rs`:** no object is loaded at run time on Moruna, whose functions are each
  one static program; the lookups in a loaded object's hash tables, which take the fork's
  private fields, are left out of its build.
- **`src/lib.rs`:** the runtime items behind `runtime`.

## Building

For the core's user target with std (`kernel/targets/x86_64-moruna.json`), on the kernel's
toolchain, the one C file (`src/c/stdlib.c`) compiled to ELF:

```sh
cd kernel/vendor/relibc
CC_x86_64_moruna=cc AR_x86_64_moruna="$(rustc --print sysroot)/lib/rustlib/<host>/bin/llvm-ar" \
CFLAGS_x86_64_moruna="--target=x86_64-unknown-none-elf -mno-red-zone -fno-stack-protector -fPIE" \
  cargo build --target ../../targets/x86_64-moruna.json -Zunstable-options -Zjson-target-spec \
  -Zbuild-std=core,alloc,compiler_builtins -Zbuild-std-features=compiler-builtins-mem
```

## Not yet

What 5B's one runtime needs from std (item 5.0's change: ELF TLS with one control block, one
thread path, one `_start`, std's writer for descriptors 1 and 2, the realtime clock and
`thread_yield`) is used when it lands; until then descriptors 1 and 2 write to the task's
console, the wall clock is the monotonic clock, and `sched_yield` returns at once. The headers,
`libmoruna_function.a` for the user target and openlibm's build are step 4.
