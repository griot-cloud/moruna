# Rules for every work item

- Your branch starts from `feature/arrow-core-0.7`. Local commits only, with the trailers
  `Signed-off-by: Moruna Agent <agents@griotdata.com>` then `Co-Authored-By`. Never push, and never
  queue CI.
- Never skip or bypass a hook or a check (`MORUNA_SKIP_QUALITY` included) without asking the PM.
- Tests ship with the code: at least 80% line coverage for the code you add to Moruna's crates.
- This Mac is arm64, so x86_64 boots run under QEMU's emulator, slowly.
- Building the boot image (`build.sh`, `package.py`) and every boot need `~/.vm-lock`
  (`tools/proof-lab/run.py` shows how to take it). Never steal it. Keep a VM at or under 6 GiB.
  Release it the moment you stop, including when you stop to ask.
- **A booted turn takes the Mac build lock too only when the Mac is short of disk:** if
  `df -h /System/Volumes/Data` shows under 20 GB free when the turn starts, take `~/.vm-lock` first,
  then a Mac slot, and hold both until the turn's build and boot are done (2026-11-18: an
  image build beside cargo builds filled the disk). With 20 GB or more free, the VM lock alone.
  Never take them in the other order.
- **Mac builds and tests take a Mac slot:** `~/.mac-build-lock` or `~/.mac-build-lock-2`, by
  `mkdir`, with an `owner` file naming your agent id, item and UTC time. Remove it the moment you
  stop building. If `~/.mac-build-lock-next` names someone else, they take the next free slot
  first. When it is empty, write your name there while you wait, and clear it once you hold a slot.
  Poll every 30 s. A third slot, `~/.mac-build-lock-3`, is for commit hooks alone (a commit's
  clippy run, minutes at most): take it only around `git commit`, never for a build or a test run.
- Host tests of single crates (`cargo test -p …`, `cargo llvm-cov -p …`) don't need the VM lock,
  but like every cargo build or test on the Mac they take a Mac slot (above). Run
  every cargo command with `-j 2`, and tests with `RUST_TEST_THREADS=2`: up to nine agents and a
  boot share this Mac's 10 cores and 16 GB.
- An item with booted checks is done in two phases: everything that needs no boot first, then a
  report; the PM continues you for the booted checks when what they need is merged.
- Free disk as you go: build with `CARGO_INCREMENTAL=0`; delete every `target` folder you made
  in your worktree whenever you stop or hand back, and your folders in `moruna-arm`'s `/tmp`. Never
  delete another agent's.
- **Inside `moruna-arm`, build into your own target on the container's disk:**
  `export CARGO_TARGET_DIR=/root/target/<your agent id>` and pass `build.sh` its fourth argument as
  `/root/target/<your agent id>/host`. Never share one target between worktrees: Cargo gives the same
  workspace crate the same artifact name in every worktree, so a shared target can link another
  branch's build (it did, 2026-11-18). Keep yours between your own turns; delete it when your item
  is done.
- Create nothing in any cloud, and spend nothing.
- Where your work item and the system design leave a choice open, ask the PM. Do not choose.
- Scratch files go only in your own folder, `<scratchpad>/<your agent id>/`. Delete only there,
  never at the scratchpad's top level, which the PM and other agents share.
