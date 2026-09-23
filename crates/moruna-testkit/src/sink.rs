//! `FakeSink`, the `Sink` fake of contracts d.15.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moruna_kernel::{
    MorunaError, BoxFuture, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkSummary,
    SourceSchema, TierPref,
};

#[derive(Default)]
struct State {
    written: Vec<Seq>,
    skipped: Vec<Seq>,
    rows: u64,
    bytes: u64,
    committed: Option<Seq>,
    state_bytes: Vec<u8>,
}

struct Inner {
    state: Mutex<State>,
    commit_every: u64,
    resumable: bool,
    fail_at: Option<Seq>,
    latency: Duration,
    requires_order: bool,
    open_calls: AtomicU64,
    resume_calls: AtomicU64,
    finish_calls: AtomicU64,
    shutdown_calls: AtomicU64,
}

/// The sink as a test sees it: it remembers the sequence numbers it was given and advances its
/// commit watermark in blocks, so a resume test can see uncommitted output (d.15).
///
/// Knobs: `commit_every(n)`, `resumable(bool)`, `fail_at(seq)`, `latency(Duration)`,
/// `requires_order(bool)`. Observables: `written()`, `skipped()`, `committed_seq()`,
/// `open_calls()`, `resume_calls()`, `finish_calls()`, `shutdown_calls()`.
///
/// `Sink` has no `shutdown` method (d.8), so `shutdown_calls()` counts this fake's own
/// inherent `shutdown()`, which a facade-level test calls to prove the shutdown sequence of
/// preamble 4.3 (escalation on the pull request that adds this crate).
#[derive(Clone)]
pub struct FakeSink {
    inner: Arc<Inner>,
}

impl Default for FakeSink {
    fn default() -> Self {
        FakeSink::new()
    }
}

