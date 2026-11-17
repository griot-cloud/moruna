# OpenVMM crates

Twelve crates from Microsoft's OpenVMM, vendored for the node on Azure (system design
`architecture/moruna-os-system.md`, section 1F; work item
`architecture/work/1.9a-b-vmbus-library.md`). They hold what has no public specification:
Hyper-V's constants and layouts, VMBus's messages and ring buffer, and the NVSP, RNDIS and
storvsc wire types. OpenVMM's drivers are not here: they need std and an async runtime, and
are read as references only.

- **Upstream:** <https://github.com/microsoft/openvmm>, not published on crates.io.
- **Commit:** `e7e7df77b6c7ac377bdebecd5553031dfbd981be`, on `main`.
- **Fetched:** 2026-11-17, by `git clone` and a checkout of that commit. Nothing in the clone
  was built or run.
- **Licence:** MIT. `LICENSE` beside this file is upstream's `LICENSE`, byte for byte. Every
  source file starts "Copyright (c) Microsoft Corporation. Licensed under the MIT License."
  The manifests name no licence, upstream's included, so `kernel/deny.toml` clarifies each
  crate as MIT against that file's hash.
- **Workspace:** each crate is a member of the kernel workspace (`vendor/openvmm/*` in
  `kernel/Cargo.toml`), version `0.0.0` as upstream leaves it, never published.

Every crate builds without std for the kernel's user target
(`kernel/targets/x86_64-moruna-user.json`) and for the host.

## The crates

| Crate | Upstream path | What it holds | Needed by | Files | Lines as fetched | Lines here |
|---|---|---|---|---|---|---|
| `hvdef` | `vm/hv1/hvdef` | Hyper-V's definitions: CPUID leaves, MSRs, hypercall codes, SynIC layouts | named by 1F | 4 | 4991 | 5044 |
| `x86defs` | `vm/x86/x86defs` | x86 definitions: APIC registers, CPUID, MSRs | named by 1F | 10 | 5448 | 5496 |
| `vmbus_core` | `vm/devices/vmbus/vmbus_core` | VMBus's channel messages and protocol versions | named by 1F | 4 | 1283 | 1291 |
| `vmbus_ring` | `vm/devices/vmbus/vmbus_ring` | the ring buffer a channel shares with the host | named by 1F | 3 | 1798 | 1804 |
| `netvsp_protocol` | `vm/devices/net/netvsp_protocol` | NVSP and RNDIS wire types | named by 1F | 4 | 2049 | 2096 |
| `storvsp_protocol` | `vm/devices/storage/storvsp_protocol` | storvsc wire types | named by 1F, forked to drop std | 2 | 209 | 254 |
| `scsi_defs` | `vm/devices/storage/scsi_defs` | SCSI opcodes, status codes and CDB layouts | `storvsp_protocol`; forked to drop std | 3 | 2676 | 2728 |
| `open_enum` | `support/open_enum` | the `open_enum!` macro | `hvdef`, `x86defs`, `vmbus_core`, `netvsp_protocol`, `scsi_defs`, `storvsp_protocol` | 2 | 100 | 147 |
| `guid` | `support/guid` | the `Guid` type and the `guid!` macro | `vmbus_core`, `storvsp_protocol` | 2 | 420 | 457 |
| `guestmem_core` | `vm/vmcore/guestmem_core` | paged ranges of guest memory and their access errors | `vmbus_ring` | 3 | 845 | 892 |
| `safeatomic` | `support/safeatomic` | byte slices read and written as atomics; the one crate here with unsafe code | `vmbus_ring` | 2 | 189 | 237 |
| `net_backend_core` | `vm/devices/net/net_backend_core` | plain descriptions of received and sent packets, offloads and RSS | `netvsp_protocol` | 2 | 425 | 472 |
| **Total** | | | | **41** | **20433** | **20918** |

Lines count every line of the crate's manifest and sources. "As fetched" is upstream's;
"here" is after the changes below.

**Not copied:** `vm/devices/vmbus/vmbus_ring/benches/ring.rs` (155 lines), a benchmark that
needs `criterion`, which needs std.

