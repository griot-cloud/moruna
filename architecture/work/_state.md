# Where every open item stands (PM's register)

Updated at every 30-minute checkpoint (Brackly, 2026-11-18): consolidate, update this board, commit, push `feature/arrow-core-0.7` to origin.

Brackly, 2026-11-18 ~18:55 UTC: the PM does the work, with at most two helper agents. Every other item is
the PM's, taken in the order below. Stopped agents' worktrees are deleted. Their branches stay, and any
uncommitted work is saved under `~/moruna-data/wip/<branch>/`:
- `base`: the commit the work sits on;
- `tracked.patch`: `git apply` it on `base`;
- `untracked.tgz`: untar it in the tree's root.

Boot evidence is kept in `~/moruna-data/boots/` (chain logs, scripts, run reports), one folder per boot, after its worktree is deleted.

## Helpers (two)

| Item | Branch | State |
|---|---|---|
| 6.3d the Parquet writers' memory | `work/6.3d-writer-heap` 114ec8c5 on 56cc2040, plus the three decisions in its tree (the tight test passes 10 of 10; the sink's lane race fixed; a decode batch never below MORSEL_MIN) | PM decided: the writers' extra comes only from what the arena's working size (sized with the claims) leaves, never from the arena: a tight run was terminated at the arena's least. Don't relax si_lane_heap; isolate it. A decode batch is never 0 bytes. Then the 672 MiB table and its report | The claim order and the wide read's arena least are built; the 768-column test is no longer ignored. The five crates' suites are running; then commit, the release table and pairs once the load drops, and coverage |
| 6.3g a function's memory measured | phases 1–2 merged (081e145c); phase 3 at 8af13d2c plus the warm-up (uncommitted) | Booted (21:09–21:52Z): 6.3i's KB5 accounts, quota and cpu lines pass; the warm-up works on Score (6.8 s of a 106 s month, other tasks sized from it). Enrich was terminated by staging on its base without 6.3h, 6.3k and 6.3j; it rebases and boots the pair again after run 5's image build, then builds the restart | Rule 1 (the per-instance morsel cap plus the kernel's cut), rule 2 (profiles scaled by count), 6.3i's refused-charge commit, the gate's two Python fixes; then a booted turn |

## The PM's, in order

All six first kernels are merged (4.1–4.6): expr, filter, take, group_aggregate, interval_coverage_count, mlp_forward. What is left of them is measurement (4G.1–4G.4) and their use by the gate functions (functions built on kernels).


Benchmark run 5 is ready (`~/moruna-data/chain-bench5.sh <commit>`): the node image at feature's tip and every engine. Running: the image build took the VM lock at 21:52Z, at b11c1342. Run 4's Linux engines never installed because PyPI timed out; fixed in 0b3ac5bc (uv waits 5 minutes a download and retries 5 times).


