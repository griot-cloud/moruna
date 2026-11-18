# Rust's `library/` for Moruna (system design 5A0, item 5.0)

`library/` is Rust's standard library source as the kernel's pinned toolchain ships it, with
Moruna's platform layer added: `target_os = "moruna"`, so an ordinary Rust program with `std`
builds for `kernel/targets/x86_64-moruna.json` (`kernel/tools/build-std.sh`).

## Where it comes from

- **Toolchain:** `nightly-2026-07-21` (`kernel/rust-toolchain.toml`), rustc
  `1.99.0-nightly (87e5904f5 2026-07-20)`.
- **Component:** that toolchain's `rust-src`, as rustup installs it:
  `https://static.rust-lang.org/dist/2026-07-21/rust-src-nightly.tar.xz`, SHA-256
  `a930fae30d962ea5eff6fe43009d180a9f435cf98432fe30ecc893125cb83c7d` (the channel manifest's
  `xz_hash`).
- **Copied with:**
  `cp -R "$(rustc +nightly-2026-07-21 --print sysroot)/lib/rustlib/src/rust/library" library`.
  Nothing else was fetched for it: the third-party crates under `library/vendor` are the ones
  `rust-src` carries.

Upstream's files stay as `rust-src` has them, apart from the functional patches below; the
repository's text rules do not apply to this copy (`tools/quality/check.sh` leaves it out of its
em-dash check). Every file not listed below is byte for byte the `rust-src` file. To check:

```sh
diff -r "$(rustc +nightly-2026-07-21 --print sysroot)/lib/rustlib/src/rust/library" library
```

lists exactly the files below.

## What changed

Every change is marked `moruna:` in the file it is in (`grep -rn "moruna:" library`), except
`Cargo.lock`'s, a file Cargo writes, which holds no comment.

### The platform layer: `target_os = "moruna"`

Modelled on Motor OS's layer (`std/src/sys/*/motor.rs` and the `moto-rt` crate): each `std::sys`
module Moruna implements is a file of a few lines over Moruna's runtime, `moruna_sys::rt`
(`kernel/sys/src/rt`), which std depends on as Motor's std depends on `moto-rt`, and where the
logic is, tested on a host. The selections in the `mod.rs` files gain a `target_os = "moruna"`
arm beside Motor's.

- **Entry:** `sys/pal/moruna`, the one `_start` for std and the C library (system design 5B).
  In order: the first thread's thread-locals and control block from the start page's process;
  std's log, which stdout and stderr (and the C library's descriptors 1 and 2) write to; the C
  library's own initialization when one is linked (it defines `moruna_libc_init`, a weak symbol
  here); the program's `.preinit_array` and `.init_array`, which `kernel/targets/x86_64-moruna.ld`
  bounds (item 5.2b); then `main`, with the process's name as its one argument (`argc` 1). After
  `main`, the first thread's thread-local destructors, then the C library's `exit` when one is
  linked (`moruna_libc_exit`: its `atexit` functions, the fini array, its streams flushed), or
  else the fini array here, and the exit with its code once the last line of stdout and stderr
  is out. `abort_internal`, where a panic ends, exits with code 101, a
  panicking program's code on Moruna.
- **alloc:** the process's heap, `moruna_sys`'s, which std's `System` allocates from.
- **thread:** the one thread path, `moruna_sys::rt::thread`, which the C library's
  `pthread_create` takes too: a 2 MiB stack by default (as Linux's std; arrow-rs and query
  engines recurse deeply) with a guard page below it; join, sleep, and yield (`thread_yield`,
  ABI 2.8). Every thread ends the runtime's one way, whichever runtime started it
  (`rt::thread::end_current`, item 5.2b): the C library's part (its cleanup handlers, its
  `thread_local` and key destructors), then std's (`sys/pal/moruna`'s `thread_end`: its
  thread-locals' destructors, run by std itself as Hermit's std does, and its handle of the
  thread), each set by its runtime's start.
