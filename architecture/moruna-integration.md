# Moruna integration handoff

## Consolidated source

The working integration is on `codex/moruna-consolidated-20261101`. Source was
assembled against `38447a99447151aec77eb28272a8a32e682052f3` from
`integration/v1`. The published commit uses the clean source ancestor
`67c0787046c8b2d1fce7e17cb194a1a6dd49a6e2` as its parent, excluding an
inherited compiler-output commit while retaining all subsequent source fixes.
It brings the
assembled feature sources and subsequent compiler/runtime corrections into this
Moruna repository. It also preserves the existing BOARD notes, scheduler
specification amendment and schema-bearing empty-output regression.

The source includes the native launcher and witness lifecycle, canonical events,
runtime checkpoint/history and task-output plumbing, shared storage format and
native service/client, network/node services, Python builder and SDK integration,
Account-based memory sampling and controller admission, and the strict node proof
entry points. The pinned ring sources under `kernel/vendor/` are build inputs,
including their upstream test fixtures; they are not compiler output.

## Verified results and remaining work

The source-bound [validation ledger](../tools/proof-lab/validation.json) preserves
historical failures separately from later passing runs. The ordered-sink
termination fix cancels queued writes that have not entered the underlying sink;
it retains submitted I/O until its original completion. Three focused regressions
passed. All 15 canonical SDK cases passed on 1, 2 and 4 CPUs using the normal
engine, including propagation of the original Python exception and Exit 4.
The ten image-sandbox tests also passed with actual namespace isolation.

The iterator defect is corrected: streaming sources read through actual end of
iteration, preserve empty batches and propagate later errors. Subsequent fixes
include shared frame-pool coalescing, bounded Linux service fairness, reuse of
read-only file backing, correct readlink errors, and strict host receipt framing.
Current native producer `3473d1cecd0f` has 52 verified artifacts. The generic
native regression passed in `4e414311af8d`. Runtime source remains `501ac3be`;
its normal producer `b33342ce1858` has 17 verified artifacts. All 15 canonical
SDK cases passed with that producer, on the earlier imagefs 256 MiB profile.
The genuine public-builder smoke passed in `549620ddce57` with all original
100,000-row, relative-asset and one-CPU assertions.

GNU release producer `3b15a968d00d` completed its actual guest build. Its original
collector error remains recorded; independent verification `d594eff2` checks all
retained artifacts without rewriting that failure. Release Linux comparison
passed all 41 cases and 46 receipts (`PLAN 0c394`). Native PEX passed with the
provisioned imagefs capacity (`89d`). Native cells 0 through 35 passed under that
profile. Cell 36 exposed a refusal-reporting fixture mismatch, corrected in
`d7670ad2`; corrected cells 36 through 40 now passed. The unchanged canonical
comparator exited zero over all 92 actual results across 21 cases, including
every sink readback (`kd3-scoped-actual-comparison-2tm752pc`). This is an
explicit source-bound multi-run comparison, not a single-run journal flag.
SDK15 also passed all CPU1/2/4 cases on the 768 MiB imagefs profile, with
actual successful cleanup on all three profile turns. The current 3ab free-threaded installed wheel passed 204 tests with 11 skips
in 68.28 seconds. Its actual test command exited zero and verified the three
new getters on the extension installed inside the fresh virtual environment.
The outer wrapper subsequently failed only while parsing results (`re` was not
imported); the original failed report and collector record remain unchanged.
The separate functional receipt records this distinction, successful cleanup
and restored linker state.

The tested node has 3 GiB, with imagefs provisioned at 768 MiB for simultaneous
parent/child ELF and Python DSO backing. The job remains 1 GiB, its account
1408 MiB, scratch 256 MiB and admission 1536 MiB. Auxiliary accounts total
2880 MiB; configure and supervisor still enforce actual boot-room and carve
checks. This proof-profile change does not raise a job's budget.

Coverage measurement is explicitly excluded by the user; no coverage percentage
or coverage-gate pass is claimed. Version 0.6.0 remains unreleased. Scope is Phase A.

| Area | Current status | Still needed before deployment |
| ---- | -------------- | ------------------------------ |
| Compiler dependencies | ✅ | None |
| Runtime, SDK, checkpoints/history | Termination regressions and SDK cases passed | Installed wheel passed; coverage excluded by user |
| Node launcher and admission | ✅ | None |
| Combined application | Image sandbox tests passed; iterator delivery corrected | 92-result scoped comparison and installed wheel passed |
| Deployment artifacts | Normal engine and tools built; 0.6.0 unreleased | Verify final affected artifacts and complete release gates |

## Remaining acceptance

1. Preserve the source-bound SDK, builder, generic boot and KD3 receipts, including
   the explicit multi-run 92-result provenance and original failed attempts.
2. Review final documentation and complete the existing CI/release sequence.
   Publication remains pending; no release tag or package publication is implied
   by this local validation record.

Local acceptance uses ARM Linux and x86 QEMU TCG. KVM was not run. Native
execution uses source-built descriptors and artifacts, not the legacy Linux OCI
launcher. Supported local OCI inputs do not imply automatic registry push or pull.

Canonical repository entry points are `kernel/tools/build.sh`,
`kernel/node/tools/proof.py`, `kernel/node/tools/io-proof.py`, and their existing
configure/package tools. The [proof lab](../tools/proof-lab/README.md) preserves
historical orchestration and input constraints. Native execution uses descriptors and artifacts built from source; the legacy
Linux OCI guest is not the native launcher. Supported runnables and local OCI
inputs do not imply automatic registry push or pull.
