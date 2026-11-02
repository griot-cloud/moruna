//! An unmodified program's standard streams on the data plane (SDD 19 f.6 over SDD 29 f.8): the
//! engine's adaptation of a program with no Moruna library, the job the Linux personality does
//! for one on the kernel.
//!
//! The bridge is the stage's task on the plane. One thread takes each batch of input slot 0 and
//! writes it to the program's standard input as an Arrow IPC stream, in place from the sealed
//! memfd (the first record whole, every later one from its record batch message on, the
//! end-of-stream marker once the input is finished). Another reads the program's standard
//! output a message at a time and lays each record batch as a record straight into a window of
//! output slot 1, which it puts: the engine seals and checks it like any task's record. The
//! copies are the pipes' own, as any `read` and `write` are; a program that makes the plane's
//! three calls itself moves no byte.
//!
//! The bridge ends its task, so the engine sees the end, when the program's output ends, and
//! when either side fails, with the failure kept for the stage's error.

use std::fs::File;
use std::io::{BufReader, Write};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use moruna_plane_linux::stream::{self, Reader};
use moruna_plane_linux::task::Plane;

/// How much of the program's output is read at once.
const READ_BUFFER: usize = 1 << 20;

/// What ended a bridge early, in words.
#[derive(Default)]
pub struct Fault(Mutex<Option<String>>);

impl Fault {
    /// The first failure is the one kept: the others follow from it.
    fn set(&self, why: String) {
        let mut f = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if f.is_none() {
            *f = Some(why);
        }
    }

    /// The failure, if there was one.
    pub fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A program's standard streams carried onto its task's plane.
pub struct Bridge {
    plane: Arc<Plane>,
    threads: Vec<JoinHandle<()>>,
    fault: Arc<Fault>,
}

impl Bridge {
    /// The bridge of the task whose plane end is `end`: input slot 0 to `stdin`, `stdout` to
    /// output slot 1.
    pub fn start(end: OwnedFd, stdin: File, stdout: File) -> Result<Bridge, String> {
        // Plane::new validates the endpoint and the engine's sealed backing objects.
        let plane =
            Arc::new(Plane::new(end).map_err(|e| format!("the stage's plane: {}", e.name()))?);
        let fault = Arc::new(Fault::default());
        let writer = {
            let (plane, fault) = (Arc::clone(&plane), Arc::clone(&fault));
            std::thread::Builder::new()
                .name("image-stdin".into())
                .spawn(move || {
                    if let Err(why) = feed(&plane, stdin) {
                        fault.set(why);
                        plane.close();
                    }
                })
                .map_err(|e| format!("the input's thread: {e}"))?
        };
        let reader = {
            let (plane, fault) = (Arc::clone(&plane), Arc::clone(&fault));
            std::thread::Builder::new()
                .name("image-stdout".into())
                .spawn(move || {
                    if let Err(why) = drain(&plane, stdout) {
                        fault.set(why);
                    }
                    // The output has ended, or failed: either way the task has.
                    plane.close();
                })
                .map_err(|e| format!("the output's thread: {e}"))?
        };
        Ok(Bridge {
            plane,
            threads: vec![writer, reader],
            fault,
        })
    }

    /// What ended the bridge early, if anything did.
    pub fn fault(&self) -> Option<String> {
        self.fault.get()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.plane.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Input slot 0 onto the program's standard input, until the input is finished.
fn feed(plane: &Plane, mut stdin: File) -> Result<(), String> {
    let mut input = plane.input(0).ok_or("input slot 0 is taken")?;
    let mut schema: Option<Vec<u8>> = None;
    loop {
        let batch = input
            .get()
            .map_err(|e| format!("its input on the plane: {}", e.name()))?;
        let Some(batch) = batch else {
            // A stream that never began ends without a byte (SDD 19 f.6).
            if schema.is_some() {
                stdin
                    .write_all(&stream::END)
                    .map_err(|e| format!("its input could not be ended: {e}"))?;
            }
            return Ok(());
        };
        let record = batch.bytes();
        let this = stream::schema_of(record)?;
        let first = match &schema {
            None => {
                schema = Some(this.to_vec());
                true
            }
            Some(s) if s.as_slice() == this => false,
            Some(_) => {
                return Err(
                    "its input changed schema between batches, which one stream cannot \
                            carry"
                        .into(),
                );
            }
        };
        stdin
            .write_all(stream::stream_bytes(record, first)?)
            .map_err(|e| format!("its input could not be written: {e}"))?;
    }
}

/// The program's standard output onto output slot 1, a record per record batch, until the
/// stream ends.
fn drain(plane: &Plane, stdout: File) -> Result<(), String> {
    let mut output = plane.output(1).ok_or("output slot 1 is taken")?;
    let mut reader = Reader::new(BufReader::with_capacity(READ_BUFFER, stdout));
    loop {
        let next = reader
            .batch()
            .map_err(|why| format!("its output is not an Arrow IPC stream: {why}"))?;
        let Some(pending) = next else {
            return Ok(());
        };
        let mut window = output
            .window()
            .map_err(|e| format!("its output's window on the plane: {}", e.name()))?
            .ok_or("the plane gave its output no window")?;
        if pending.len() > window.len() {
            return Err(format!(
                "its output batch is {} bytes as a record, more than the {} of its window, the \
                 stage's ceiling",
                pending.len(),
                window.len()
            ));
        }
        let len = reader
            .lay(pending, window.bytes())
            .map_err(|why| format!("its output is not an Arrow IPC stream: {why}"))?;
        window.put(0, len).map_err(|(error, _window)| {
            format!("its output's record on the plane: {}", error.name())
        })?;
    }
}
