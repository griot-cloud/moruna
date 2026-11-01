//! Where a run's events go (MH 4.3 version 7): each is a record (SDD 21), told once and delivered
//! to every place the run has for it.
//!
//! One thread, apart from every worker and drive, takes the facts in the order they became true
//! ([`Events`]), numbers each from 1 with no gap, writes it as a record with
//! [`moruna_event::Encoder`] and hands the record on:
//!
//! - to the peer on the connection, as a frame (`moruna_event::frame`), when there is one;
//! - to a file in the run's output store, [`EVENTS_FILE`] in the attempt's directory, when there
//!   is none: the records back to back, each exactly as it was written;
//! - into the [`EventRing`] an embedder gave the run, which it drains in its own time: a full
//!   ring refuses the record and counts it, and the run never waits for it;
//! - as a line of JSON, rendered from the record at the edge, to the stream the document of
//!   `moruna run --events-json` names ([`EventOptions::json`]).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use moruna_event::{Encoder, Stamp};
use moruna_record::Ring;

use super::lifecycle::EventOut;
use super::protocol::Event;
use super::session::Wire;

/// The file of a run's events in its output store, in the attempt's directory beside the
/// producers' (`_` starts no task id): records back to back. Written when no peer is there to
/// be told.
pub const EVENTS_FILE: &str = "_events.mrec";

/// The extension of a record file in the output store: records back to back.
pub const RECORD_FILE: &str = "mrec";

/// Whose events these are: what each record carries beside its fact.
#[derive(Clone, Debug)]
pub struct Meta {
    /// The run's id.
    pub run: String,
    /// The id of the plan the run runs; `None` when the document was refused before it was
    /// planned.
    pub plan: Option<String>,
    /// The document's `report.attempt`.
    pub attempt: u32,
    /// This process's id: 32 lowercase hex characters.
    pub start: String,
    /// Unix nanoseconds at the instant `t_us` counts from.
    pub epoch_ns: i64,
}

impl Meta {
    /// What a record carries beside its fact.
    pub fn stamp(&self) -> Stamp<'_> {
        Stamp {
            run: Some(&self.run),
            plan: self.plan.as_deref(),
            attempt: self.attempt,
            start: &self.start,
        }
    }

    /// The unix nanoseconds of `t_us` microseconds into the run.
    pub fn ts_ns(&self, t_us: u64) -> i64 {
        self.epoch_ns
            .saturating_add((t_us as i64).saturating_mul(1000))
    }
}

/// The unix nanoseconds now.
pub fn unix_ns_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// A ring of records an embedder drains while the run fills it: every event of the run is pushed
/// to it as the record it is, the encoder writing straight into the slot. The run never waits
/// for the drain; a full ring refuses the record and counts it ([`EventRing::dropped`]).
///
/// The region the ring lives in is allocated once and kept for the life of the process, as a
/// ring that producers on other threads hold must be: make one per embedder, not one per run.
pub struct EventRing {
    ring: Ring<'static>,
}

impl EventRing {
    /// A ring whose data area is the largest power of two that fits `capacity` bytes (at least
    /// `moruna_record::ring::MIN_CAPACITY`).
    pub fn new(capacity: usize) -> Result<Arc<EventRing>, moruna_record::Error> {
        let len =
            moruna_record::ring::HEADER + capacity.max(moruna_record::ring::MIN_CAPACITY as usize);
        // The ring needs a 64-byte-aligned region; the allocator promises less, so take 64
        // bytes more and start at the first aligned address.
        let region: &'static mut [u8] = Box::leak(vec![0u8; len + 64].into_boxed_slice());
        let skew = region.as_ptr().align_offset(64);
        let (_, region) = region.split_at_mut(skew);
        let (region, _) = region.split_at_mut(len);
        Ok(Arc::new(EventRing {
            ring: Ring::init(region)?,
        }))
    }

    /// Hand every record in the ring to `each`, oldest first, and release it; the number of
    /// records. Only one drain runs at a time: a second is refused while the first lives.
    pub fn drain(&self, mut each: impl FnMut(&[u8])) -> Result<usize, moruna_record::Error> {
        let mut consumer = self.ring.consumer()?;
        let mut n = 0;
        while let Some(record) = consumer.pop() {
            each(record);
            n += 1;
        }
        Ok(n)
    }

    /// The records the ring refused so far for want of room.
    pub fn dropped(&self) -> u64 {
        self.ring.dropped()
    }

    fn push(&self, record: &[u8]) -> bool {
        self.ring.producer().push(record).is_ok()
    }
}

/// Where a run's events go beside the connection and the output store.
#[derive(Clone, Default)]
pub struct EventOptions {
    /// A ring the embedder drains.
    pub ring: Option<Arc<EventRing>>,
    /// A stream that gets every event as a line of JSON, rendered from its record (`moruna run
    /// --events-json`).
    pub json: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
}

