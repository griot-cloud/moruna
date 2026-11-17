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
  `rust-src` carries, and `library/.cargo/config.toml` points Cargo at them.

Every file not listed below is byte for byte the `rust-src` file. To check:

```sh
diff -r "$(rustc +nightly-2026-07-21 --print sysroot)/lib/rustlib/src/rust/library" library
```

lists exactly the files below.

## What changed

Every change is marked `moruna:` in the file it is in (`grep -rn "moruna:" library`).

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
