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
    /// The byte count a Parquet file actually rolls at: `sink.file_bytes`, or less when the
    /// arena could not serve a buffer that large (08 f.1).
    pub roll_bytes: u64,
}
