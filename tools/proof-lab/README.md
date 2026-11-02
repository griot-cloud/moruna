# Moruna proof lab

This directory preserves the offline Apple Container proof tools used to assemble
and check the Moruna microkernel integration. They were previously in a temporary
workspace. Product source lives in the normal `crates/` and `kernel/` trees.

Start with [the integration handoff](../../architecture/moruna-integration.md) and
[the source-bound validation ledger](validation.json). The ledger distinguishes
compiled artifacts, actual execution, failures and work that has not run.

## Entry points

- `queue-proof.py`: one bounded, explicitly requested producer using an owned VM
  and the shared reservation. `combined-linux.py` implements the selected runtime
  and SDK-driver producers.
- `node-acceptance.py`: checks actual producer receipts, selects existing artifacts
  and invokes the canonical `kernel/node/tools/proof.py` and `io-proof.py` tools.
  Without `--run`, it prepares and validates inputs only.
- `cache-retarget.py`: checks source identity and prior cleanup before rebinding
  an unmounted owned compiler cache. It requires a subsequent authored rebuild.
- The remaining scripts preserve dependency preparation, source assembly,
  packaging, native linking and resource-management history. Read a script before
  invoking it; some historical preparation scripts execute at module scope.

## Artifact workspace

These are lab tools, not a turnkey installer. They expect a private, owner-only
artifact workspace alongside the scripts. Existing receipts contain original
absolute paths, source hashes and historical run identities. Copying scripts does
not recreate the artifacts or authorize a historical receipt to certify new
source. For resuming the original local proof, use the artifact-store location in
`validation.json`; preserve its original source and ownership checks.

For a new machine, use the canonical repository build and node proof entry points
first. Prepare fresh explicit inputs and receipts before using this lab adapter.
Never copy old success flags onto new source or rewrite evidence to make it pass.

Large images, compiler caches, dependency archives and raw row/event captures are
deliberately excluded from Git. No credentials are needed to read this directory.
`script-origins.json` records the captured scripts' original hashes. The tracked
adapter also includes the separately reviewed one-file diagnostic-delta check;
that change and any text normalization can differ from the captured hash.

KVM remains mandatory for production Linux launch. The explicit TCG proof feature
is a correctness-test seam and does not establish native deployment readiness.
