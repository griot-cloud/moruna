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
| 6.3g a function's memory measured | MERGED 13bb3750 (phases 1–3, with 6.3i's two commits) | Done, booted: Score and Enrich complete with the warm-up (4.9 s of 96 s); gate-grows restarts past its room with every row once; 6.3i's KB5 lines pass. Left for 6.3f: the Linux ImageKernel's slicing at the cap, a real Python image function at 4 instances |

## The PM's, in order

Run 6 next: the node alone at feature's tip (the asynchronous fetch, 6.3g, 6.3r's gap fix), its table merged with run 5's Linux engines.

Benchmark run 5 so far (results 202611182200, image b11c1342): node Score 55–57 s 6/6, Occupancy 15.8–17.6 s, Enrich 63–74 s 6/6, Audit 17.4–17.9 s 4/6 (2 failed on 6.3r's gap). The Linux engines are installing and running this time (0.6.6's Rust Score 30.3 s, 0.6.6 Python 60.1 s, Dask 78.8 s). Harness faults to fix before the next full run: Ray fails every job with `ImportError: cannot import name 'HTTPFileSystem' from 'fsspec.implementations.http'` (fsspec too new for ray 2.60; pin it in install.py); Dask's Enrich r0 ended KilledWorker.


All six first kernels are merged (4.1–4.6): expr, filter, take, group_aggregate, interval_coverage_count, mlp_forward. What is left of them is measurement (4G.1–4G.4) and their use by the gate functions (functions built on kernels).


Benchmark run 5 is ready (`~/moruna-data/chain-bench5.sh <commit>`): the node image at feature's tip and every engine. Running: the image build took the VM lock at 21:52Z, at b11c1342. Run 4's Linux engines never installed because PyPI timed out; fixed in 0b3ac5bc (uv waits 5 minutes a download and retries 5 times).