- **thread-local storage:** native ELF TLS (`has-thread-local`, the `local-exec` model; x86_64
  psABI variant II), the key table gone (system design 5B). Each thread's block is its TLS,
  made from the program's `PT_TLS` template, directly below its control block, whose address
  is the thread's FS base and the control block's first word (FS:0).
  `kernel/targets/x86_64-moruna.ld` maps the program's headers at `__ehdr_start`, where the
  runtime finds the template. The control block (`moruna_sys::rt::tls::Control`) begins as
  relibc's `GenericTcb` does and keeps 512 bytes for the C library's per-thread state.
- **sync:** std's futex `Mutex`, `Condvar`, `RwLock`, `Once` and thread parking, over the core's
  futex calls (`moruna_sys::rt::futex`, with timeouts on the monotonic clock).
- **time:** `Instant` from the clock page; `SystemTime` from the node's wall clock on the same
  page (ABI 2.8: its realtime base, set from the CMOS clock at boot and by `platform`). While
  the wall clock is unset it reads 1970 plus the monotonic clock, as the Linux layer's does; it
  never panics.
- **random:** the core's `random_read`.
- **stdio:** stdout and stderr are lines of the task's log (`moruna_sys::plane::Log`, start slot
  `log`); with no log, lines on its console (start slot `console`), each in one write; with
  neither, dropped. stdin is empty.
- **env and args:** from the start page: the arguments are the process's name, and the
  environment is empty; setting a variable is unsupported.
- **fs, net, process, pipe:** std's unsupported implementations: every call answers
  `Unsupported`.
- **`std::os::moruna`:** `moruna_sys`, std's own copy, re-exported as Hermit's std re-exports
  `hermit_abi`, so a program reaches Moruna's calls with std's state (its start page, the slots
  std took, its log) rather than through a second copy.

### Offline: std's dependencies are `library/vendor`'s

Cargo's build-std reads `library/Cargo.lock` but not `library/.cargo/config.toml`, so by itself
it takes std's crates.io dependencies from crates.io. `library/Cargo.toml`'s `[patch.crates-io]`
points each of the 30 crates `rust-src` vendors at its copy in `library/vendor`, so std's lock
names no registry and its build fetches nothing; `build-std.sh` checks the lock before every
build and builds `--offline`. A build without std's optional features (no backtrace
symbolication, no tests) uses only some of them, and Cargo says the others' patches were not
used.

### Files

