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

The latest private verifier correction reads full failure details from the
canonical terminated report and requires equality with the Exit diagnostic. It
retains Exit 4, the actual kernel-1 task failure, a corresponding error counter,
and the original expected exception text. The event's deliberately generic
message remains unchanged. This correction has source review, but its rebuilt
driver and execution are still pending.

The human explicitly requested consolidation, commit and push on 2026-11-01.
Earlier BOARD restrictions on publication describe previous authorization. This
request authorizes this branch push; it does not authorize release or deployment.
Griot source and its Moruna dependency pin are outside this change.

## Verified results and remaining work

The source-bound [validation ledger](../tools/proof-lab/validation.json) records
the actual runs. The latest strict CPU1 run passed the 100,000-row success case,
then stopped at the second case when the private verifier searched the wrong
diagnostic field. Its failure receipt is preserved. Cases 3 through 5 did not run.
The engine produced the intended Python failure and Exit 4 in case 2; this is
evidence for the diagnostic correction, not a passing acceptance result.

The current work is Phase A only, with a four-hour target ending at 14:54 Nairobi
on 2026-11-01. Progress tables contain only its five active areas.

| Area | Current status | Still needed before deployment |
| ---- | -------------- | ------------------------------ |
| Compiler dependencies | ✅ | None |
| Runtime, SDK, checkpoints/history | 245 ARM tests passed; lint corrections prepared | Recheck lint and finish conformance |
| Node launcher and admission | Launcher and frame tests passed | Pass the 1/2/4-CPU job matrix |
| Combined application | Actual fixtures and verified Python/PyArrow inputs prepared | Execute Linux/native rows, refusals and readback |
| Deployment artifacts | 0.6.0 engine and host tools built | Verify final artifacts and complete release gates |

## Resume sequence

1. Build the corrected SDK driver and run the six existing
   `moruna-event --test records kd3_sink_ack_tests` tests. Preserve the raw log,
   exact source identity and both unstripped and shipped artifact hashes.
2. Start a fresh acceptance output with that actual successful producer. Run the
   five canonical cases in order on 1, 2 and 4 CPUs. Retain strict row counts,
   identity, sink acknowledgement, budget, deadline and cleanup checks. Stop on
   the first failure; do not merge old successes across different source inputs.
3. Complete all 21 cross-adapter cases and 92 required result cells. Native sink
   readback stays in the original admitted scope and reads the actual acknowledged
   objects through canonical source adapters.
4. Run local ARM Linux and x86 QEMU execution checks. No remote KVM server is
   available; do not label emulated results as KVM execution. Complete the canonical
   CI and release gates before publishing 0.6.0.

Canonical repository entry points are `kernel/tools/build.sh`,
`kernel/node/tools/proof.py`, `kernel/node/tools/io-proof.py`, and their existing
configure/package tools. The [proof lab](../tools/proof-lab/README.md) preserves
the exact private orchestration code and its historical input constraints.
Use `tools/quality/check.sh --fast` for the normal pre-commit gate and
`tools/quality/check.sh` for the normal pre-push gate. Publishing this integration
snapshot does not itself mark those execution or deployment gates complete.

## Consolidation checks

Rust formatting, documentation checks, 76 changed Python syntax checks, five
shell syntax checks, stub checks and DCO self-tests passed. The normal commit
hook reached clippy but no compiler started: the shared heavy-work reservation
is held by the running Griot VM. Full compilation, tests, coverage and Python
runtime checks remain pending for this branch. The source whitespace check
passes with unchanged vendored/generated sources and patch fixtures excluded.

The invalid ServedCall-header fixture uses `NOPE` instead of `XXXX` so the
repository stub-marker scan does not mistake test input for unfinished code.
Its expected `CallError::Magic` rejection is unchanged. Two documentation
headings also use the repository's required punctuation.

The human approved: "Push WIP snapshot; defer heavy checks" on 2026-11-01.
This publication uses the existing `MORUNA_SKIP_QUALITY=1` switch for this
commit/push only. Hook configuration and CI are unchanged. The deferred
compilation, runtime and coverage gates above remain outstanding.

The inherited integration history had accidentally tracked 4,458 compiler-output
files under `target-linux/` (about 2.18 GB). The initial upload was stopped before
a remote branch was created. This source-only consolidation omits those files
and their introducing commit; local artifacts and the original integration
branch are preserved. The root ignore rules prevent those outputs being added
again.