**Not vendored, by the work item's rule:** `inspect` (OpenVMM's introspection) and
`mesh_protobuf` (its messaging). `vmbus_core`, `vmbus_ring`, `netvsp_protocol` and `guid` use
them only to derive or implement introspection and message encoding, so those derives and
implementations are removed here instead. With them the closure is the twelve crates above;
with them vendored it would have been larger, and would have brought their derive crates.

**New dependencies from crates.io:** `bitfield-struct` 0.11.0 and `static_assertions` 1.1.0.
`zerocopy` 0.8.63 and `thiserror` 2.0.21 were already in `kernel/Cargo.lock`.

## Changes to the manifests

Upstream's manifests cannot be used as they are: each takes its edition, rust-version,
dependency versions and lints from OpenVMM's workspace manifest. All twelve are rewritten,
and every one says so in a `moruna:` comment at its top. What each carries:

- **Package:** the name, `edition = "2024"` and `rust-version = "1.97"` (the workspace's
  values at this commit), and `publish = false` (added).
- **Dependencies between these crates:** `{ path = "../<crate>", version = "=0.0.0" }`.
- **Dependencies from crates.io,** each pinned to one version as the kernel workspace does:
  `bitfield-struct = "=0.11.0"` (upstream asks `0.11.0`, locks 0.11.0),
  `static_assertions = "=1.1.0"` (asks `1.1`, locks 1.1.0),
  `zerocopy = "=0.8.63"` with `derive` (asks `0.8.25` with `derive`, locks 0.8.27), and
  `thiserror = "=2.0.21"` without default features (asks `2` without default features, locks
  2.0.16). The last two are the versions the kernel's lock file already held.
- **Lints:** OpenVMM's `[workspace.lints]`, copied into each manifest in place of
  `[lints] workspace = true`. The sources' `#![expect(missing_docs)]` and
  `#![expect(unsafe_code)]` are written against those levels and warn without them.

Left out of the manifests:

| Crate | Left out | Why |
|---|---|---|
| `vmbus_core` | `inspect`, `mesh_protobuf`; `guid`'s features `inspect` and `mesh` | Not vendored |
| `vmbus_ring` | `inspect` | Not vendored |
| `vmbus_ring` | dev-dependency `criterion`, `[[bench]] ring` | Needs std; the bench is not copied |
| `netvsp_protocol` | `inspect` | Not vendored |
| `guid` | optional `inspect` and `mesh_protobuf`, feature `mesh` | Not vendored |
| `guid` | optional `getrandom`, feature `getrandom` | A random GUID is not needed, and the user target has no `getrandom` backend |
| `guid` | `windows-sys`, `windows`, `win_etw_provider` for `cfg(windows)` | Windows is no target here, and `win_etw_provider` is another OpenVMM crate |
| `x86defs` | optional `arbitrary` | Fuzzing only |
| `x86defs` | dev-dependency `test_with_tracing` | An OpenVMM crate that needs std |
| `storvsp_protocol`, `scsi_defs` | optional `arbitrary`, feature `arbitrary` | Fuzzing only |

Added to the manifests:

- `x86defs`, `storvsp_protocol`, `scsi_defs` and `guid` still test the feature left out
  (`arbitrary`, `getrandom`) in their sources, which are unchanged there. Each manifest's
  `unexpected_cfgs` lint is told that name, so the test is not a warning. The code behind it is
  never compiled.
- `vmbus_ring`: `chunks_exact_to_as_chunks = "allow"`. The kernel's clippy (0.1.99) is newer
  than upstream's (1.97) and has this lint, which fires on two unchanged lines of
  `PagedRingMem` in `src/lib.rs`.

## Changes to the sources

Every changed or added line carries a `moruna:` comment, and a removed line leaves one where
it was: `grep -rn "moruna:" kernel/vendor/openvmm` lists them all. 20 of the 29 source files
are unchanged. Line numbers are this tree's.

