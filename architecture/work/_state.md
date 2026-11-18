# Where every open item stands (PM's register)

Brackly, 2026-11-18 ~18:55 UTC: the PM does the work, with at most two helper agents. Every other item is
the PM's, taken in the order below. Stopped agents' worktrees are deleted. Their branches stay, and any
uncommitted work is saved under `~/moruna-data/wip/<branch>/`:
- `base`: the commit the work sits on;
- `tracked.patch`: `git apply` it on `base`;
- `untracked.tgz`: untar it in the tree's root.

## Helpers (two)

| Item | Branch | State |
|---|---|---|
| 6.3d the Parquet writers' memory | `work/6.3d-writer-heap` (980500b5 plus work in the tree) | Claim order and the wide read's arena least; tests, then commit |
| 6.3g a function's memory measured | `work/6.3g-function-memory-measured` (86501754) | Rule 1 (the per-instance morsel cap plus the kernel's cut) and rule 2 (profiles scaled by count); then the pre-merge turn |

## The PM's, in order

| # | Item | Branch, tip | Saved work | Next |
|---|---|---|---|---|
| 1 | 6.3s a Completed run lost a row group | `work/6.3s-repro` fed1357f (the repro on 6.3d's WIP) | none | Confirm the lead in the item; fix `close_when_exhausted`; add the invariant |
| 2 | 6.3n push into a closed stage | `work/6.3n-push-into-closed-stage` | see wip | Likely the same root as 6.3s |
| 3 | 6.3r several instances fail on the node | `work/6.3r-instances` 7ea41593 (no work) | none | Leads in the item; after 6.3g merges |
| 4 | 6.3h, j, k, m bytes out in place (the stack) | `work/6.3j-probe-first-piece` 92541183 | see wip | The booted check of the four gate jobs (January) |
| 5 | 6.3i the node reports job CPU | `work/6.3i-node-job-cpu` c85452f9 (the last commit is WIP, untested) | none | Mac slot ~4 min: kernel host tests; then the booted turn |
| 6 | 6.3l decode cost of small reads | `work/6.3l-decode-cost-of-small-reads` | see wip | to read |
| 7 | 1.11e a woken thread waits a slice | `work/1.11e-wake-waits-a-slice` 4924d5c0 | none | VM turn: rebuild and turn.sh (scripts were in scratch) |
| 8 | 6.3p plane batching | `work/6.3p-plane-batching` 0dfe00b0 (WIP, built by hook only) | none | cargo test plus coverage of moruna-plane; DS-P boots |
| 9 | 6.3o the cost model is dormant on the node | `work/6.3o-cost-model-dormant-on-node` f56429ff | `untracked.tgz` (node_learns.rs) | Harness; measure the policies |
| 10 | 6.3f a Linux image runs one instance per CPU | `work/6.3f-linux-image-runs-alone` 48267e9e (WIP) | none | After 6.3g: a VM turn, the image-kind suite and the table |
| 11 | 4.6 mlp_forward | `work/4.6-mlp-forward` 151e1a39 (committed, tested) | none | 20 min in moruna-arm: measure on one core |
| 12 | 4.1 + 4.3 catalogue runner, filter, take | `work/4.1-kernel-catalogue` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then the Enrich month |
| 13 | 4.2 expr | `work/4.2-expr` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then 4G.3 |
| 14 | 4.4 group_aggregate | `work/4.4-group-aggregate` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then 4G.4 |
| 15 | 4.5 interval_coverage_count | `work/4.5-interval-coverage-count` | see wip | to read |
| 16 | S.1 dependency advisories | `work/S.1-dependency-advisories` 9b045426 | patch | to read |

"See wip" means the agent was still stopping when this was written; its folder is filled when it stops.
