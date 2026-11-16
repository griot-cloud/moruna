//! The data disk's virtio-blk queues, driven by the engine itself with DMA into arena pages
//! (`architecture/data-os.md` §3, decision 2): stream C's module. Only stream C writes this
//! file and the files under `blk/`.
