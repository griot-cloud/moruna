//! `ReorderBuffer` and `SinkHandle`: SI-T5, SI-T15 (08 k).

mod common;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use moruna_kernel::{MorunaError, Seq, Sink, SourceSchema};
use moruna_sinks::{ReorderBuffer, SinkHandle};
use moruna_testkit::{FakeAllocator, FakeSink};
use common::{arena_payload, block_on, table_source_schema};

/// A deterministic shuffle, so a failure is reproducible without a random number generator.
fn shuffled(n: u64, seed: u64) -> Vec<Seq> {
    let mut order: Vec<Seq> = (0..n).collect();
    let mut state = seed | 1;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

/// SI-T5. A thousand sequences arriving in a random order are delivered to the inner sink in
/// strictly increasing order, the held bytes stay within the bound plus one morsel, the stall
/// flag rises and clears, and two sequences declared skipped before their turn advance the
/// buffer without waiting. SI-I5, f.4.
#[test]
fn si_t5_reorder() {
    let alloc = FakeAllocator::new();
    let morsel = arena_payload(&alloc, 16, 0).bytes();
    let buffer_bytes = morsel * 20;
    let fake = FakeSink::new();
    let buffer: ReorderBuffer<dyn Sink> =
        ReorderBuffer::new(Box::new(fake.clone()) as Box<dyn Sink>, buffer_bytes);

    // The scheduler declares 7 and 23 skipped before either could be written (f.4).
    buffer.skip(7);
    buffer.skip(23);

    let order: Vec<Seq> = shuffled(1_000, 0x9E3779B97F4A7C15)
        .into_iter()
        .filter(|seq| *seq != 7 && *seq != 23)
        .collect();
    let stalled_at_least_once = drive(&buffer, &alloc, &order);

    let written = fake.written();
    assert_eq!(written.len(), 998, "every morsel but the two skips arrived");
    assert!(
        written.windows(2).all(|w| w[0] < w[1]),
        "the inner sink saw the sequences out of order"
    );
    assert_eq!(fake.skipped(), vec![7, 23]);
    assert_eq!(buffer.next_expected(), 1_000);
    assert_eq!(buffer.held_bytes(), 0);
    assert!(!buffer.is_stalled(), "the stall cleared at the end");
    assert!(stalled_at_least_once, "a bound this small must stall");
    let stats = buffer.stats();
    assert!(stats.stalls > 0);
    assert!(
        stats.reorder_held_max <= buffer_bytes + morsel,
        "held {} bytes, the bound is {buffer_bytes} plus one morsel of {morsel}",
        stats.reorder_held_max
    );
}

/// Admit morsels the way the scheduler does: freely while the buffer is not stalled, and while
/// it is stalled only the sequence it is waiting for, which is the morsel already in flight
/// downstream that the stall is waiting on. Returns whether the buffer ever stalled.
fn drive(buffer: &ReorderBuffer<dyn Sink>, alloc: &FakeAllocator, order: &[Seq]) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    type Pending<'a> = Pin<Box<dyn Future<Output = moruna_kernel::Result<()>> + Send + 'a>>;
    let mut futures: Vec<Option<Pending<'_>>> = Vec::new();
    let mut admitted = vec![false; order.len()];
    let mut left = order.len();
    let mut ever_stalled = false;
    while left > 0 || futures.iter().any(|f| f.is_some()) {
        for (i, seq) in order.iter().enumerate() {
            if admitted[i] {
                continue;
            }
            let stalled = buffer.is_stalled();
            ever_stalled |= stalled;
            if stalled && *seq != buffer.next_expected() {
                continue;
            }
            admitted[i] = true;
            left -= 1;
            // A `write` future admits the morsel on its first poll, so the first poll happens
            // here: the admission decision above has to see the buffer the morsel just entered.
            let mut future: Pending<'_> = buffer.write(*seq, arena_payload(alloc, 16, 0));
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(outcome) => outcome.expect("the inner sink accepted the morsel"),
                Poll::Pending => futures.push(Some(future)),
            }
        }
        for slot in futures.iter_mut() {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            if let Poll::Ready(outcome) = future.as_mut().poll(&mut cx) {
                outcome.expect("the inner sink accepted the morsel");
                *slot = None;
            }
        }
    }
    ever_stalled
}

