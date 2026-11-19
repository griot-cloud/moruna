//! The arena file buffer an encoding sink writes its file into (08 f.1): the Parquet sink's
//! (f.11).
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

// Stream C (2026-11-17): a lane's buffer starts small and grows with its file.
/// The bytes a lane's file buffer starts with when no spare is free: it doubles as the file
/// grows, up to the lane's share.
pub(crate) const START_BYTES: u64 = 8 << 20;

// Stream C (2026-11-17): a lane's share is capped, whatever the budget.
/// The most one lane's file buffer grows to: a file rolls at this less its footer. A larger
/// budget buys more room and more lanes, not larger files or larger morsels (a morsel is at
/// most half a lane's buffer), so the first morsels and the memory first touched before the
/// first row stay the same at any budget.
pub const LANE_MAX_BYTES: u64 = 64 << 20;

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
    /// Where the buffer grows from when the file outgrows it, and how far: `None` for a buffer
    /// sized for one morsel, which never grows (h).
    pub(crate) growth: Option<Growth>,
}

// Stream C (2026-11-17): a lane's buffer is made at its first file and grows with it.
/// How a file buffer grows: from `alloc`, to at most `cap` bytes, a size class at a time.
#[derive(Clone)]
pub(crate) struct Growth {
    pub(crate) alloc: Arc<dyn Allocator>,
    pub(crate) cap: u64,
}

impl FileBuf {
    /// Append `data` at the end of what the encoder has written, growing the buffer when it is
    /// full, or refuse with `TOO_LARGE` when it cannot hold it: never a truncated file (h).
    pub(crate) fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        let end = self.used + data.len();
        let len = self.buf.as_ref().map_or(0, |b| b.len());
        if self.buf.is_some() && end > len && !self.reserve(end as u64) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("{TOO_LARGE}: {end} bytes into {len} bytes"),
            ));
        }
        let FileBuf { buf, used, .. } = self;
        let Some(buf) = buf.as_mut() else {
            return Err(std::io::Error::other("the output buffer is gone"));
        };
        buf[*used..end].copy_from_slice(data);
        *used = end;
        Ok(())
    }

    /// Room for `need` bytes in all: true when the buffer holds them, grown when it did not
    /// into a larger class (twice its size, or the class `need` falls in, at most `cap`), the
    /// bytes written so far copied over and the old buffer given back; false when it cannot
    /// grow that far (no growth, past `cap`, or the arena has no such buffer).
    pub(crate) fn reserve(&mut self, need: u64) -> bool {
        let Some(buf) = self.buf.as_ref() else {
            return false;
        };
        let len = buf.len() as u64;
        if need <= len {
            return true;
        }
        let Some(growth) = &self.growth else {
            return false;
        };
        if need > growth.cap {
            return false;
        }
        let tier = host_tier(&*growth.alloc);
        // Twice the buffer first, so a growing file is copied a bounded number of times; the
        // least class that holds `need` when the arena has nothing that large.
        let doubled = class_ceil(need.max(len.saturating_mul(2))).min(growth.cap);
        let least = class_ceil(need).min(growth.cap);
        let bigger = [doubled, least]
            .into_iter()
            .filter_map(|want| usize::try_from(want).ok())
            .find_map(|want| growth.alloc.alloc(want, tier).ok());
        let Some(mut bigger) = bigger else {
            return false;
        };
        bigger[..self.used].copy_from_slice(&buf[..self.used]);
        self.buf = Some(bigger);
        true
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
    /// How many spares the pool keeps: one for a sink with one open file, one per lane for a
    /// sink that encodes into several at once (08 f.12).
    keep: std::sync::atomic::AtomicUsize,
}

impl FileBuffers {
    pub(crate) fn new(alloc: Arc<dyn Allocator>) -> FileBuffers {
        FileBuffers {
            alloc,
            spare: Mutex::new(Vec::new()),
            keep: std::sync::atomic::AtomicUsize::new(1),
        }
    }