| # | Item | Branch, tip | Saved work | Next |
|---|---|---|---|---|
| 1 | 6.3s a Completed run lost a row group | MERGED 56cc2040 (`work/6.3s-rows`) | none | Done: SC-T50, SC-T51; a source closes only when no read waits to be retried and every read taken from its cursor was pushed. 6.3d's 768-column test passes with it |
| 2 | 6.3n push into a closed stage | MERGED with 6.3s (its test is SC-T51) | none | Done: the same root as 6.3s |
| 3 | 6.3r several instances fail on the node | MERGED f7c561e9 (fix aa5cd429) | `sc_t52` (a race ruled out) | Done booted: Score at 4 instances 2 of 2 complete (2,964,624 rows, every batch answered, 0.00 s waiting for room); Enrich, Occupancy, Audit complete. Enrich's run-4 'done without a record' never reproduced; watch for it in run 5 |
| 4 | 6.3h, j, k bytes out in place (the stack) | MERGED 47a393a5 | scratch in wip | Booted check passed (TCG proof boot, 4 CPUs, January): Enrich amplification 2.17 (15.06 in run 3), out/in 1.172; Audit 1.22 and 0.216; Occupancy 1.27 and 0.247. 6.3m (a unit's output is not its own morsel's) has three failing tests at 1ad2c093, not merged, and is still to build |
| 5 | 6.3i the node reports job CPU | `work/6.3i-node-job-cpu` c85452f9 (the last commit is WIP, untested) | none | Mac slot ~4 min: kernel host tests; then the booted turn |
| 5b | 6.3t the drive's fetches run one at a time | MERGED 4c7eb964 (`work/6.3t-concurrent-fetch`) | none | Done on the host: an object's range is cut at column-chunk starts into requests of at most 8 MiB, all issued before any is waited for (SO-T49). Left: measure on the node (Occupancy and Audit from Blob); then make the drive's fetch truly asynchronous, so one group's wait does not hold the next |
| 5c | 6.3u Enrich waits on memory for 46 of its 63 s on the node | PM, after 6.3d merges | none | Run 5 (202611182200), Enrich r0: wall 62.9 s, bottleneck Memory 46.2 s, workers idle ~36 s each, encode 62 s and write 140 s summed over 549 sink writes, 28 files of ~42 MB to Blob (1.17 GB, ~19 MB/s). The output drains too slowly; the report has no upload timings. Next: time the lanes' uploads (multipart parts in flight, part 24 MB) and the file buffers' wait, then find the limit |
| 6 | 6.3l decode cost of small reads | MERGED 0698ea2a (fix ba7225e4) | table in ~/moruna-data/measurements | Done: the year on 4 workers, Parquet at 384 MiB, decode 5504 -> 2800 ms, wall 5511 -> 4631, CPU 18.4 -> 15.6 s. Follow-ups noted: F3 (cut parts on a page start) and fewer, larger reads |
| 7 | 1.11e a woken thread waits a slice | `work/1.11e-wake-waits-a-slice` 4924d5c0 | none | VM turn: rebuild and turn.sh (scripts were in scratch) |
| 8 | 6.3p plane batching | `work/6.3p-plane-batching` 0dfe00b0 (WIP, built by hook only) | none | cargo test plus coverage of moruna-plane; DS-P boots |
| 9 | 6.3o the cost model is dormant on the node | `work/6.3o-cost-model-dormant-on-node` (no commits) | `untracked.tgz` (node_learns.rs), scratch (harness, drive.py, map-notes.txt) | Mapped (map-notes.txt): the node starts the engine with no cost store, so no model, no learning, policy admission, plan basis default; ModelSizer is never built by the runtime. Run 3's node jobs never left a ready unit waiting, so a policy has little to choose at the node's budget; the model's larger effect would be morsel sizing. Next: the harness (~/moruna-data/measure-6.3o) fails to resolve images/gate/functions' inherited deps from outside the repo; build it inside the repo, then measure the policies |
| 10 | 6.3f a Linux image runs one instance per CPU | `work/6.3f-linux-image-runs-alone` 48267e9e (WIP) | none | After 6.3g: a VM turn, the image-kind suite and the table |
| 11 | 4.6 mlp_forward | MERGED (`work/4.6-mlp-forward` 151e1a39) | none | Left: the 20-minute one-core measurement in moruna-arm (`kernel_mlp.py measure`, `score BIN numpy`) |
| 12 | 4.1 + 4.3 catalogue runner, filter, take | MERGED 5577f6d7 (3b4bd4a5) | none | Done: the runner runs every listed implementation over its corpus (filter, take, mlp_forward, interval_coverage_count all pass); coverage 95–96%. Left: the Enrich month check (tests/enrich_month.rs, ignored, needs MORUNA_ENRICH_DIR) and dropping 4.5's stand-in corpus test |
| 13 | 4.2 expr | MERGED 0108239a (58793368) | none | Done: binder, fused evaluator, exact round, temporal parts, legacy filter; 41 cases pass the runner; coverage 95.7%. Fixed: add(duration, timestamp) cast order, and the Audit tree's null minutes (skip_nulls false). Left: the 4G.3 measurement |
| 14 | 4.4 group_aggregate | MERGED 8a4b6ed4 (bba7bb70) | none | Done: hash and dense paths, exact float sums (the superaccumulator's top-digit overflow fixed), 29 cases pass the runner, coverage 94.4%. Left: the 4G.4 counts and the float-sum cost (exact, binned, plain) on a month and on the year |
| 15 | 4.5 interval_coverage_count | MERGED (`work/4.5-interval-coverage-count` a70e3416) | scratch in wip | Done: year compute 53.3 s in Python against 0.85 s; peak 1,217 MiB against 89 MiB; bit for bit. Left: drop its stand-in corpus test once 4.1's runner lands |
| 16 | S.1 dependency advisories | `work/S.1-dependency-advisories` 9b045426 | patch | Read the patch, then build |

