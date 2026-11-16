//! Batches between the engine and a task through pre-shared regions and rings, with no copy
//! and no call per batch (`architecture/data-os.md` §5): stream P's module. Only stream P
//! writes this file and the files under `rings/`.
