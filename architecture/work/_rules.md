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
- Host tests of single crates (`cargo test -p …`, `cargo llvm-cov -p …`) don't need the lock. Run
  every cargo command with `-j 2`: up to six agents share this Mac's 10 cores and 16 GB.
- An item with booted checks is done in two phases: everything that needs no boot first, then a
  report; the PM continues you for the booted checks when what they need is merged.
- The disk is nearly full: build with `CARGO_INCREMENTAL=0`, and delete `target` when you stop.
- Create nothing in any cloud, and spend nothing.
- Where your work item and the system design leave a choice open, ask the PM. Do not choose.
