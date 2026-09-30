//! The arena file buffer an encoding sink writes its file into (08 f.1), shared by the Parquet
//! and Vortex sinks (f.11).
//!
//! An encoder writes straight into one arena buffer per open file, and the reactor writes the
//! finished file from a view over that buffer. The buffer is one whole size class of 02 e.2
//! with the footer's room inside it, halved on `Alloc` down to what the format and the morsel in
//! hand require; the two a sink needs are taken at `open`, while the arena is still empty, and
//! a written file's buffer is kept for the next file (F8.8).

use std::sync::{Arc, Mutex};

use moruna_kernel::arrow::datatypes::SchemaRef;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Allocator, Buffer, MorunaError, Result, RunId};

use crate::host_tier;

/// Room above the roll size for the footer the writer appends at close (f.1).
const FOOTER_HEADROOM: u64 = 1 << 20;

/// The smallest file buffer worth asking for: the arena's own smallest class (02 e.2).
pub(crate) const GRANULE: u64 = 64 << 10;

/// The message a buffer overflow carries out of the encoder, so `write` can turn an I/O error
/// back into the `Sink` error section h names.
pub(crate) const TOO_LARGE: &str = "morsel too large to encode";

/// The size class 02 e.2 will charge for `n` bytes: a power of two, no smaller than the 64 KiB
/// granule. The arena charges the class and not the request (AR-I5), so a sink that asks for
/// anything else pays for the difference and needs the whole class free at once.
pub(crate) fn class_ceil(n: u64) -> u64 {
    n.max(GRANULE).next_power_of_two()
}

/// The largest size class no larger than `n`, and the granule below it.
fn class_floor(n: u64) -> u64 {
    if n <= GRANULE {
        GRANULE
    } else {
        1u64 << (63 - n.leading_zeros())
    }
}

/// The footer's share of a `want` byte class (f.1): `FOOTER_HEADROOM`, or half the class when
/// that is smaller, because a small file's footer is small and a buffer that is all footer can
/// hold no row group. A file with a `want / 2` roll size can hold at most one row group per
/// `row_group_bytes`, so half the class is ample for its footer.
pub(crate) fn footer_for(want: u64) -> u64 {
    FOOTER_HEADROOM.min(want.max(2) / 2)
}

/// The bytes a batch's rows occupy, which is what the encoder reads: not the capacity of the
/// buffers they are a slice of. A morsel read back from staging is a slice of the segment it
/// was staged in, and measured by its buffers it looked several times its size, so the sink
/// rolled for it and asked for a buffer the arena could not give (F8.8, H6 at 256 MiB).
pub(crate) fn rows_bytes(batch: &RecordBatch) -> u64 {
    batch
        .columns()
        .iter()
        .map(|c| {
            c.to_data()
                .get_slice_memory_size()
                .unwrap_or_else(|_| c.get_array_memory_size()) as u64
        })
        .sum()
}

/// One open output file's arena buffer and how much of it the encoder has used.
pub(crate) struct FileBuf {
    pub(crate) buf: Option<Buffer>,
    pub(crate) used: usize,
}

impl FileBuf {
    /// Append `data` at the end of what the encoder has written, or refuse with `TOO_LARGE`
    /// when the buffer cannot hold it: never a truncated file (h).
    pub(crate) fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        let FileBuf { buf, used } = self;
        let Some(buf) = buf.as_mut() else {
            return Err(std::io::Error::other("the output buffer is gone"));
        };
        let end = *used + data.len();
        if end > buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("{TOO_LARGE}: {end} bytes into {} bytes", buf.len()),
            ));
        }
        buf[*used..end].copy_from_slice(data);
        *used = end;
        Ok(())
    }
}

/// The file buffers a sink holds: the allocator it asks and the buffers no file holds.
pub(crate) struct FileBuffers {
    alloc: Arc<dyn Allocator>,
    /// File buffers no file holds: the two taken at `open` while the arena is still empty, and
    /// after that the buffer of the last file whose write completed. The next ordinary file
    /// takes one rather than asking the arena again, so the sink's buffers are found once, when
    /// they can be, and not in an arena the queues have since divided up (F8.8).
    spare: Mutex<Vec<Buffer>>,
}

impl FileBuffers {
    pub(crate) fn new(alloc: Arc<dyn Allocator>) -> FileBuffers {
        FileBuffers {
            alloc,
            spare: Mutex::new(Vec::new()),
        }
    }

