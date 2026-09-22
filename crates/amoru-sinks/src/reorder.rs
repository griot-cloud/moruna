//! `ReorderBuffer`, the wrapper that delivers morsels to any sink in sequence order (08 f.4).

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use amoru_kernel::{
    AmoruError, BoxFuture, Payload, PayloadSpec, Result, Seq, Sink, SinkSummary, SourceSchema,
};

use crate::stats::SinkStats;

/// One morsel waiting for its turn, and the task that is waiting with it.
///
/// The payload stays here until the buffer forwards it, so the `write` future that produced it
/// has not resolved and the payload's arena bytes have not been released (SI-I1). A reorder
/// buffer that resolved early would be holding bytes it had promised to be rid of.
struct Slot {
    payload: Option<Payload>,
    waker: Option<Waker>,
    bytes: u64,
    /// Whether these bytes count against the bound. A morsel that arrives on its turn is
    /// passed straight through and is never held out of order, so it does not.
    counted: bool,
}

struct State {
    next_expected: Seq,
    held: BTreeMap<Seq, Slot>,
    skipped_ahead: BTreeSet<Seq>,
    held_bytes: u64,
    buffer_bytes: u64,
    stalled: bool,
    held_max: u64,
    stalls: u64,
}

impl State {
    /// Move `next_expected` over sequence numbers the scheduler declared skipped, then wake
    /// whichever task now holds the turn. A held payload always wins over a skip of the same
    /// sequence number, so a morsel that was written is never dropped.
    fn advance(&mut self) {
        loop {
            if self.held.contains_key(&self.next_expected) {
                break;
            }
            if self.skipped_ahead.remove(&self.next_expected) {
                self.next_expected += 1;
                continue;
            }
            break;
        }
        self.skipped_ahead = self.skipped_ahead.split_off(&self.next_expected);
        if let Some(slot) = self.held.get_mut(&self.next_expected)
            && let Some(waker) = slot.waker.take()
        {
            waker.wake();
        }
    }

    /// The stall flag clears at half the bound, so admission does not flap around it (f.4).
    fn relax(&mut self) {
        if self.stalled && self.held_bytes <= self.buffer_bytes / 2 {
            self.stalled = false;
        }
    }
}

/// Delivers to `inner` in strictly increasing `seq` within `buffer_bytes`. `S` is `?Sized` so
/// the run's handle is `ReorderBuffer<dyn Sink>` over a boxed sink (`Box<dyn Sink>` cannot
/// itself implement the foreign `Sink` trait here).
pub struct ReorderBuffer<S: Sink + ?Sized> {
    inner: Box<S>,
    state: Mutex<State>,
}

impl<S: Sink + ?Sized> ReorderBuffer<S> {
    /// Wrap `inner`, holding at most `buffer_bytes` of out-of-order morsels before stalling.
    pub fn new(inner: Box<S>, buffer_bytes: u64) -> Self {
        ReorderBuffer {
            inner,
            state: Mutex::new(State {
                next_expected: 0,
                held: BTreeMap::new(),
                skipped_ahead: BTreeSet::new(),
                held_bytes: 0,
                buffer_bytes,
                stalled: false,
                held_max: 0,
                stalls: 0,
            }),
        }
    }

    /// True while the buffer holds more than `buffer_bytes`; the scheduler reads it each
    /// admission cycle and stops admitting source work until the missing sequence arrives.
    pub fn is_stalled(&self) -> bool {
        self.lock().stalled
    }

    /// The sequence number the buffer is waiting for.
    pub fn next_expected(&self) -> Seq {
        self.lock().next_expected
    }

    /// Bytes held out of order right now.
    pub fn held_bytes(&self) -> u64 {
        self.lock().held_bytes
    }

    /// The reorder counters of section j; the other fields belong to the inner sink.
    pub fn stats(&self) -> SinkStats {
        let state = self.lock();
        SinkStats {
            reorder_held_max: state.held_max,
            stalls: state.stalls,
            ..SinkStats::default()
        }
    }

    /// The sink this buffer wraps.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take `seq` into the buffer, or refuse it (f.4).
    fn admit(&self, seq: Seq, payload: Payload) -> Result<()> {
        let bytes = payload.bytes();
        let mut state = self.lock();
        if seq < state.next_expected
            || state.skipped_ahead.contains(&seq)
            || state.held.contains_key(&seq)
        {
            return Err(AmoruError::Sink("sequence out of range".into()));
        }
        let counted = seq != state.next_expected;
        state.held.insert(
            seq,
            Slot {
                payload: Some(payload),
                waker: None,
                bytes,
                counted,
            },
        );
        if counted {
            state.held_bytes += bytes;
            if state.held_bytes > state.held_max {
                state.held_max = state.held_bytes;
            }
        }
        // The bound is soft by one morsel and hard thereafter: the morsel stays held and the
        // scheduler stops admitting, which is what keeps the buffer from growing (SI-I5).
        if state.held_bytes > state.buffer_bytes && !state.stalled {
            state.stalled = true;
            state.stalls += 1;
            tracing::warn!(
                target: "sink.stall",
                held_bytes = state.held_bytes,
                buffer_bytes = state.buffer_bytes,
                next_expected = state.next_expected,
                "reorder buffer stalled"
            );
        }
        state.advance();
        Ok(())
    }

    /// Take the payload back out once its turn has come.
    fn take(&self, seq: Seq) -> Result<Payload> {
        let mut state = self.lock();
        let Some(mut slot) = state.held.remove(&seq) else {
            return Err(AmoruError::Sink("sequence out of range".into()));
        };
        if slot.counted {
            state.held_bytes -= slot.bytes;
        }
        state.relax();
        slot.payload
            .take()
            .ok_or_else(|| AmoruError::Sink("sequence out of range".into()))
    }
}

/// Resolves when the buffer is waiting for `seq`.
struct Turn<'a, S: Sink + ?Sized> {
    buffer: &'a ReorderBuffer<S>,
    seq: Seq,
}

impl<S: Sink + ?Sized> core::future::Future for Turn<'_, S> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.buffer.lock();
        if state.next_expected == self.seq {
            return Poll::Ready(());
        }
        let seq = self.seq;
        if let Some(slot) = state.held.get_mut(&seq) {
            slot.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl<S: Sink + ?Sized> Sink for ReorderBuffer<S> {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        self.inner.open(schema)
    }

    fn accepts(&self) -> PayloadSpec {
        self.inner.accepts()
    }

    fn requires_order(&self) -> bool {
        true
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.admit(seq, payload)?;
            Turn { buffer: self, seq }.await;
            let payload = self.take(seq)?;
            let outcome = self.inner.write(seq, payload).await;
            // The turn passes whether the inner write succeeded or not: a failure that held the
            // buffer shut would strand every morsel behind it as well.
            let mut state = self.lock();
            state.next_expected = seq + 1;
            state.relax();
            state.advance();
            drop(state);
            outcome
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish()
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.inner.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        self.inner.skip(seq);
        let mut state = self.lock();
        if seq < state.next_expected {
            return;
        }
        if seq == state.next_expected && !state.held.contains_key(&seq) {
            state.next_expected += 1;
        } else {
            state.skipped_ahead.insert(seq);
        }
        state.advance();
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        self.inner.checkpoint()
    }

    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        self.inner.resume(schema, state, committed_seq)?;
        let mut own = self.lock();
        own.next_expected = committed_seq.map_or(0, |w| w + 1);
        own.held.clear();
        own.skipped_ahead.clear();
        own.held_bytes = 0;
        own.stalled = false;
        Ok(())
    }
}
