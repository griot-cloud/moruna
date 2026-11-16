// SPDX-License-Identifier: MPL-2.0

//! Host tests of a task's start: the plane's framing against records in memory, and the
//! log ring.

use core::sync::atomic::{AtomicBool, Ordering};

use moruna_record::ring::Ring;
use ostd::prelude::ktest;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{
    plane::{Input, Output, Region, Slot},
    text::{RingBytes, RingText},
};
use crate::{
    fs::task_streams::{MessageKind, PlaneInput, PlaneOutput, TextSink},
    prelude::*,
};

// A table of one int64 column `n` of four rows, as pyarrow 24 writes it: the schema
// message, the record batch message, and the batch's body.
const SCHEMA: &[u8] = &[
    0xff, 0xff, 0xff, 0xff, 0x78, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00,
    0x0c, 0x00, 0x06, 0x00, 0x05, 0x00, 0x08, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00,
    0x0c, 0x00, 0x00, 0x00, 0x08, 0x00, 0x08, 0x00, 0x00, 0x00, 0x04, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x10, 0x00, 0x14, 0x00,
    0x08, 0x00, 0x06, 0x00, 0x07, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x02, 0x10, 0x00, 0x00, 0x00, 0x1c, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x6e, 0x00, 0x00, 0x00, 0x08, 0x00, 0x0c, 0x00,
    0x08, 0x00, 0x07, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x40, 0x00, 0x00, 0x00,
];
const BATCH: &[u8] = &[
    0xff, 0xff, 0xff, 0xff, 0x88, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x0c, 0x00, 0x16, 0x00, 0x06, 0x00, 0x05, 0x00, 0x08, 0x00, 0x0c, 0x00, 0x0c, 0x00, 0x00, 0x00,
    0x00, 0x03, 0x04, 0x00, 0x18, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x0a, 0x00, 0x18, 0x00, 0x0c, 0x00, 0x04, 0x00, 0x08, 0x00, 0x0a, 0x00, 0x00, 0x00,
    0x3c, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const BODY: &[u8] = &[
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// An output window: room for a record, which starts and ends on pages.
const WINDOW: usize = 1 << 20;

/// A batch or a window in memory.
struct MemRegion(Arc<SpinLock<Vec<u8>>>);

impl Region for MemRegion {
    fn len(&self) -> u64 {
        self.0.lock().len() as u64
    }

    fn read(&self, at: u64, out: &mut [u8]) -> Result<()> {
        let bytes = self.0.lock();
        let from = at as usize;
        let Some(part) = bytes.get(from..from + out.len()) else {
            return_errno!(Errno::EFAULT);
        };
        out.copy_from_slice(part);
        Ok(())
    }

    fn write(&self, at: u64, data: &[u8]) -> Result<()> {
        let mut bytes = self.0.lock();
        let from = at as usize;
        let Some(part) = bytes.get_mut(from..from + data.len()) else {
            return_errno!(Errno::EFAULT);
        };
        part.copy_from_slice(data);
        Ok(())
    }
}

/// A slot in memory: the records or windows it gives, and the records put on it.
struct MemSlot {
    given: SpinLock<VecDeque<Vec<u8>>>,
    put: SpinLock<Vec<Vec<u8>>>,
    interrupted: AtomicBool,
}

impl Default for MemSlot {
    fn default() -> Self {
        Self {
            given: SpinLock::new(VecDeque::new()),
            put: SpinLock::new(Vec::new()),
            interrupted: AtomicBool::new(false),
        }
    }
}

impl MemSlot {
    fn giving(records: &[Vec<u8>]) -> Self {
        let slot = Self::default();
        slot.given.lock().extend(records.iter().cloned());
        slot
    }
}

impl Slot for Arc<MemSlot> {
    type Region = MemRegion;

    fn wait(&self) -> Result<()> {
        if self.interrupted.load(Ordering::Relaxed) {
            return_errno!(Errno::EINTR);
        }
        Ok(())
    }

    fn ready(&self) -> bool {
        !self.given.lock().is_empty()
    }

    fn get(&self) -> Result<Option<MemRegion>> {
        Ok(self
            .given
            .lock()
            .pop_front()
            .map(|bytes| MemRegion(Arc::new(SpinLock::new(bytes)))))
    }

    fn put(&self, window: MemRegion, len: u64) -> Result<()> {
        let bytes = window.0.lock();
        self.put.lock().push(bytes[..len as usize].to_vec());
        Ok(())
    }
}

fn errno<T>(result: Result<T>) -> Errno {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.error(),
    }
}

/// The record `Output::put` lays out for the table's batch.
fn record() -> Vec<u8> {
    let slot = Arc::new(MemSlot::giving(&[vec![0u8; WINDOW]]));
    let output = Output::new(slot.clone());
    output.put(SCHEMA, BATCH, BODY).unwrap();
    let mut put = slot.put.lock();
    assert_eq!(put.len(), 1);
    put.pop().unwrap()
}

#[ktest]
fn output_frames_the_stream_s_messages() {
    let output = Output::new(Arc::new(MemSlot::default()));
    assert_eq!(output.frame(SCHEMA).unwrap(), (MessageKind::Schema, 0));
    assert_eq!(
        output.frame(BATCH).unwrap(),
        (MessageKind::RecordBatch, BODY.len())
    );
    assert_eq!(errno(output.frame(&[0xff; 4])), Errno::EINVAL);
    assert_eq!(errno(output.frame(&[0u8; 16])), Errno::EINVAL);
}

#[ktest]
fn output_puts_a_record_the_check_accepts() {
    let record = record();
    let need = moruna_ipc_check::record::framing_need(&record).unwrap();
    let moruna_ipc_check::record::Need::Framing(framing) = need else {
        panic!("the record's framing is not whole: {need:?}");
    };
    let checked =
        moruna_ipc_check::record::check_record(&record[..framing], record.len() as u64).unwrap();
    // The record's body is laid out on its own pages, the batch's body at its start.
    assert!(checked.body_length as usize >= BODY.len());
    let body = checked.body_start as usize;
    assert!(record[body..].windows(BODY.len()).any(|w| w == BODY));
}

#[ktest]
fn output_refuses_what_it_cannot_put() {
    // No window: the engine closed the slot.
    let closed = Output::new(Arc::new(MemSlot::default()));
    assert_eq!(errno(closed.put(SCHEMA, BATCH, BODY)), Errno::ENOSPC);
    // A window too small for the record.
    let small = Output::new(Arc::new(MemSlot::giving(&[vec![0u8; 64]])));
    assert_eq!(errno(small.put(SCHEMA, BATCH, BODY)), Errno::EFBIG);
    // A body shorter than the batch says.
    let short = Output::new(Arc::new(MemSlot::giving(&[vec![0u8; WINDOW]])));
    assert_eq!(errno(short.put(SCHEMA, BATCH, &BODY[..8])), Errno::EINVAL);
    // Not a schema and a batch.
    let wrong = Output::new(Arc::new(MemSlot::giving(&[vec![0u8; WINDOW]])));
    assert_eq!(errno(wrong.put(BATCH, SCHEMA, BODY)), Errno::EINVAL);
    // A signal while waiting for a window.
    let slot = Arc::new(MemSlot::giving(&[vec![0u8; WINDOW]]));
    slot.interrupted.store(true, Ordering::Relaxed);
    assert_eq!(
        errno(Output::new(slot).put(SCHEMA, BATCH, BODY)),
        Errno::EINTR
    );
}

#[ktest]
fn input_gives_the_first_record_whole_and_the_rest_from_their_batch() {
    let record = record();
    let slot = Arc::new(MemSlot::giving(&[record.clone(), record.clone()]));
    let input = Input::new(slot.clone());
    assert_eq!(errno(input.read(0, &mut [0u8; 1])), Errno::EINVAL);
    assert!(input.ready());

    let first = input.next(true).unwrap().unwrap();
    let mut whole = vec![0u8; first as usize];
    input.read(0, &mut whole).unwrap();
    assert!(whole.starts_with(&SCHEMA[..8]));
    assert!(whole.ends_with(BODY));

    let later = input.next(false).unwrap().unwrap();
    assert!(later < first);
    let mut part = vec![0u8; later as usize];
    input.read(0, &mut part).unwrap();
    assert_eq!(&part[..4], &[0xff; 4]);
    assert_eq!(part.len(), whole.len() - (first - later) as usize);
    assert_eq!(&part[..], &whole[(first - later) as usize..]);

    // The engine closed the slot.
    assert!(!input.ready());
    assert_eq!(input.next(false).unwrap(), None);
    assert_eq!(errno(input.read(0, &mut [0u8; 1])), Errno::EINVAL);
}

#[ktest]
fn input_refuses_a_record_that_is_not_one() {
    let input = Input::new(Arc::new(MemSlot::giving(&[vec![0u8; 64]])));
    assert_eq!(errno(input.next(true)), Errno::EIO);
    let slot = Arc::new(MemSlot::giving(&[record()]));
    slot.interrupted.store(true, Ordering::Relaxed);
    assert_eq!(errno(Input::new(slot).next(true)), Errno::EINTR);
}

/// 64 bytes on a 64-byte boundary: a ring starts on one.
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(64))]
struct Block([u8; 64]);

