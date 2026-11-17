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
- **std's own dependencies at build time:** Cargo's build-std reads `library/Cargo.lock` but not
  `library/.cargo/config.toml`, so it takes std's crates.io dependencies (`hashbrown`, `libc`,
  `cfg-if`, `rustc-demangle`) from crates.io, at the versions and checksums the lock pins: the
  same bytes as their copies in `library/vendor`. This is what build-std does with the
  toolchain's own `rust-src` too.

Every file not listed below is byte for byte the `rust-src` file. To check:

```sh
diff -r "$(rustc +nightly-2026-07-21 --print sysroot)/lib/rustlib/src/rust/library" library
```

lists exactly the files below.

## What changed

Every change is marked `moruna:` in the file it is in (`grep -rn "moruna:" library`), except
those in `Cargo.lock` and the vendored crates' `.cargo-checksum.json`, files Cargo writes and
which hold no comment.

### The platform layer: `target_os = "moruna"`

Modelled on Motor OS's layer (`std/src/sys/*/motor.rs` and the `moto-rt` crate): each `std::sys`
module Moruna implements is a file of a few lines over Moruna's runtime, `moruna_sys::rt`
(`kernel/sys/src/rt`), which std depends on as Motor's std depends on `moto-rt`, and where the
logic is, tested on a host. The selections in the `mod.rs` files gain a `target_os = "moruna"`
arm beside Motor's.

- **Entry:** `sys/pal/moruna`: `_start` takes the start page's address, gives the first thread
  its table of thread-local keys, runs the program's `main` and exits with its code, once the
  log has stdout's and stderr's last line. `abort_internal`, where a panic ends, does the same
  with code 101, a panicking program's code on Moruna.
- **alloc:** the process's heap, `moruna_sys`'s, which std's `System` allocates from.
- **thread:** spawn (a stack of its own with a guard page, and a key table), join, sleep and
  yield; thread-local storage is std's key-based kind (`thread_local::key`, as Motor and Xous),
  each thread's FS base pointing at its table. The core has no call that gives the CPU away, so
  yield is a hint to the CPU only.
- **sync:** std's futex `Mutex`, `Condvar`, `RwLock`, `Once` and thread parking, over the core's
  futex calls (`moruna_sys::rt::futex`, with timeouts on the monotonic clock).
- **time:** `Instant` from the clock page. A native task is given no realtime base, so
  `SystemTime::now` is unsupported (it panics, as on std's other platforms without a wall
  clock); `SystemTime`'s arithmetic works.
- **random:** the core's `random_read`.
- **stdio:** stdout and stderr are lines of the task's log (`moruna_sys::plane::Log`, start slot
  `log`), and stdin is empty. A task with no log slot has its output dropped.
- **env and args:** from the start page: the arguments are the process's name, and the
  environment is empty.
- **fs, net, process, pipe:** std's unsupported implementations: every call answers
  `Unsupported`.

| File | Lines | Changed | SHA-256 in `rust-src` | SHA-256 here |
|---|---|---|---|---|
| `Cargo.lock` | 484 | +26 -0 | `9e87d1ac04edbf5fa61e27cb21984a83566573a007767713868965fba70acb6d` | `4721f4f472771ba8a518dca156c58c1a92df2b612c5a8c1eca144a70a1ab4a69` |
| `std/Cargo.toml` | 176 | +5 -0 | `ef320a0ca2b3bb4d68c3cdeb2fd226ac83d93093bb1c10f4390dcfd8c11f1a9a` | `5ff5f3e4fdf4a742129c90edb418181c353b7a77faf86fb7437aa65909b3176b` |
| `std/build.rs` | 109 | +2 -0 | `f5510c905e126f1de772d73c02cacb7a5ed0a485a22fb99f395593b16a9de8b2` | `b37021b451d7c8538070febf22a2c0cbc554094d081d2ecfab113edca628c49b` |
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
| `std/src/sys/pal/moruna/mod.rs` | 52 | +52 -0 | new | `620d9f0aaf9c8b87c7ac1bda8be3fbe465753fc82a0002bf3f772e7c710e8f5a` |
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
| `std/src/sys/thread/moruna.rs` | 49 | +49 -0 | new | `6c349e214785bf6ccaa640b7dbcb0117543f39742eda9a79fb5a2f79552c2601` |
| `std/src/sys/thread_local/mod.rs` | 231 | +9 -0 | `bc072e04d8564c050420e597b89e2b100502bb1395423277c057ca8aec70317a` | `c18b209709b464353043f33284439d14f2bf2e0eb65fcc6901ee982c50b0d7d0` |
| `std/src/sys/time/mod.rs` | 57 | +4 -0 | `449afdd86955252a302d889032a6cb2cbb04dd786f8cb81d9a9059ad933e7e4c` | `dc5f131f9033808546387962e3c22d24f962204bc06b42123d328775035dbbfa` |