impl FakeSink {
    /// A sink that commits every write, is not resumable and never fails.
    pub fn new() -> FakeSink {
        FakeSink {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                commit_every: 1,
                resumable: false,
                fail_at: None,
                latency: Duration::ZERO,
                requires_order: false,
                open_calls: AtomicU64::new(0),
                resume_calls: AtomicU64::new(0),
                finish_calls: AtomicU64::new(0),
                shutdown_calls: AtomicU64::new(0),
            }),
        }
    }

    /// Knob: commit in blocks of `n` sequence numbers, so `committed_seq` advances in steps.
    pub fn commit_every(self, n: u64) -> FakeSink {
        self.rebuild(|b| b.commit_every = n.max(1))
    }

    /// Knob: whether the sink supports resume (`checkpoint` then returns `Some`, even before
    /// the first write, which is how SC f.11 tells a resumable sink from one that is not).
    pub fn resumable(self, resumable: bool) -> FakeSink {
        self.rebuild(|b| b.resumable = resumable)
    }

    /// Knob: writing this sequence number fails with `Sink`.
    pub fn fail_at(self, seq: Seq) -> FakeSink {
        self.rebuild(|b| b.fail_at = Some(seq))
    }

    /// Knob: how long a write takes to complete.
    pub fn latency(self, latency: Duration) -> FakeSink {
        self.rebuild(|b| b.latency = latency)
    }

    /// Knob: whether the sink needs morsels in sequence order.
    pub fn requires_order(self, requires_order: bool) -> FakeSink {
        self.rebuild(|b| b.requires_order = requires_order)
    }

    /// Observable: the sequence numbers written, in call order.
    pub fn written(&self) -> Vec<Seq> {
        self.lock().written.clone()
    }

    /// Observable: the sequence numbers the scheduler declared skipped.
    pub fn skipped(&self) -> Vec<Seq> {
        self.lock().skipped.clone()
    }

    /// Observable: how many times `open` was called.
    pub fn open_calls(&self) -> u64 {
        self.inner.open_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many times `resume` was called.
    pub fn resume_calls(&self) -> u64 {
        self.inner.resume_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many times `finish` was called.
    pub fn finish_calls(&self) -> u64 {
        self.inner.finish_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many times this fake's `shutdown` was called.
    pub fn shutdown_calls(&self) -> u64 {
        self.inner.shutdown_calls.load(Ordering::SeqCst)
    }

    /// The shutdown the `Sink` trait does not have; counted by `shutdown_calls`.
    pub fn shutdown(&self) {
        self.inner.shutdown_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakeSink {
        let mut builder = Builder {
            commit_every: self.inner.commit_every,
            resumable: self.inner.resumable,
            fail_at: self.inner.fail_at,
            latency: self.inner.latency,
            requires_order: self.inner.requires_order,
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakeSink {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                commit_every: builder.commit_every,
                resumable: builder.resumable,
                fail_at: builder.fail_at,
                latency: builder.latency,
                requires_order: builder.requires_order,
                open_calls: AtomicU64::new(self.open_calls()),
                resume_calls: AtomicU64::new(self.resume_calls()),
                finish_calls: AtomicU64::new(self.finish_calls()),
                shutdown_calls: AtomicU64::new(self.shutdown_calls()),
            }),
        }
    }

    /// Advance the watermark to the highest sequence number whose whole block has been written
    /// or skipped.
    fn recompute_committed(&self, state: &mut State) {
        let mut seen: Vec<Seq> = state
            .written
            .iter()
            .chain(state.skipped.iter())
            .copied()
            .collect();
        seen.sort_unstable();
        let mut committed = None;
        let mut contiguous = 0u64;
        for (index, seq) in seen.iter().enumerate() {
            if *seq != index as u64 {
                break;
            }
            contiguous += 1;
        }
        if contiguous > 0 {
            let blocks = contiguous / self.inner.commit_every;
            if blocks > 0 {
                committed = Some(blocks * self.inner.commit_every - 1);
            }
        }
        state.committed = committed;
    }
}

struct Builder {
    commit_every: u64,
    resumable: bool,
    fail_at: Option<Seq>,
    latency: Duration,
    requires_order: bool,
}

impl Sink for FakeSink {
    fn open(&mut self, _schema: &SourceSchema) -> Result<()> {
        self.inner.open_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Host,
        }
    }

    fn requires_order(&self) -> bool {
        self.inner.requires_order
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        let latency = self.inner.latency;
        let failing = self.inner.fail_at == Some(seq);
        let rows = payload.rows();
        let bytes = payload.bytes();
        let this = self.clone();
        Box::pin(async move {
            if !latency.is_zero() {
                std::thread::sleep(latency);
            }
            if failing {
                return Err(MorunaError::Sink(format!("FakeSink::fail_at({seq})")));
            }
            let mut state = this.lock();
            state.written.push(seq);
            state.rows += rows;
            state.bytes += bytes;
            this.recompute_committed(&mut state);
            Ok(())
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        self.inner.finish_calls.fetch_add(1, Ordering::SeqCst);
        let state = self.lock();
        Ok(SinkSummary {
            rows: state.rows,
            bytes: state.bytes,
            files: vec!["fake-sink-0".to_string()],
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        self.lock().committed
    }

    fn skip(&self, seq: Seq) {
        let mut state = self.lock();
        state.skipped.push(seq);
        self.recompute_committed(&mut state);
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        if !self.inner.resumable {
            return Ok(None);
        }
        let state = self.lock();
        let mut bytes = Vec::with_capacity(8 + state.written.len() * 8);
        bytes.extend_from_slice(&(state.written.len() as u64).to_le_bytes());
        for seq in &state.written {
            bytes.extend_from_slice(&seq.to_le_bytes());
        }
        Ok(Some(bytes))
    }

    fn resume(
        &mut self,
        _schema: &SourceSchema,
        state_bytes: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        if !self.inner.resumable {
            return Err(MorunaError::Resume("FakeSink is not resumable".into()));
        }
        self.inner.resume_calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.lock();
        state.state_bytes = state_bytes.to_vec();
        // Output above the watermark is discarded, exactly as d.8 requires of a real sink.
        state
            .written
            .retain(|seq| committed_seq.is_some_and(|watermark| *seq <= watermark));
        state.committed = committed_seq;
        Ok(())
    }
}
