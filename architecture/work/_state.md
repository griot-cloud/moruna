# Where every open item stands (PM's register)

Updated at every 30-minute checkpoint (Brackly, 2026-11-18): consolidate, update this board, commit, push `feature/arrow-core-0.7` to origin.

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
| 6.3g a function's memory measured | phases 1–2 merged (081e145c); phase 3 on `work/6.3g-function-memory-measured` | Rule 1 (the per-instance morsel cap plus the kernel's cut), rule 2 (profiles scaled by count), 6.3i's refused-charge commit, the gate's two Python fixes; then a booted turn |

## The PM's, in order

| # | Item | Branch, tip | Saved work | Next |
|---|---|---|---|---|
| 1 | 6.3s a Completed run lost a row group | MERGED 56cc2040 (`work/6.3s-rows`) | none | Done: SC-T50, SC-T51; a source closes only when no read waits to be retried and every read taken from its cursor was pushed. 6.3d's 768-column test passes with it |
| 2 | 6.3n push into a closed stage | MERGED with 6.3s (its test is SC-T51) | none | Done: the same root as 6.3s |
| 3 | 6.3r several instances fail on the node | `pm/boot-6.3` (feature plus the 6.3h stack) | `sc_t52` (a race ruled out) | Booted 4-CPU repro running. Ruled out: the slot reuse race (`running` covers it), plane event order, and the RB5 retry moving instance. Failure 3 follows failure 2 (the retirement cannot be certified after a task dies mid-stream) |
| 4 | 6.3h, j, k, m bytes out in place (the stack) | `work/6.3j-probe-first-piece` 92541183 | scratch in wip | Its booted check runs in the same boot as 6.3r (`pm/boot-6.3`) |
| 5 | 6.3i the node reports job CPU | `work/6.3i-node-job-cpu` c85452f9 (the last commit is WIP, untested) | none | Mac slot ~4 min: kernel host tests; then the booted turn |
| 5b | 6.3t the drive's fetches run one at a time | PM, `feature` | none | `parquet/read.rs` `fetch_now` blocks on each GET's `.wait()` inside an `async` block, so a source's reads in flight run in turn: the node reads at about 43 MB/s and Occupancy and Audit wait on it. Make the fetch's ranges real futures (`Completion` is a `Future`) |
| 6 | 6.3l decode cost of small reads | `work/6.3l-decode-cost-of-small-reads` ff1af30a (WIP, F1 and F2, builds) | scratch in wip | Run the sources suite under coverage, then the year table. Found: the node's Occupancy and Audit are bound by the drive's one-at-a-time GET (about 43 MB/s), not by decode: a new item |
| 7 | 1.11e a woken thread waits a slice | `work/1.11e-wake-waits-a-slice` 4924d5c0 | none | VM turn: rebuild and turn.sh (scripts were in scratch) |
| 8 | 6.3p plane batching | `work/6.3p-plane-batching` 0dfe00b0 (WIP, built by hook only) | none | cargo test plus coverage of moruna-plane; DS-P boots |
| 9 | 6.3o the cost model is dormant on the node | `work/6.3o-cost-model-dormant-on-node` f56429ff | `untracked.tgz` (node_learns.rs) | Harness; measure the policies |
| 10 | 6.3f a Linux image runs one instance per CPU | `work/6.3f-linux-image-runs-alone` 48267e9e (WIP) | none | After 6.3g: a VM turn, the image-kind suite and the table |
| 11 | 4.6 mlp_forward | MERGED (`work/4.6-mlp-forward` 151e1a39) | none | Left: the 20-minute one-core measurement in moruna-arm (`kernel_mlp.py measure`, `score BIN numpy`) |
| 12 | 4.1 + 4.3 catalogue runner, filter, take | `work/4.1-kernel-catalogue` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then the Enrich month |
| 13 | 4.2 expr | `work/4.2-expr` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then 4G.3 |
| 14 | 4.4 group_aggregate | `work/4.4-group-aggregate` f2e046ad | patch plus tgz (uncompiled) | Compile, test, then 4G.4 |
| 15 | 4.5 interval_coverage_count | MERGED (`work/4.5-interval-coverage-count` a70e3416) | scratch in wip | Done: year compute 53.3 s in Python against 0.85 s; peak 1,217 MiB against 89 MiB; bit for bit. Left: drop its stand-in corpus test once 4.1's runner lands |
| 16 | S.1 dependency advisories | `work/S.1-dependency-advisories` 9b045426 | patch | Read the patch, then build |

