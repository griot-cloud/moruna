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

| Area | Current status | Still needed before deployment |
| ---- | -------------- | ------------------------------ |
| Compiler dependencies | ✅ | None |
| Runtime, SDK, checkpoints/history | Six verifier tests and first SDK case passed; diagnostic correction reviewed | Rebuild corrected verifier and complete SDK cases |
| Node launcher and admission | Test workload and owned cleanup exercised | Complete admission cases and native KVM staging |
| Secure networking | Certificate bundle accepted at startup | Execute connection and refusal checks |
| Durable storage and file operations | Storage code compiled; NVMe device starts | Execute durable write and recovery checks |
| Combined application | 100,000-row workload passes on CPU1 | Finish CPU1/2/4 cases and verify Griot on Moruna |
| Deployment artifacts | Engine and image built; source consolidated | Finish checks and staging deployment |

## Resume sequence

1. Build the corrected SDK driver and run the six existing
   `moruna-event --test records kd3_sink_ack_tests` tests. Preserve the raw log,
   exact source identity and both unstripped and shipped artifact hashes.
2. Start a fresh acceptance output with that actual successful producer. Run the
   five canonical cases in order on 1, 2 and 4 CPUs. Retain strict row counts,
   identity, sink acknowledgement, budget, deadline and cleanup checks. Stop on
   the first failure; do not merge old successes across different source inputs.
3. Execute the separate native network/storage proof: real connection/refusal
   behavior and durable write/recovery over two boots of the same home image.
4. Exercise a real Linux x86 KVM staging host, then Griot's actual application,
   persistent data and restart paths. An emulated workload pass does not certify
   either of these steps.

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