| # | Item | Branch, tip | Saved work | Next |
|---|---|---|---|---|
| 1 | 6.3s a Completed run lost a row group | MERGED 56cc2040 (`work/6.3s-rows`) | none | Done: SC-T50, SC-T51; a source closes only when no read waits to be retried and every read taken from its cursor was pushed. 6.3d's 768-column test passes with it |
| 2 | 6.3n push into a closed stage | MERGED with 6.3s (its test is SC-T51) | none | Done: the same root as 6.3s |
| 3 | 6.3r several instances fail on the node | MERGED f7c561e9 and the gap fix 75347b85 (dc352acc) | boots in ~/moruna-data/boots | Done, booted: Score 2/2, Audit 2/2, Enrich and Occupancy complete with step() counting outstanding(). Run 6 on the node confirms |
| 4 | 6.3h, j, k bytes out in place (the stack) | MERGED 47a393a5 | scratch in wip | Booted check passed (TCG proof boot, 4 CPUs, January): Enrich amplification 2.17 (15.06 in run 3), out/in 1.172; Audit 1.22 and 0.216; Occupancy 1.27 and 0.247. 6.3m (a unit's output is not its own morsel's) has three failing tests at 1ad2c093, not merged, and is still to build |
| 4b | 6.3d the Parquet writers' memory | MERGED f2814aa1 (070d4963, fc149aaa) | data in wip | Done: the writers stay within their claim at every budget, output 2–7% smaller, rows and hashes identical; slices take the lane's room; claims count inside the out-of-arena share; the sink's lane race fixed. Left: ~3% at 1 GiB on 4 workers and 672 MiB on 4 workers, where the writers stay near their claim and flush row groups by their heap (the note says so) |
| 5 | 6.3i the node reports job CPU | MERGED with 6.3g (c9627493, 8b65a548) | none | Done: booted, KB5 accounts, quota and cpu |
| 5b | 6.3t the drive's fetches run one at a time | MERGED 4c7eb964 (requests in flight) and a906df5d (asynchronous fetch, ranges share a group's fetch) | none | Done on the host: SO-T49 to SO-T52. Run 5 (image b11c1342, before the asynchronous fetch): Occupancy 15.8–17.6 s from 18.6. Left: measure the asynchronous fetch on the node (run 6) |
| 5c | 6.3u Enrich waits on memory for 46 of its 63 s on the node | PM | none | Run 5, Enrich r0: wall 62.9 s, Memory 46.2 s, workers idle ~36 s. The sink's put awaits each rolled file's upload: 28 files of ~42 MB, 140 s of summed waits, ~5 s a file, ~8 MB/s an upload, ~32 MB/s over 4 lanes, which is the 46 s. Each upload has 2 parts in flight (part = room / requests = 24 MB). On Linux the same output goes up in 18.5 s, so Blob is not the limit. Candidates on the node's path: the part size for uploads (5 MiB parts would put ~8 in flight), and the per-connection upload rate (TCP send buffer 256 KiB, Nagle on, the engine's writes through moruna.net). Merged 3ad236b9 (timing): the report now notes each read's and write's MB/s; run 6 gives the node's figures. Then: 5 MiB parts and a 4 MiB send buffer (a `setting` line), measured the same way |
| 6 | 6.3l decode cost of small reads | MERGED 0698ea2a (fix ba7225e4) | table in ~/moruna-data/measurements | Done: the year on 4 workers, Parquet at 384 MiB, decode 5504 -> 2800 ms, wall 5511 -> 4631, CPU 18.4 -> 15.6 s. Follow-ups noted: F3 (cut parts on a page start) and fewer, larger reads |
| 7 | 1.11e a woken thread waits a slice (helper with 6.3p since 04:45) | `work/1.11e-wake-waits-a-slice` c897b0ae (rebased on 36c5b055) | evidence in ~/moruna-data/boots/agent-a648-1.11e-turn | Booted: no measurable gain or loss on Score and Enrich months at 1 and 4 CPUs (so Score's node idle is not the slice wait); KB3 passes in all six boots. Alone it fails DS-P steady (6.3p's regression), so it merges with 6.3p after 6.3p's VM turn |
| 8 | 6.3p plane batching (with 1.11e) | `work/6.3p-plane-batching` be72cff4 on 1.11e | none | Host done (32 tests, 98.6% of its lines). Booted: DS-P steady still fails under TCG (a sleeping task woken once a post, 208–215 for 256; the engine defers only while the task lingers). PM: no fixed ms cap; try (d), the engine defers waking a sleeping task until GROUP posts, its own block or return, or GROUP × the task's measured posting interval. Measure first |
| 9 | 6.3o the cost model is dormant on the node | `work/6.3o-cost-model-dormant-on-node` (no commits) | `untracked.tgz` (node_learns.rs), scratch (harness, drive.py, map-notes.txt) | Mapped (map-notes.txt): the node starts the engine with no cost store, so no model, no learning, policy admission, plan basis default; ModelSizer is never built by the runtime. Run 3's node jobs never left a ready unit waiting, so a policy has little to choose at the node's budget; the model's larger effect would be morsel sizing. Next: the harness (~/moruna-data/measure-6.3o) fails to resolve images/gate/functions' inherited deps from outside the repo; build it inside the repo, then measure the policies |
| 10 | 6.3f a Linux image runs one instance per CPU | MERGED b7ffda21 (4d4038e8) | turn2 evidence in wip | Done: Score (real Python image, January, 4 CPUs) 7.86 s -> 4.47 s, every row equal; the warm-up on child memory.peak; the cut at the cap; MADV_DONTFORK on writable windows. The node-side shared floor has had the x86_64 clippy only: run 6 compiles and runs it. Found: a warm-up's peak only rises (memory.peak, and likely the node's account peak), so a small piece after a large one reads the larger; it errs safe. Resetting the peak per piece (Linux 6.12+) is to decide |
| 11 | 4.6 mlp_forward | MERGED (`work/4.6-mlp-forward` 151e1a39) | none | Left: the 20-minute one-core measurement in moruna-arm (`kernel_mlp.py measure`, `score BIN numpy`) |
| 12 | 4.1 + 4.3 catalogue runner, filter, take | MERGED 5577f6d7 (3b4bd4a5) | none | Done: the runner runs every listed implementation over its corpus (filter, take, mlp_forward, interval_coverage_count all pass); coverage 95–96%. Left: the Enrich month check (tests/enrich_month.rs, ignored, needs MORUNA_ENRICH_DIR) and dropping 4.5's stand-in corpus test |
| 13 | 4.2 expr | MERGED 0108239a (58793368) | none | Done: binder, fused evaluator, exact round, temporal parts, legacy filter; 41 cases pass the runner; coverage 95.7%. Fixed: add(duration, timestamp) cast order, and the Audit tree's null minutes (skip_nulls false). Left: the 4G.3 measurement |
| 14 | 4.4 group_aggregate | MERGED 8a4b6ed4 (bba7bb70) | none | Done: hash and dense paths, exact float sums (the superaccumulator's top-digit overflow fixed), 29 cases pass the runner, coverage 94.4%. Left: the 4G.4 counts and the float-sum cost (exact, binned, plain) on a month and on the year |
| 15 | 4.5 interval_coverage_count | MERGED (`work/4.5-interval-coverage-count` a70e3416) | scratch in wip | Done: year compute 53.3 s in Python against 0.85 s; peak 1,217 MiB against 89 MiB; bit for bit. Left: drop its stand-in corpus test once 4.1's runner lands |
| 17 | 6.3v the Linux restart | after 6.3f | none | A Linux image instance killed by its cgroup (memory.events oom_kill) restarts once at the need (peak and what the kill shows, widened by the margin, capped at half the room), its failed slice sent again, as the node's restart does (6.3g) |
| 18 | 6.3w the controller's anon fit on small morsels | to write | none | Found by 6.3f: on 5 KB test batches the anon fit read 115–215 bytes a morsel byte (the process's own few MiB put on tiny morsels, fixed term 0), so the controller ran 1 worker of 4. Boots show "anon fit 0 bytes fixed plus ~4 bytes a morsel byte" too. Figures (IM-T32/33, 1 GiB, arrow_echo): 5 KB batches fit 215.8 and 114.4 bytes a byte, anon for kernels 66 MB, 1–2 of 4 workers; 1 MiB batches fit 12.9 (true about 2, the process's ~3.9 MB fixed put on the morsels), workers dropped to 1 at the warm-up's end and came back one every 0.27 s. The fit needs its fixed term |
| 19 | 1.11g the device manager's drain clock | to write | none | Found by 1.11e: the device manager gives its drivers WAIT_NS = 120 s to end once its own listeners close (kernel/devices/server/src/node.rs:880), but block3–7 (the Linux stack's disks) end only when their last client does; a boot whose tests finish sooner drains for longer (83–97 s, once ~120 s). Start the clock only for drivers whose own clients have gone |
| 16 | S.1 dependency advisories | `work/S.1-dependency-advisories` 9b045426 | patch | Read the patch, then build |