/// A ring's bytes in memory.
struct MemRing(SpinLock<Vec<Block>>);

impl MemRing {
    fn new(bytes: usize) -> Self {
        Self(SpinLock::new(vec![Block([0; 64]); bytes / 64]))
    }
}

impl RingBytes for Arc<MemRing> {
    fn with<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let mut blocks = self.0.lock();
        Some(f(blocks.as_mut_slice().as_mut_bytes()))
    }
}

#[ktest]
fn text_lines_become_records_in_the_ring() {
    let ring = Arc::new(MemRing::new(64 << 10));
    assert_eq!(
        errno(RingText::new(ring.clone(), "task".into())),
        Errno::EINVAL
    );
    ring.with(|bytes| Ring::init(bytes).map(|_| ()))
        .unwrap()
        .unwrap();
    let text = RingText::new(ring.clone(), "task".into()).unwrap();
    assert!(text.line(1, b"hello"));
    assert!(text.line(2, b"oops"));
    let popped = ring
        .with(|bytes| {
            let ring = Ring::attach(bytes).unwrap();
            let mut consumer = ring.consumer().unwrap();
            let mut n = 0;
            while consumer.pop().is_some() {
                n += 1;
            }
            n
        })
        .unwrap();
    assert_eq!(popped, 2);
    // Bytes that stop being a ring drop the line.
    ring.with(|bytes| bytes.fill(0)).unwrap();
    assert!(!text.line(1, b"lost"));
}
