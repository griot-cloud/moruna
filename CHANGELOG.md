# Changelog

## 0.1.1

Fixes S1 for tight budgets. **0.1.0 is yanked and should not be used.**

- **The memory bound holds at a tight ceiling.** The controller planned morsels into the whole of the headroom the arena leaves above itself, and the morsels are not the only thing there: a Parquet writer's encoder, the Arrow builders a batch is converted through and the interpreter's own growth are all outside the arena, none of them in the model, and every one of them in the figure S1 is measured from. A Python kernel at a 512 MiB budget on a host with a large resting footprint reached 1.005 and 1.012 of its ceiling. The kernels are now planned into a share of that headroom and the rest is held back (11 f.3), and the same job peaks at 0.976 across resting footprints from 0 to 380 MB.
- **A kernel's fixed cost is funded before its first morsel.** One probe cannot tell a kernel that holds a fixed cost from one whose cost scales with the morsel, and the model read its whole measurement as the scaling term, which under-predicted a constant-cost kernel by the ratio of the morsel sizes. The probe now seeds both terms until a record separates them.
- **A budget that cannot hold a run says so before the run starts.** Where the smallest morsel the runtime can form does not fit the ceiling, `start` refuses with the arithmetic it refused on. It used to begin, run one morsel and report a footprint the process was already holding.
- **macOS wheels.** 0.1.0 published a Linux wheel only, because the publishing step cannot run on a macOS runner. Building and publishing are separate jobs now, so both platforms ship.
- The run report travels with the Python surface's budget diagnostics, so a ceiling passed on a host you cannot log into is answerable from the report alone.

## 0.1.0

Yanked. First release: Parquet to Python to Parquet inside a byte budget, spill to disk, resume from a manifest, Polars and DataFusion bridges. Its memory bound could be exceeded by about 1% at a tight ceiling (see 0.1.1), and the wheel was Linux only.