Nothing is removed and no line of `rust-src`'s is changed: the layer only adds. `Cargo.lock`
gains the three Moruna crates std now depends on (`moruna-sys`, `moruna-abi`,
`moruna-record`), as `cargo metadata` writes them.

### Building with it

`kernel/tools/build-std.sh MANIFEST` builds a program for `kernel/targets/x86_64-moruna.json`
with `-Zbuild-std=std,panic_abort`, `__CARGO_TESTS_ONLY_SRC_ROOT` pointing build-std at this
`library/`, and the native programs' link (static, `kernel/sys/user.ld`). The proof is
`kernel/examples/std-arrow`.

### Text only: em dashes

The repository's text rule (no em dash, U+2014, in a tracked file; `tools/quality/check.sh`)
applies to this copy too. In each of these files every em dash of a comment or of prose is
written `--`, and the one file whose em dashes are in string literals (a benchmark's corpus)
writes them as the escape `\u{2014}`, so its strings are the same bytes. A marker line was added
at the top of each. No code changed. A crate under `library/vendor` records the new SHA-256 of
its file in its `.cargo-checksum.json`, which Cargo checks.

| File | SHA-256 in `rust-src` | SHA-256 here |
|---|---|---|
| `alloc/src/ffi/c_str.rs` | `a9a4e636ee388b4c9690ea643f2a6ea1d1f406431fdd5f0cb86fea5c9f465178` | `9e5a7aa2739f48e8c8644503955bb6abc61e5286941fa3a8b91ef54126feb035` |
| `alloc/src/ffi/mod.rs` | `d7bd201345e8fc6c792937e548b7c229975e39180f98f5b2c4e292964c07f375` | `9da82be6ba542226d8c305c2b7477f0b32881f5392a34d8f83c62797f882b077` |
| `core/src/ascii/ascii_char.rs` | `fcf1bfaab18788738efce4b5edf8e08600627bf6d897787a671bd2f27eac3673` | `8a96897c63068a4256cbb93f13877046d9bb72860313990a5155327950f83d9d` |
| `core/src/cell.rs` | `9688204c59a29b83e3f4108f11c5a827dde4c8627e35ce173678c21b9abeb8a7` | `923c2c38806be96565f25b5763be6f3047a6909810453b0e888d9765f9d61422` |
| `core/src/char/methods.rs` | `43651b8f604d20a0a45d58139003a154114acc84e794b5094e72fa6b8db3c945` | `2c6c38d5001ded9cac996898fc7e1a85aade02556133d63b6b9bcfd2a96f7aad` |
| `core/src/io/error.rs` | `461fb8ea5dc086b197f43a362ee6fe3dd5b41ab59f47265b3cdb6086c51327fc` | `c003832217768c0d6771aac554aa6f3f50b6be757e2ddcb7f5461229f2c62ac8` |
| `core/src/mem/mod.rs` | `80a2ca9dc509606b7be5e083e106d5927647444868557190eaac87fbf218ecb7` | `084003df302792acd12ac175f73d4e18ca5e7e16988284139ad29c755c49975e` |
| `core/src/num/mod.rs` | `1978e979505fdeec8e489a4fa6e070fe557c0295e106ee1096a5cb8ffa39fc8e` | `792c0ee5b374b0f8f133bad455714f2549fa369389ba3156d4af69daf9d7cf4d` |
| `core/src/task/wake.rs` | `7c2ceba687944836ea4d42859ec1e191fb14144503257a9cd1ad381f352d1fc1` | `e438a2da17c5aab9b1be41b456541503fe9bb6c7fb441c219efce4e5965abb24` |
| `coretests/benches/str/corpora.rs` | `f275a16e44f7646cb4317f28bd604881dcf845b7103327116548838c2d69b19b` | `c8aca2a9a45410aadda59452c94e643bd7672d598e933f5ffe8b0f110b6f9ed4` |
| `std/src/alloc.rs` | `74400627286d27c36dd2f51bb32433851e54d5a0a2dcd2365ed301c5fd91ee54` | `c749f5f257689718e18993bdbd421f675b024d605245dc71c508af397984b080` |
| `std/src/ffi/mod.rs` | `082280aa251604243b72563ed62915b686c0b47fd1ac70d4070131cc20d393c3` | `9e577e161b8e164947d21cd0775c7b651f2cd80d6c2f82d7e0572d20f677b275` |
| `std/src/process.rs` | `5cab3fe1f471db27db7c3d79d4123613ba0646983f5ad3236bfaaab87f4abaa5` | `109e84264994032eb6bb96323796aa9fe2ddca9cc8052e210cbf3060ea6b04ee` |
| `std/src/sys/net/connection/xous/tcplistener.rs` | `7cccc82e4b741b46939eaf5207c4ccfcea79682c485c8cd41c1431887e2e9a36` | `0bc148588010d6eb2448ec72c9c435918c2c3b173684fe01e33729ca24694489` |
| `std/src/sys/net/connection/xous/udp.rs` | `b9610cd0dc20fc5619cfe9e26c66282df273dc642d42bf311fde68c0fccab191` | `d636a8506b4de8c8a241a3ea8b8183042e09d47d0cee5fd7efe2b326470f8de2` |
| `std/src/sys/pal/uefi/helpers.rs` | `cecd697ccb5cdc3c68b32b3e4ee2a7ce7b1aa657bb6ac36d2825771ae48cf9fc` | `c7474cfc3ac5d82ad171b1e6789223ca67c0b04afd5b666ac3747b4e25a0e7d0` |
| `stdarch/crates/stdarch-gen-common/src/lib.rs` | `ce8ba0b55240debe033e3766fd7ee7e204da98e4ad37bcf6e37123e3df1dba38` | `5d79a4d3cc6ade486eb12e4f174ddb82a5a594633b8c7cda1e1f1117301a3bfc` |
| `vendor/rand-0.9.2/CHANGELOG.md` | `3a56b7876eff2d0dd9d0975844b251236af71e385f413ac06c241e6996038cd7` | `65484dd26377ec5d002076e235482d5aeb9dc20b946df7b954ebe39b43f14436` |
| `vendor/rand_core-0.9.3/CHANGELOG.md` | `01ed6dbeab8975c68fd08e14f3579a964ad7579efa60998e70848cc3b6862647` | `7ab00a6f65b374041c9ad3af940281dd5bcc2984664322784b8526480b5e91fc` |
| `vendor/shlex-1.3.0/src/quoting_warning.md` | `566d6509211ddcd4afbd4f1117c5234567f6b6d01f5da60acfaef011362be045` | `d3b38e8ef4f2748d3fc43785d2e4c1d101eb764c9f7baba2da036535fbe56ca7` |
| `vendor/wasip3-0.6.0+wasi-0.3.0-rc-2026-03-15/src/http_compat/conversions.rs` | `8f5562c525e6bf3036a7293263b3331c8156183c03127d4394f3c4bff52afb8b` | `faf56a542a1939baa803bbf6e8af9c5bb5ffb4d4a8ae0faebab3d3255f642c99` |

The four crates' checksum files changed with them, one hash each:
`vendor/rand-0.9.2/.cargo-checksum.json`, `vendor/rand_core-0.9.3/.cargo-checksum.json`,
`vendor/shlex-1.3.0/.cargo-checksum.json` and
`vendor/wasip3-0.6.0+wasi-0.3.0-rc-2026-03-15/.cargo-checksum.json`.

`.gitignore`, beside this file, keeps three of the copy's directories from the repository's
`target/` and `build/` rules: `vendor/cc-1.2.0/src/target`, and `src/build` and `tests/build` of
`vendor/object-0.39.1`, are source. The checksum files are JSON, which has no comment, so they
carry no marker.