/// The thread that writes a run's events (MH 4.3 `event`): each gets the next `seq` as it is
/// written, so a peer reads them numbered from 1 with no gap, in the order they became true.
pub(super) struct Events {
    tx: mpsc::Sender<Option<(u64, Event)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Events {
    /// Start the writer for the run `meta` names. `store` is the attempt's directory in the output
    /// store, where the records go when `wire` has no peer.
    pub(super) fn start(
        wire: &Arc<Wire>,
        meta: Meta,
        store: PathBuf,
        options: &EventOptions,
    ) -> Events {
        let (tx, rx) = mpsc::channel::<Option<(u64, Event)>>();
        let writer = wire.clone();
        let options = options.clone();
        let to_file = !wire.has_peer();
        let thread = std::thread::Builder::new()
            .name("moruna-events".to_string())
            .spawn(move || {
                let mut out = Out {
                    wire: writer,
                    file: None,
                    file_path: to_file.then(|| store.join(EVENTS_FILE)),
                    options,
                    refused: 0,
                };
                let mut encoder = Encoder::new();
                let mut seq = 0u64;
                while let Ok(Some((t_us, body))) = rx.recv() {
                    match encoder.encode(&meta.stamp(), seq + 1, meta.ts_ns(t_us), t_us, &body) {
                        Ok(record) => {
                            seq += 1;
                            out.deliver(record);
                        }
                        Err(error) => out.wire.note(format!(
                            "an event of the run could not be written as a record and is not \
                             told: {error}"
                        )),
                    }
                }
                out.finish();
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                wire.note(format!(
                    "the run's events cannot be written: no thread for them ({error})"
                ));
                None
            }
        };
        Events { tx, thread }
    }

    pub(super) fn out(&self) -> EventOut {
        let tx = self.tx.clone();
        Box::new(move |t_us, event| {
            let _ = tx.send(Some((t_us, event)));
        })
    }
}

/// Dropping the events writes every event told so far and ends the thread; later ones go
/// nowhere. Dropping is the one way they close, so a run that unwinds past its events still
/// writes every event told before, the ends its lifecycle told as it dropped included (PY-I11).
impl Drop for Events {
    fn drop(&mut self) {
        let _ = self.tx.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The places one record goes.
struct Out {
    wire: Arc<Wire>,
    file: Option<File>,
    /// Where the file goes, until it is open or has failed.
    file_path: Option<PathBuf>,
    options: EventOptions,
    /// Records the ring refused.
    refused: u64,
}

impl Out {
    fn deliver(&mut self, record: &[u8]) {
        self.wire.send_event(record);
        self.write_file(record);
        if let Some(ring) = &self.options.ring
            && !ring.push(record)
        {
            self.refused += 1;
        }
        self.write_json(record);
    }

    fn write_file(&mut self, record: &[u8]) {
        if self.file.is_none()
            && let Some(path) = self.file_path.take()
        {
            let opened = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| File::create(&path));
            match opened {
                Ok(file) => self.file = Some(file),
                Err(error) => self.wire.note(format!(
                    "the run's events are not kept: {} could not be made: {error}",
                    path.display()
                )),
            }
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(error) = file.write_all(record) {
            self.file = None;
            self.wire.note(format!(
                "the run's events stopped being kept: the events file could not be written: \
                 {error}"
            ));
        }
    }

    fn write_json(&mut self, record: &[u8]) {
        let Some(json) = &self.options.json else {
            return;
        };
        let line = match moruna_event::decode(record) {
            Ok(event) => event.json_line(),
            Err(error) => {
                self.wire.note(format!(
                    "an event the run wrote does not read back as one: {error}"
                ));
                return;
            }
        };
        let mut out = json.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = out.write_all(line.as_bytes()).and_then(|()| out.flush()) {
            drop(out);
            self.options.json = None;
            self.wire.note(format!(
                "the events stopped being printed: the stream could not be written: {error}"
            ));
        }
    }

    fn finish(&mut self) {
        if self.refused > 0 {
            self.wire.note(format!(
                "the embedder's ring had no room for {} events of the run, which it did not get",
                self.refused
            ));
        }
        if let Some(file) = self.file.as_mut()
            && let Err(error) = file.sync_all()
        {
            self.wire.note(format!(
                "the run's events file could not be synced: {error}"
            ));
        }
    }
}

/// A writer that gets the host protocol as Moruna sends it, lines and frames, and writes it as
/// lines of JSON: a line as it is, an event's frame as the line its record renders (`moruna_event`
/// `EventMsg::json_line`). What `moruna run --vm` prints for people. A frame that is not an event
/// is an `InvalidData` error: the stream stops rather than print what it cannot read.
pub struct JsonLines<W: Write> {
    out: W,
    pending: Vec<u8>,
}

impl<W: Write> JsonLines<W> {
    /// Render onto `out`.
    pub fn new(out: W) -> JsonLines<W> {
        JsonLines {
            out,
            pending: Vec::new(),
        }
    }

    fn invalid(why: String) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, why)
    }

    /// Write every whole message in `pending`.
    fn drain(&mut self) -> std::io::Result<()> {
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            if rest[0] == moruna_event::frame::FRAME {
                if rest.len() < 5 {
                    break;
                }
                let len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
                if !(moruna_record::FIXED - 4..=moruna_record::MAX_RECORD).contains(&len) {
                    return Err(Self::invalid(format!(
                        "a frame's record says it has {len} bytes, which no record has"
                    )));
                }
                let total = 1 + 4 + len;
                if rest.len() < total {
                    break;
                }
                let event = moruna_event::decode(&rest[1..total])
                    .map_err(|e| Self::invalid(format!("a frame that is not an event: {e}")))?;
                self.out.write_all(event.json_line().as_bytes())?;
                at += total;
            } else {
                let Some(end) = rest.iter().position(|b| *b == b'\n') else {
                    break;
                };
                self.out.write_all(&rest[..=end])?;
                at += end + 1;
            }
        }
        self.pending.drain(..at);
        Ok(())
    }
}

impl<W: Write> Write for JsonLines<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        self.drain()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}