/// A write for a sequence already skipped, or below the one the buffer is waiting for, is
/// refused rather than delivered out of order (f.4).
#[test]
fn a_sequence_out_of_range_is_refused() {
    let alloc = FakeAllocator::new();
    let fake = FakeSink::new();
    let buffer: ReorderBuffer<dyn Sink> =
        ReorderBuffer::new(Box::new(fake.clone()) as Box<dyn Sink>, 1 << 20);
    buffer.skip(0);
    buffer.skip(3);
    assert_eq!(buffer.next_expected(), 1);

    let refused = block_on(buffer.write(0, arena_payload(&alloc, 4, 0)));
    assert!(matches!(refused, Err(MorunaError::Sink(_))), "{refused:?}");
    let refused = block_on(buffer.write(3, arena_payload(&alloc, 4, 0)));
    assert!(matches!(refused, Err(MorunaError::Sink(_))), "{refused:?}");

    block_on(buffer.write(1, arena_payload(&alloc, 4, 0))).expect("write");
    block_on(buffer.write(2, arena_payload(&alloc, 4, 0))).expect("write");
    // 3 was skipped ahead of its turn, so the buffer steps over it without waiting.
    assert_eq!(buffer.next_expected(), 4);
    assert_eq!(fake.written(), vec![1, 2]);
}

/// SI-T15. `SinkHandle::wrap` chooses the shape, and every `Sink` method called on the handle
/// reaches the sink underneath. f.9, d.1.
#[test]
fn si_t15_sink_handle() {
    let alloc = FakeAllocator::new();

    let fake = FakeSink::new().resumable(true);
    let plain = SinkHandle::wrap(Box::new(fake.clone()), false, 1 << 20);
    assert!(!plain.is_ordered());
    assert!(!plain.is_stalled());
    assert_eq!(plain.next_expected(), None);
    assert!(!plain.requires_order());

    let mut plain = plain;
    plain.open(&table_source_schema()).expect("open");
    block_on(plain.write(0, arena_payload(&alloc, 4, 0))).expect("write");
    plain.skip(1);
    assert_eq!(fake.open_calls(), 1);
    assert_eq!(fake.written(), vec![0]);
    assert_eq!(
        fake.skipped(),
        vec![1],
        "a plain handle passes skip through"
    );
    assert_eq!(plain.committed_seq(), fake.committed_seq());
    assert!(plain.checkpoint().expect("checkpoint").is_some());
    assert_eq!(plain.accepts(), fake.accepts());
    plain
        .resume(&table_source_schema(), b"", Some(0))
        .expect("resume");
    assert_eq!(fake.resume_calls(), 1);
    plain.finish().expect("finish");
    assert_eq!(fake.finish_calls(), 1);

    // `ordered = true` wraps, and so does a sink that asks for order itself.
    let asked = SinkHandle::wrap(Box::new(FakeSink::new()), true, 1 << 20);
    assert!(asked.is_ordered());
    assert_eq!(asked.next_expected(), Some(0));
    assert!(asked.requires_order());

    let inner = FakeSink::new().requires_order(true);
    let ordered = SinkHandle::wrap(Box::new(inner.clone()), false, 1 << 20);
    assert!(ordered.is_ordered());
    assert!(!ordered.is_stalled());

    let mut ordered = ordered;
    ordered.open(&table_source_schema()).expect("open");
    block_on(ordered.write(0, arena_payload(&alloc, 4, 0))).expect("write");
    ordered.skip(1);
    block_on(ordered.write(2, arena_payload(&alloc, 4, 0))).expect("write");
    assert_eq!(inner.written(), vec![0, 2]);
    assert_eq!(inner.skipped(), vec![1]);
    assert_eq!(ordered.next_expected(), Some(3));
    assert_eq!(ordered.committed_seq(), inner.committed_seq());
    assert!(ordered.checkpoint().expect("checkpoint").is_none());
    let refused = ordered.resume(&table_source_schema(), b"", Some(1));
    assert!(matches!(refused, Err(MorunaError::Resume(_))), "{refused:?}");
    ordered.finish().expect("finish");
    assert_eq!(inner.finish_calls(), 1);
}

/// A reorder buffer over a resumable sink restarts at the watermark the scheduler restored
/// and delegates the three resume methods to the sink underneath (d.1).
#[test]
fn a_reorder_buffer_resumes_at_the_watermark() {
    let inner = FakeSink::new().resumable(true);
    let mut buffer: ReorderBuffer<dyn Sink> =
        ReorderBuffer::new(Box::new(inner.clone()) as Box<dyn Sink>, 1 << 20);
    let schema: SourceSchema = table_source_schema();
    buffer.open(&schema).expect("open");
    assert!(buffer.checkpoint().expect("checkpoint").is_some());
    buffer.resume(&schema, b"", Some(41)).expect("resume");
    assert_eq!(buffer.next_expected(), 42);
    assert_eq!(inner.resume_calls(), 1);

    let mut buffer: ReorderBuffer<dyn Sink> = ReorderBuffer::new(
        Box::new(FakeSink::new().resumable(true)) as Box<dyn Sink>,
        1 << 20,
    );
    buffer.resume(&schema, b"", None).expect("resume");
    assert_eq!(buffer.next_expected(), 0);
}