| File | Lines | Changed | SHA-256 in `rust-src` | SHA-256 here |
|---|---|---|---|---|
| `Cargo.lock` | 424 | +30 -64 | `9e87d1ac04edbf5fa61e27cb21984a83566573a007767713868965fba70acb6d` | `333f5bc1f18d100f8f8255bbdaddca78db00643f6327262b27bb19af5d9e9b35` |
| `Cargo.toml` | 122 | +32 -0 | `b346ae33bd9648949894510a2bcc1d1f8b78c7301c3110e4a98b68bacd3b4584` | `571f4569f6f5718a19f1feac9b58841fca3145f98b74ad2ef50805694023cf3a` |
| `std/Cargo.toml` | 176 | +5 -0 | `ef320a0ca2b3bb4d68c3cdeb2fd226ac83d93093bb1c10f4390dcfd8c11f1a9a` | `5ff5f3e4fdf4a742129c90edb418181c353b7a77faf86fb7437aa65909b3176b` |
| `std/build.rs` | 109 | +2 -0 | `f5510c905e126f1de772d73c02cacb7a5ed0a485a22fb99f395593b16a9de8b2` | `b37021b451d7c8538070febf22a2c0cbc554094d081d2ecfab113edca628c49b` |
| `std/src/os/mod.rs` | 150 | +3 -0 | `b05633fdbbfe57827651e13afc896ac9b1edfdbcca13d1a6bb4d5444c82bdb0e` | `ff54c71423bf05a6f61d7d6a661ec5a85b5a0f84e805d427077f03bf030c91f9` |
| `std/src/os/moruna/mod.rs` | 14 | +14 -0 | new | `cb0cee87c33e0eedf611156d2b76e0c482fbe2ee41f8bcf88c909f17730f4028` |
| `std/src/sys/alloc/mod.rs` | 144 | +5 -0 | `7c40f1c30fcf850a5fa25361f779b7b1205c94486fb42acf83e9dda1e71915da` | `a6e88909538a209dedecdc31a201ce72d8c5a185daa2cb6a78d1b0f466c67e3f` |
| `std/src/sys/alloc/moruna.rs` | 3 | +3 -0 | new | `2b58b859d36bdb9ff1091b7ed1c37ecf148129bbae45da353dc48f206559e648` |
| `std/src/sys/args/mod.rs` | 71 | +7 -0 | `555d13de7285b38f85c22c162a3e6a34bbdbdc3d5161185d93f7c5339fe366e9` | `3fb4a68bc12be14c5c7a4f9acba8f13cd07919bf6e74cfeda4f2ce5b70f6a426` |
| `std/src/sys/args/moruna.rs` | 9 | +9 -0 | new | `e546896dda0da8e6585afe135fc99fb7813de171e316dceeae31c81123cb57ca` |
| `std/src/sys/env/mod.rs` | 69 | +7 -0 | `289b1cbe20f832c77397b87d996dbb17944ec96f48228f7bfd2a09a6cc9601c4` | `ec91605682920bd2a6c2276904fe470de18f3ddfa6b9e769c67c241f9d3224f6` |
| `std/src/sys/env/moruna.rs` | 22 | +22 -0 | new | `0096ec86aa09f611f544197ef7d0f70679e451f6e1803ab5e4a69d72f18e4feb` |
| `std/src/sys/exit.rs` | 166 | +4 -0 | `86fb31659a64ab6b5830e57b6ae506d9c3c5f424b585f4f90bb1f364cb64de04` | `6562bd174272860254af743889842a213c4eef10d2db5ff9e6a2e90d56182210` |
| `std/src/sys/io/error/mod.rs` | 48 | +5 -0 | `1b591042fe82bc290244f6099db38bdb233578cdbcb2d927eab1181f035389d7` | `ad87700d9467fc5c6d9bf978ded0380d9bd15436fd99c40711999359e884f22c` |
| `std/src/sys/io/error/moruna.rs` | 43 | +43 -0 | new | `21cbf255f65ed305d6f9e32708fea40deb7ac67e484252e17df769d204d08948` |
| `std/src/sys/pal/mod.rs` | 72 | +5 -0 | `251dee50eff49b81ae5283ffd9871635dc63c316e0156a0f6f69672657ddbec3` | `6feb3489f30948c6a4bf197beb558874a5ef495beeb2365c1d43b786d144e99d` |
| `std/src/sys/pal/moruna/mod.rs` | 166 | +166 -0 | new | `fccbcb14697da2000bea62585df943605ddee74810eedea57f47155edf4f5fc5` |
| `std/src/sys/random/mod.rs` | 138 | +5 -0 | `38d7c91c3fee07226c5071a4e8edd75dd693f54cd76bc7f2a06fd8b80d0cb794` | `0603c349f20dcd2d5f87321465ed3d7fb2840841aa5baadd6b4bb752ea9a8af7` |
| `std/src/sys/random/moruna.rs` | 7 | +7 -0 | new | `b5e898270826fc097a484efb411cc622261702726e0edee54e3d2b39330c207e` |
| `std/src/sys/stdio/mod.rs` | 57 | +5 -0 | `ffc59c348ecc12e0d4748855309112f13979392983cafb677be6b82fc6e74d59` | `5a4ee16a5de372d607316f6efc08e7a11b5cf1b4a4f83cdfbfbe0ca7c47c443f` |
| `std/src/sys/stdio/moruna.rs` | 47 | +47 -0 | new | `32d418c86a5f9831ee793b3a22db37f1a1706e56332d0d693a5252018a0b1d0a` |
| `std/src/sys/sync/condvar/mod.rs` | 46 | +2 -0 | `af88b86441a57ae779ae0a01528bc4e8b0068f1e72978547c1a08ac4d7107c2d` | `803c7ffb436c06a7e5419de9bf30ee295f8ca998aa48527ddc1dad4b6a36563a` |
| `std/src/sys/sync/mutex/mod.rs` | 49 | +2 -0 | `1c715675a2bc34471b96de58f7aaaf2db411289cf838d17edc61cb2d6b006aa2` | `c3066135dce38da688d000d945e957d56c69a1e8ebfb40c77bbce53832bdd62b` |
| `std/src/sys/sync/once/mod.rs` | 42 | +2 -0 | `4e9b50925535fe54275f6e3aa1bb26b093d15e4a6168935bdb92d1117271293c` | `537446b2a7ee6399b590c84659392d3365fea01f1002a4bf2ca397c91244f106` |
| `std/src/sys/sync/rwlock/mod.rs` | 37 | +2 -0 | `5438fdbc0ade05b50d7350c252ccb9d37bea596355560fbdc695f4937d727ef3` | `7fbee57f5ac665965a6b0d30dedb4f803dd8ef8255a7d6313b998d7ee97cd80f` |
| `std/src/sys/sync/thread_parking/mod.rs` | 51 | +2 -0 | `26f989d09efadae98b1fa6447b386d8eda21b9fc3ef99c8bea3154d6625788a3` | `cc9cc6f4ad37c89e4cf5e8babff0a51d16e8a1e088fb2420c57eab279f26ec74` |
| `std/src/sys/thread/mod.rs` | 162 | +8 -0 | `3590ca4dc8b112c787b14fcc5585add929a6e7c827ce01c329c2ef43e45587d6` | `cc4904f4fad1399a9a5bc508ca0bdf0cc635113d5def953898d80ab099c13352` |
| `std/src/sys/thread/moruna.rs` | 52 | +52 -0 | new | `edcb19fc2f466900a8efa1df718c928258f6b73d806a10aff2fccef7ddb219a3` |
| `std/src/sys/thread_local/mod.rs` | 224 | +2 -0 | `bc072e04d8564c050420e597b89e2b100502bb1395423277c057ca8aec70317a` | `346931af35a4f0c38e0751681f94fe71f3172a04cb77b2da733c8f9f07754b75` |
| `std/src/sys/time/mod.rs` | 57 | +4 -0 | `449afdd86955252a302d889032a6cb2cbb04dd786f8cb81d9a9059ad933e7e4c` | `dc5f131f9033808546387962e3c22d24f962204bc06b42123d328775035dbbfa` |

No upstream file is removed, and outside `Cargo.lock` no upstream line changes: the layer only
adds. `Cargo.lock` gains the three Moruna crates std depends on (`moruna-sys`, `moruna-abi`,
`moruna-record`), and its 30 vendored crates lose their registry source and checksum, as
`cargo metadata --offline` writes them with the patches.

### Building with it

`kernel/tools/build-std.sh MANIFEST` builds a program for `kernel/targets/x86_64-moruna.json`
with `-Zbuild-std=std,panic_abort`, `__CARGO_TESTS_ONLY_SRC_ROOT` pointing build-std at this
`library/`, getrandom's custom backend (`kernel/getrandom`, the core's `random_read`), and the
native programs' static link, with their layout and the TLS template
(`kernel/targets/x86_64-moruna.ld`). A digest of this `library/`, the target and the Moruna
crates std depends on goes into every unit's metadata, since build-std does not notice an edit to
them by itself. The proof is `kernel/examples/std-arrow`.
