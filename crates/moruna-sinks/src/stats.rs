//! Sink counters (08 d.1, j).

/// What a sink counted while it ran. Read by the run report and by the SI tests.
///
/// `multipart_parts` stays 0 in this crate: the reactor splits an object write into parts
/// (06 f.4) and the sink issues one `write_object` per file (f.1), so a part is never the
/// sink's to count. The field exists because d.1 names it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SinkStats {
    /// Calls to `write` that reached the encoder.
    pub writes: u64,
    /// The slices a Parquet sink's writers wrote those calls' morsels in: one a morsel unless
    /// the lane's room under its flush point cut it (6.3d, `lane_heap::Bounds::slice`).
    pub slices: u64,
    /// Payload bytes the CPU encoded into arena memory (the one copy G-I2 allows a sink).
    pub encode_bytes: u64,
    /// Files whose bytes are visible to a reader and safe against process loss.
    pub files_committed: u64,
    /// Times the current output file was closed and the next opened.
    pub rolls: u64,
    /// Multipart parts this sink issued itself; always 0, see the type comment.
    pub multipart_parts: u64,
    /// The largest number of bytes a reorder buffer held at once.
    pub reorder_held_max: u64,
    /// Stall episodes a reorder buffer entered.
    pub stalls: u64,
    /// Files a `resume` removed because their sequence range lay above the watermark.
    pub resumed_files_removed: u64,
    /// An earlier run's part files and `_SUCCESS` a fresh `open` removed from the prefix.
    pub earlier_files_removed: u64,
    /// The byte count a Parquet file actually rolls at: `sink.file_bytes`, or less when the
    /// arena could not serve a buffer that large (08 f.1).
    pub roll_bytes: u64,
    /// The lanes a Parquet sink writes on (08 f.12): the run's workers, or fewer when one
    /// lane's columns need more of the writers' heap than a worker's part of it (6.3d). Zero
    /// for a sink without lanes, or before `open`.
    pub lanes: u64,
    /// Whether a Parquet sink's writers' heap was measured, by the counting allocator the engine
    /// and the Python extension install, or estimated from parquet's own figure where the
    /// program installs none (6.3d, `lane_heap::FALLBACK_FACTOR`).
    pub writer_heap_measured: bool,
}