| File | Lines | Change |
|---|---|---|
| `storvsp_protocol/src/lib.rs` | 9 | **The fork:** `#![no_std]` added |
| `storvsp_protocol/src/lib.rs` | 13 | **The fork:** `use std::fmt::Debug;` is now `use core::fmt::Debug;`, moved up four lines to where rustfmt sorts it |
| `scsi_defs/src/lib.rs` | 23 | **The fork:** `#![no_std]` added |
| `scsi_defs/src/lib.rs` | 1800-1804 | **The fork:** `impl TryFrom<usize> for FeatureNumber` has `type Error = core::convert::Infallible` where it had `std::io::Error`. It never returned an error. `#[expect(clippy::infallible_try_from)]` is added above it, because clippy asks for `From` once the type says so |
| `scsi_defs/src/lib.rs` | 11-12, 17-18 | An em dash in each of two doc lines is now a colon |
| `hvdef/src/save_restore.rs` | 100-101, 116-117, 153-154, 235-236, 321-322 | An em dash in each of five doc lines is now a colon |
| `x86defs/src/snp.rs` | 1524-1525 | `use test_with_tracing::test;` removed from the test module; its tests run under the built-in `#[test]` |
| `guid/src/lib.rs` | 22-23 | The `mesh_protobuf::Protobuf` and `inspect::Inspect` derives on `Guid`, each behind a feature, removed (6 lines) |
| `guid/src/lib.rs` | 26, 28, 30, 32 | The four `#[cfg_attr(feature = "mesh", mesh(n))]` field attributes removed |
| `vmbus_core/src/lib.rs` | 16 | `use inspect::Inspect;` removed |
| `vmbus_core/src/lib.rs` | 30, 141 | `Inspect` removed from the derives of `VersionInfo` and `HvsockConnectRequest` |
| `vmbus_core/src/protocol.rs` | 13-14 | `use inspect::Inspect;` and `use mesh_protobuf::Protobuf;` removed |
| `vmbus_core/src/protocol.rs` | 153, 511, 591 | `#[derive(Inspect)]` removed from `FeatureFlags`, `OfferFlags` and `PipeFlags` |
| `vmbus_core/src/protocol.rs` | 244-246 | `Protobuf` and `#[mesh(package = "vmbus")]` removed from `GpadlId` |
| `vmbus_core/src/protocol.rs` | 255, 264-266 | `Inspect`, `Protobuf` and `#[inspect(transparent)]` removed from `ChannelId` |
| `vmbus_core/src/protocol.rs` | 320 | `Inspect` removed from the derive of `Version` |
| `vmbus_core/src/protocol.rs` | 403-406 | `Protobuf`, `Inspect`, `#[mesh(transparent)]` and `#[inspect(transparent)]` removed from `UserDefinedData`; its derive now fits one line |
| `vmbus_core/src/protocol.rs` | 488-494 | `Inspect`, `#[inspect(extra = "Self::inspect_extra")]` and `#[inspect(skip)]` removed from `OfferChannel`; its derive now fits one line |
| `vmbus_core/src/protocol.rs` | 508-509 | `impl OfferChannel { fn inspect_extra }` removed (37 lines): a private function only the derive called |
| `vmbus_core/src/protocol.rs` | 513-514, 593-594 | `Protobuf` and `#[mesh(transparent)]` removed from `OfferFlags` and `PipeFlags` |
| `vmbus_core/src/protocol.rs` | 552-555 | `Protobuf`, `Inspect`, `#[mesh(transparent)]` and `#[inspect(transparent)]` removed from `PipeUserDefinedData` |
| `vmbus_ring/src/lib.rs` | 39, 80-81 | `use inspect::Inspect;` removed twice, and `use core::sync::atomic::Ordering;` from `mod protocol`, where only the removed implementation used it |
| `vmbus_ring/src/lib.rs` | 130 | `impl Inspect for Control<'_>` removed (16 lines) |
| `vmbus_ring/src/lib.rs` | 805, 971 | `impl Inspect` for `IncomingRing<M>` and for `OutgoingRing<M>` removed (5 lines each) |
| `vmbus_ring/src/lib.rs` | 808-814, 974-980 | `Inspect` and three `#[inspect(hex)]` removed from each of `IncomingOffset` and `OutgoingOffset` |
| `vmbus_ring/src/lib.rs` | 1216-1218 | `impl Inspect for InnerRing<M>` removed (7 lines), and the public function `inspect_ring` (10 lines with its documentation), whose argument was an `inspect::Response` |
| `netvsp_protocol/src/protocol.rs` | 6 | `use inspect::Inspect;` removed |
| `netvsp_protocol/src/protocol.rs` | 18 | `Inspect` removed from the derive of `Version` |
| `netvsp_protocol/src/protocol.rs` | 415-433 | `#[derive(Inspect)]` and eight `#[inspect(safe)]` removed from `NdisConfigCapabilities` |