    /// The file buffer and the roll size it implies (f.1). A morsel that needs more than
    /// `file_bytes` gets the buffer it needs or nothing; an ordinary file takes the largest
    /// buffer the arena will give between `file_bytes` and the class its row group needs.
    ///
    /// Every request is a whole size class and the footer headroom lives *inside* it (f.1).
    /// Asking for `row_group_bytes + footer` asked for 129 MiB at the default row group, and
    /// 02 e.2 serves that out of the 256 MiB class, which has to be free all at once: the
    /// sink reserved twice what it wanted and no 512 MiB budget could open it (PM, 2026-09-23).
    pub(crate) fn alloc_file_buf(
        &self,
        min_bytes: u64,
        row_group_bytes: u64,
        file_bytes: u64,
    ) -> Result<(Buffer, u64)> {
        if min_bytes == 0
            && let Some(buf) = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop()
        {
            let want = buf.len() as u64;
            return Ok((buf, want - footer_for(want)));
        }
        // `row_group_bytes` is a flush threshold and an upper bound, so the footer may come out
        // of its class; `min_bytes` is a morsel that has to fit, so its class is sized above the
        // footer as well as above the morsel.
        let soft = class_ceil(row_group_bytes.max(GRANULE));
        let hard = match min_bytes {
            0 => 0,
            need => class_ceil(need.saturating_add(footer_for(need))),
        };
        // A row group is a target, not a requirement: `sink.row_group_bytes` says how large
        // a row group should be, and nothing in the format says a file cannot hold smaller
        // ones. The floor used to be that target's own size class, which made the default
        // 128 MiB row group demand a 128 MiB contiguous class, a quarter of a 512 MiB
        // budget. On a host whose resting footprint was a few megabytes larger than the
        // author's laptop, the arena then had 121 MiB and an ordinary job was refused
        // rather than run: a coin flip on baseline, decided somewhere nobody was looking
        // (found on a Linux CI runner, 2026-09-23). The floor is now what the format and
        // the morsel in hand actually require, so a tight budget writes smaller row groups
        // and completes, which is what S6 asks of every other knob: degrade, never refuse
        // what can still be done.
        let floor = hard.max(class_ceil(GRANULE));
        let mut want = class_ceil(file_bytes).max(floor);
        loop {
            match usize::try_from(want) {
                Ok(capacity) => match self.alloc.alloc(capacity, host_tier(&*self.alloc)) {
                    // The footer is spent out of the class, so the file rolls below it.
                    Ok(buf) => {
                        if want < soft {
                            tracing::info!(
                                target: "moruna::sink",
                                requested_row_group_bytes = row_group_bytes,
                                file_buffer_bytes = want,
                                "the arena cannot hold this row group target; writing smaller row groups"
                            );
                        }
                        return Ok((buf, want - footer_for(want)));
                    }
                    // Below the floor the error stands: a sink that cannot hold one row group
                    // cannot encode one.
                    Err(e) if want <= floor => return Err(e),
                    Err(_) => {}
                },
                Err(_) if want <= floor => {
                    return Err(MorunaError::Sink(format!(
                        "a file buffer of {want} bytes does not fit this platform"
                    )));
                }
                Err(_) => {}
            }
            want = (want / 2).max(floor);
        }
    }

    /// Keep a written file's buffer for the next file when there is no spare already; any other
    /// goes back to the arena.
    pub(crate) fn keep_spare(&self, buf: Arc<Buffer>) {
        if let Ok(buf) = Arc::try_unwrap(buf) {
            let mut spare = self.spare.lock().unwrap_or_else(|e| e.into_inner());
            if spare.is_empty() {
                spare.push(buf);
            }
        }
    }

    /// The sink's two file buffers, taken at `open` (f.1): the file being encoded and the one a
    /// roll opens while the first is still being written. Each is the class `file_bytes` asks
    /// for and no more than a quarter of what the arena has free, so the pair leaves the arena
    /// at least half of itself; when the arena cannot give the pair, both halve together, down
    /// to the smallest class, below which the sink cannot open.
    pub(crate) fn alloc_pair(&self, file_bytes: u64) -> Result<()> {
        let tier = host_tier(&*self.alloc);
        let floor = class_ceil(GRANULE);
        let mut want = class_ceil(file_bytes).max(floor);
        if let Some(free) = self.alloc.available(tier) {
            want = want.min(class_floor(free / 4).max(floor));
        }
        loop {
            let capacity = usize::try_from(want).map_err(|_| {
                MorunaError::Sink(format!(
                    "a file buffer of {want} bytes does not fit this platform"
                ))
            })?;
            let pair = self
                .alloc
                .alloc(capacity, tier)
                .and_then(|first| Ok(vec![first, self.alloc.alloc(capacity, tier)?]));
            match pair {
                Ok(pair) => {
                    *self.spare.lock().unwrap_or_else(|e| e.into_inner()) = pair;
                    return Ok(());
                }
                Err(e) if want <= floor => return Err(e),
                Err(_) => want = (want / 2).max(floor),
            }
        }
    }
}

/// A run id as the lowercase hex a footer carries (e.2).
pub(crate) fn hex(run_id: &RunId) -> String {
    use std::fmt::Write as _;
    run_id.0.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Name the first field that differs, which is what the scheduler's diagnostic needs (h).
pub(crate) fn differing_field(expected: &SchemaRef, found: &SchemaRef) -> String {
    for (i, field) in expected.fields().iter().enumerate() {
        match found.fields().get(i) {
            Some(other) if other == field => {}
            Some(other) => {
                return format!(
                    "schema drift: field {} is {:?}, the first morsel had {:?}",
                    field.name(),
                    other.data_type(),
                    field.data_type()
                );
            }
            None => return format!("schema drift: field {} is missing", field.name()),
        }
    }
    match found.fields().get(expected.fields().len()) {
        Some(extra) => format!("schema drift: field {} is not in the schema", extra.name()),
        None => "schema drift: the schemas differ in metadata".to_string(),
    }
}