    /// The file buffer and the roll size it implies (f.1). A morsel that needs more than
    /// `file_bytes` gets a buffer sized for it, which does not grow, or nothing. An ordinary
    /// file rolls at `file_bytes` (the lane's share, less the footer) but starts in a spare or in
    /// a buffer of [`START_BYTES`], and the buffer grows with the file up to `file_bytes`
    /// (stream C, 2026-11-17): a run that writes a few megabytes holds a few megabytes, where
    /// every lane used to hold its whole share from the sink's `open`.
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
    ) -> Result<(FileBuf, u64)> {
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
        let cap = class_ceil(file_bytes).max(floor);
        let growth = (min_bytes == 0).then(|| Growth {
            alloc: Arc::clone(&self.alloc),
            cap,
        });
        let file = |buf: Buffer, growth: Option<Growth>| FileBuf {
            buf: Some(buf),
            used: 0,
            growth,
        };
        if min_bytes == 0
            && let Some(buf) = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop()
        {
            let cap = cap.max(buf.len() as u64);
            let growth = growth.map(|g| Growth { cap, ..g });
            return Ok((file(buf, growth), cap - footer_for(cap)));
        }
        let first = match min_bytes {
            0 => class_ceil(START_BYTES).min(cap).max(floor),
            _ => cap,
        };
        let mut want = first;
        loop {
            match usize::try_from(want) {
                Ok(capacity) => match self.alloc.alloc(capacity, host_tier(&*self.alloc)) {
                    // The footer is spent out of the class, so the file rolls below it. An
                    // ordinary file rolls at its cap, to which its buffer grows; one whose first
                    // buffer the arena already had to halve rolls at what it got.
                    Ok(buf) => {
                        if want < soft.min(cap) && want < first {
                            tracing::info!(
                                target: "moruna::sink",
                                requested_row_group_bytes = row_group_bytes,
                                file_buffer_bytes = want,
                                "the arena cannot hold this row group target; writing smaller row groups"
                            );
                        }
                        let roll_at = if want < first { want } else { cap };
                        let growth = growth.map(|g| Growth { cap: roll_at, ..g });
                        return Ok((file(buf, growth), roll_at - footer_for(roll_at)));
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
            if spare.len() < self.keep.load(std::sync::atomic::Ordering::SeqCst) {
                spare.push(buf);
            }
        }
    }

    /// The share of the pair's bytes, `2 x file_bytes`, each of `count` lanes may grow its file
    /// buffer to (08 f.12): a sink that encodes into `count` files at once holds at most what
    /// one with two buffers held, divided between them, and keeps up to `count` spares. Nothing
    /// is allocated here (stream C, 2026-11-17): a lane's buffer is made when it opens its first
    /// file, so a lane that never opens one holds nothing.
    pub(crate) fn lane_cap(&self, file_bytes: u64, count: usize) -> u64 {
        let count = count.max(2);
        let tier = host_tier(&*self.alloc);
        let floor = class_ceil(GRANULE);
        let share = file_bytes.saturating_mul(2) / count as u64;
        let mut want = class_ceil(share).max(floor);
        if let Some(free) = self.alloc.available(tier) {
            want = want.min(class_floor(free / (2 * count as u64)).max(floor));
        }
        let want = want.min(LANE_MAX_BYTES).max(floor);
        self.keep.store(
            if count == 2 { 1 } else { count },
            std::sync::atomic::Ordering::SeqCst,
        );
        want
    }
}

/// The file a lane of a sink of `file_bytes` files encoding into `lanes` at once grows to, the
/// arena aside ([`FileBuffers::lane_cap`] with all the arena it asks free): a share of the
/// pair's bytes, a whole class, at most [`LANE_MAX_BYTES`]. The run reads it to know how large a
/// row group a lane can reach before it rolls (system design 3A, 6.3d).
pub fn lane_file_bytes(file_bytes: u64, lanes: usize) -> u64 {
    let share = file_bytes.saturating_mul(2) / lanes.max(2) as u64;
    class_ceil(share)
        .min(LANE_MAX_BYTES)
        .max(class_ceil(GRANULE))
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