What the changes take away from the crates' interfaces: the `Inspect` and `Protobuf`
implementations of the types above; `vmbus_ring::inspect_ring`; the features `mesh`, `inspect`
and `getrandom` of `guid` (and so `Guid::new_random`); the feature `arbitrary` of `x86defs`,
`storvsp_protocol` and `scsi_defs`. What they change: the error type of
`scsi_defs::FeatureNumber`'s `TryFrom<usize>`. No layout, constant or wire format changes.

## Upstream's tests

Upstream's unit tests run on the host with the rest of the crate
(`cargo test --target <host> -p <crate>` in `kernel/`): 28 in `x86defs`, 6 in `vmbus_ring`,
3 in `vmbus_core`, 3 in `guid`, 1 in `guestmem_core`, and the doc test of `open_enum`. The
other crates have none.

## SHA-256 of every file as fetched

The hash is of upstream's bytes at the commit above. For a file marked unchanged,
`shasum -a 256` of the file here gives the same value.

| File here | Upstream path | SHA-256 as fetched | Here |
|---|---|---|---|
| `LICENSE` | `LICENSE` | `c2cfccb812fe482101a8f04597dfc5a9991a6b2748266c47ac91b6a5aae15383` | unchanged |
| `guestmem_core/Cargo.toml` | `vm/vmcore/guestmem_core/Cargo.toml` | `5d04e98b1d25693d988364bf0fc9c30c62f79a71f7b5a076ef678526da96a50a` | rewritten |
| `guestmem_core/src/lib.rs` | `vm/vmcore/guestmem_core/src/lib.rs` | `c925708ef3ee8013884c6beb36f6ae1ee7d40084f8396b420640f72344a924cc` | unchanged |
| `guestmem_core/src/ranges.rs` | `vm/vmcore/guestmem_core/src/ranges.rs` | `b4b54ef7185847785eaa9c65419d423094b233c911219847423476f40c7abb20` | unchanged |
| `guid/Cargo.toml` | `support/guid/Cargo.toml` | `1dc69d77e9a4ba1258157a485aaa74a08b18b81b6c2642c6635af2960147af6a` | rewritten |
| `guid/src/lib.rs` | `support/guid/src/lib.rs` | `339a85a5c756afae5b4423e67cfd6a74b86561bc951aaab1bb6f34a4e1f4f043` | changed |
| `hvdef/Cargo.toml` | `vm/hv1/hvdef/Cargo.toml` | `a5b348588f35c6de7db03a7b773b2914e07e0eb18a529d0df64432420727e647` | rewritten |
| `hvdef/src/lib.rs` | `vm/hv1/hvdef/src/lib.rs` | `804a05ad192e1280abf0cf156bf572d47f371c718e1cb28efc02537db4400ebd` | unchanged |
| `hvdef/src/save_restore.rs` | `vm/hv1/hvdef/src/save_restore.rs` | `4a52b3db3ee6373bad5ba243688220770afa7be942a8e71c15c1bddbb919a60b` | changed |
| `hvdef/src/vbs.rs` | `vm/hv1/hvdef/src/vbs.rs` | `4c9352170165ee5006d1fa22a9cbcc200156e1992c0e5464de7cd1a7e6c93242` | unchanged |
| `net_backend_core/Cargo.toml` | `vm/devices/net/net_backend_core/Cargo.toml` | `22e05c08c2bc951475f1f11de41cc097c3690f592583812cd0a678880716a455` | rewritten |
| `net_backend_core/src/lib.rs` | `vm/devices/net/net_backend_core/src/lib.rs` | `07f804d2b907e6e2fe367b47e9b286e27f966339ac7b508af0c7d48732d81e6a` | unchanged |
| `netvsp_protocol/Cargo.toml` | `vm/devices/net/netvsp_protocol/Cargo.toml` | `ccd883f128e7fc42bfcd17798ce3f87f9abc5135cba287a4edc7c905d2275480` | rewritten |
| `netvsp_protocol/src/lib.rs` | `vm/devices/net/netvsp_protocol/src/lib.rs` | `d40b3e2365327fd1f9f5bee1b96cf3e1ca1696dbe15b643eb561f4d9a620b0e5` | unchanged |
| `netvsp_protocol/src/protocol.rs` | `vm/devices/net/netvsp_protocol/src/protocol.rs` | `d60ea4e66a064f17200faf65d02c1e024721f0b240ab0e392a5db63682fadaf5` | changed |
| `netvsp_protocol/src/rndisprot.rs` | `vm/devices/net/netvsp_protocol/src/rndisprot.rs` | `33f268e090ca6fc1bf218352d48710e696c2a5e59d3d9e593dda34ca1567f1db` | unchanged |
| `open_enum/Cargo.toml` | `support/open_enum/Cargo.toml` | `95c413647af26643484fe01846aa5522cdc29937b2cc2a075a3adcd06996a485` | rewritten |
| `open_enum/src/lib.rs` | `support/open_enum/src/lib.rs` | `0c419c25b03b50f930a0b802e2dac9e2e9aa23d0051cd447ed367d247360a9ce` | unchanged |
| `safeatomic/Cargo.toml` | `support/safeatomic/Cargo.toml` | `f79c02011011fcc110efa23d45bcb25518e5978e845d42d9f971655176a37b84` | rewritten |
| `safeatomic/src/lib.rs` | `support/safeatomic/src/lib.rs` | `b1c3381dc77675f68ddbec610a0cd18417c7c0775a72d75fe99364faa02beaa3` | unchanged |
| `scsi_defs/Cargo.toml` | `vm/devices/storage/scsi_defs/Cargo.toml` | `fabdb274f00bcf8104c1e7e39c70e1146e3d5538eac070409a5b986ab25c6742` | rewritten |
| `scsi_defs/src/lib.rs` | `vm/devices/storage/scsi_defs/src/lib.rs` | `d61376d99acf813c41a9db61be556b1eee5d39d9fc0ec5b2998226e60f406c54` | changed |
| `scsi_defs/src/srb.rs` | `vm/devices/storage/scsi_defs/src/srb.rs` | `ba1b23403395e5be315aa486bb03c7456cb14ca390afa62e32da177f1d70ebd7` | unchanged |
| `storvsp_protocol/Cargo.toml` | `vm/devices/storage/storvsp_protocol/Cargo.toml` | `366359ec2d88ed13c01df4e647d07dc841a36611292bcdd93b6b592c136bac49` | rewritten |
| `storvsp_protocol/src/lib.rs` | `vm/devices/storage/storvsp_protocol/src/lib.rs` | `116f9d3d7ed634140a7a5886108999de4844610a9a6aa704304b8ba4824706be` | changed |
| `vmbus_core/Cargo.toml` | `vm/devices/vmbus/vmbus_core/Cargo.toml` | `19f56ba6cf8b08cc5db77e5c567f7dc2d33433e6f3ae56be8faff6f07c4c544e` | rewritten |
| `vmbus_core/src/lib.rs` | `vm/devices/vmbus/vmbus_core/src/lib.rs` | `25a2f5bc5d50a58fe49f750d7a38ccd249851e71476643e49d5722de5ccb6e10` | changed |
| `vmbus_core/src/protocol.rs` | `vm/devices/vmbus/vmbus_core/src/protocol.rs` | `4bbd6cb08e3619ac05286e19e40f89d02fc75628ca6677531ecf99ee8fe70617` | changed |
| `vmbus_core/src/protocol/macros.rs` | `vm/devices/vmbus/vmbus_core/src/protocol/macros.rs` | `53c0c975f88e13226b4e3c6bfaf03fd5ba040029413c57e0f8112fcda6203ef0` | unchanged |
| `vmbus_ring/Cargo.toml` | `vm/devices/vmbus/vmbus_ring/Cargo.toml` | `1c91d62bb985b9a8ceb458c6c28635294ce23529aee4d7d420d4f80600bd02df` | rewritten |
| `vmbus_ring/src/gparange.rs` | `vm/devices/vmbus/vmbus_ring/src/gparange.rs` | `38870fa61d1be6ef46c43c1e00817bbd3dfc717ae29fc696226887aca542d1e0` | unchanged |
| `vmbus_ring/src/lib.rs` | `vm/devices/vmbus/vmbus_ring/src/lib.rs` | `7f2b8877204c48b49593d19910ffe8571259e0d0c37009a270169ee268b58a01` | changed |
| `x86defs/Cargo.toml` | `vm/x86/x86defs/Cargo.toml` | `20a42d28cb8f01aca8cf2bd2e4b33f730c4a1907e586400cd4bf42ec0320ee77` | rewritten |
| `x86defs/src/apic.rs` | `vm/x86/x86defs/src/apic.rs` | `f612ba4b6091db21d86c7f2e363a70e8e715ce8ea67aaa4c901366d5b2141a8b` | unchanged |
| `x86defs/src/cpuid.rs` | `vm/x86/x86defs/src/cpuid.rs` | `c6f2691bd5d489db713734568ee0083cb97b912f190c9ac30c6073d4db1c8ff5` | unchanged |
| `x86defs/src/lib.rs` | `vm/x86/x86defs/src/lib.rs` | `a9e6ce60f7a360c6f565bd73a3ea36172b2c29f482f3af1e2dfc3202b1246171` | unchanged |
| `x86defs/src/msi.rs` | `vm/x86/x86defs/src/msi.rs` | `f6b1345d83f888353c122a9fc2e2e2622929636b122dee4da522cd73f11a07e4` | unchanged |
| `x86defs/src/serial.rs` | `vm/x86/x86defs/src/serial.rs` | `232f9bf2a98daec986aae39264819c742bc1ee29c5de321335bc769710e849d6` | unchanged |
| `x86defs/src/snp.rs` | `vm/x86/x86defs/src/snp.rs` | `7de677060285ae57e1c1921d26e7f6b7f6ca34255bdb409a176c0ba770d4dfbc` | changed |
| `x86defs/src/tdx.rs` | `vm/x86/x86defs/src/tdx.rs` | `7a44b7e1223ad6869e4018c845ee58c61161d84b3958a2415d38a713fcb74e25` | unchanged |
| `x86defs/src/vmx.rs` | `vm/x86/x86defs/src/vmx.rs` | `e9994a6b6b0fc645bb1d839112c786cfe04e5581558e8917dfb8635e00984993` | unchanged |
| `x86defs/src/xsave.rs` | `vm/x86/x86defs/src/xsave.rs` | `b5d014e0a61cb5dff9e35e258c72563366b28c78f2ff15ed0c6dc553408a6f25` | unchanged |

## Taking a newer commit

Clone upstream outside the repository, check out the commit, and copy each crate's `src/`
over the one here. Then apply the changes above again: `git diff` against this tree shows each
one beside its `moruna:` comment. Read upstream's workspace manifest for changed dependency
versions and lints, update this file's commit, tables and hashes, and the hash in
`kernel/deny.toml` if `LICENSE` changed. The checks are the work item's: every crate builds
for the host and for the user target, upstream's tests pass, and
`cargo deny --manifest-path kernel/Cargo.toml check licenses bans sources` passes.
