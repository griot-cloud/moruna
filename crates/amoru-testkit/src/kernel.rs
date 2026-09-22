//! `FakeKernel`, the `Kernel` fake of contracts d.15.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use amoru_kernel::{
    AmoruError, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, Payload,
    PayloadKind, PayloadSpec, Result, ResumePolicy, SourceSchema, TierPref,
};

/// One `apply`: its index, the instance that ran it and the thread it ran on (d.15).
///
/// `Kernel::apply` is handed a `Payload` (d.7), which carries no sequence number and no worker
/// id, and that is deliberate: a kernel that knew its position in the run could not be the same
/// code inside a Polars expression or a DataFusion function (S7). The scheduler holds the
/// sequence number, attaches it to `AmoruError::Kernel` and writes it to the trace, so a test
/// that wants "morsel 7 failed" asserts on the trace and on the sink's `skipped()`.
pub type ApplyRecord = (usize, usize, ThreadId);

/// The state one `FakeKernel` instance keeps.
pub struct FakeState {
    /// The instance index this state belongs to.
    pub instance: usize,
    bytes: u64,
    grow_by: u64,
    checkpoint_calls: Arc<AtomicU64>,
    ballast: Vec<u8>,
}

impl KernelState for FakeState {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn checkpoint(&mut self) -> Result<Option<Vec<u8>>> {
        self.checkpoint_calls.fetch_add(1, Ordering::SeqCst);
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&(self.instance as u64).to_le_bytes());
        bytes.extend_from_slice(&self.bytes.to_le_bytes());
        Ok(Some(bytes))
    }

    fn footprint(&self) -> Option<u64> {
        Some(self.bytes)
    }
}

#[derive(Default)]
struct State {
    applies: Vec<ApplyRecord>,
}

struct Inner {
    state: Mutex<State>,
    amplification: f64,
    latency: Duration,
    instances: usize,
    state_bytes: u64,
    resume: ResumePolicy,
    fail_on: Vec<usize>,
    panic_on: Vec<usize>,
    grow_state_by: u64,
    init_calls: AtomicU64,
    restore_calls: AtomicU64,
    checkpoint_calls: Arc<AtomicU64>,
}

/// The kernel as a test sees it: identity by default, with knobs for the behaviour the
/// controller and the scheduler are sized against.
///
/// Knobs: `amplification(f64)`, `latency(Duration)`, `stateful(instances, state_bytes)`,
/// `resume(ResumePolicy)`, `fail_on(applies)`, `panic_on(applies)`, `grow_state_by(bytes)`.
/// Observables: `applies()`, `init_calls()`, `restore_calls()`, `checkpoint_calls()`.
#[derive(Clone)]
pub struct FakeKernel {
    inner: Arc<Inner>,
}

impl Default for FakeKernel {
    fn default() -> Self {
        FakeKernel::new()
    }
}

impl FakeKernel {
    /// A stateless kernel that returns its input unchanged and allocates nothing.
    pub fn new() -> FakeKernel {
        FakeKernel {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                amplification: 0.0,
                latency: Duration::ZERO,
                instances: 0,
                state_bytes: 0,
                resume: ResumePolicy::Reinit,
                fail_on: Vec::new(),
                panic_on: Vec::new(),
                grow_state_by: 0,
                init_calls: AtomicU64::new(0),
                restore_calls: AtomicU64::new(0),
                checkpoint_calls: Arc::new(AtomicU64::new(0)),
            }),
        }
    }

    /// Knob: allocate `a * bytes_in` from the global allocator during `apply`, freed on return.
    pub fn amplification(self, a: f64) -> FakeKernel {
        self.rebuild(|b| b.amplification = a)
    }

    /// Knob: how long `apply` takes.
    pub fn latency(self, latency: Duration) -> FakeKernel {
        self.rebuild(|b| b.latency = latency)
    }

    /// Knob: make the kernel stateful, with this instance pool size and this much state per
    /// instance.
    pub fn stateful(self, instances: usize, state_bytes: u64) -> FakeKernel {
        self.rebuild(|b| {
            b.instances = instances;
            b.state_bytes = state_bytes;
        })
    }

    /// Knob: what the hints declare about resuming this kernel.
    pub fn resume(self, resume: ResumePolicy) -> FakeKernel {
        self.rebuild(|b| b.resume = resume)
    }

    /// Knob: these applies, by index, fail with `Kernel` (d.15: the fake counts applies,
    /// because a kernel never sees a sequence number).
    pub fn fail_on(self, applies: &[usize]) -> FakeKernel {
        let applies = applies.to_vec();
        self.rebuild(|b| b.fail_on = applies)
    }

    /// Knob: these applies, by index, panic, so a test can prove a worker survives one.
    pub fn panic_on(self, applies: &[usize]) -> FakeKernel {
        let applies = applies.to_vec();
        self.rebuild(|b| b.panic_on = applies)
    }

    /// Knob: the instance's state grows by this many bytes on every apply, so the controller
    /// sees state that morsels make bigger (RC f.3).
    pub fn grow_state_by(self, bytes: u64) -> FakeKernel {
        self.rebuild(|b| b.grow_state_by = bytes)
    }

    /// Observable: one record per apply, in call order.
    pub fn applies(&self) -> Vec<ApplyRecord> {
        self.lock().applies.clone()
    }

    /// Observable: how many instances were built with `init`.
    pub fn init_calls(&self) -> u64 {
        self.inner.init_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many instances were rebuilt with `restore`.
    pub fn restore_calls(&self) -> u64 {
        self.inner.restore_calls.load(Ordering::SeqCst)
    }

    /// Observable: how many times an instance's state was checkpointed.
    pub fn checkpoint_calls(&self) -> u64 {
        self.inner.checkpoint_calls.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn rebuild(self, f: impl FnOnce(&mut Builder)) -> FakeKernel {
        let mut builder = Builder {
            amplification: self.inner.amplification,
            latency: self.inner.latency,
            instances: self.inner.instances,
            state_bytes: self.inner.state_bytes,
            resume: self.inner.resume,
            fail_on: self.inner.fail_on.clone(),
            panic_on: self.inner.panic_on.clone(),
            grow_state_by: self.inner.grow_state_by,
        };
        f(&mut builder);
        let state = std::mem::take(&mut *self.lock());
        FakeKernel {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                amplification: builder.amplification,
                latency: builder.latency,
                instances: builder.instances,
                state_bytes: builder.state_bytes,
                resume: builder.resume,
                fail_on: builder.fail_on,
                panic_on: builder.panic_on,
                grow_state_by: builder.grow_state_by,
                init_calls: AtomicU64::new(self.init_calls()),
                restore_calls: AtomicU64::new(self.restore_calls()),
                checkpoint_calls: Arc::clone(&self.inner.checkpoint_calls),
            }),
        }
    }

    fn new_state(&self, instance: usize) -> Box<dyn KernelState> {
        Box::new(FakeState {
            instance,
            bytes: self.inner.state_bytes,
            grow_by: self.inner.grow_state_by,
            checkpoint_calls: Arc::clone(&self.inner.checkpoint_calls),
            ballast: vec![0u8; 0],
        })
    }
}

struct Builder {
    amplification: f64,
    latency: Duration,
    instances: usize,
    state_bytes: u64,
    resume: ResumePolicy,
    fail_on: Vec<usize>,
    panic_on: Vec<usize>,
    grow_state_by: u64,
}

impl Kernel for FakeKernel {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute(
            "amoru-testkit::FakeKernel",
            &self.inner.state_bytes.to_le_bytes(),
        )
    }

    fn kind(&self) -> KernelKind {
        match core::num::NonZeroUsize::new(self.inner.instances) {
            Some(max_instances) => KernelKind::Stateful { max_instances },
            None => KernelKind::Stateless,
        }
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(self.inner.amplification),
            uses_device_memory: false,
            releases_gil: None,
            preferred_rows: None,
            resume: self.inner.resume,
            state_bytes: Some(self.inner.state_bytes),
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        self.inner.init_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.new_state(ctx.instance))
    }

    fn restore(&self, ctx: &InitCtx, state: &[u8]) -> Result<Box<dyn KernelState>> {
        if self.inner.resume != ResumePolicy::Checkpoint {
            return Err(AmoruError::Resume(format!(
                "FakeKernel declares {:?}, so it cannot be restored",
                self.inner.resume
            )));
        }
        self.inner.restore_calls.fetch_add(1, Ordering::SeqCst);
        let mut restored = self.new_state(ctx.instance);
        if state.len() == 16
            && let Some(fake) = restored.as_any_mut().downcast_mut::<FakeState>()
        {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&state[8..16]);
            fake.bytes = u64::from_le_bytes(bytes);
        }
        Ok(restored)
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        let call = {
            let mut recorded = self.lock();
            let call = recorded.applies.len();
            let instance = state
                .as_any_mut()
                .downcast_mut::<FakeState>()
                .map_or(usize::MAX, |fake| fake.instance);
            recorded
                .applies
                .push((call, instance, std::thread::current().id()));
            call
        };
        if self.inner.panic_on.contains(&call) {
            panic!("FakeKernel::panic_on({call})");
        }
        if !self.inner.latency.is_zero() {
            std::thread::sleep(self.inner.latency);
        }
        if let Some(fake) = state.as_any_mut().downcast_mut::<FakeState>() {
            fake.bytes += fake.grow_by;
            if fake.grow_by > 0 {
                fake.ballast.resize(fake.bytes as usize, 0);
            }
        }
        if self.inner.fail_on.contains(&call) {
            return Err(AmoruError::Kernel {
                stage: 0,
                seq: call as u64,
                msg: format!("FakeKernel::fail_on({call})"),
                features: None,
            });
        }
        if self.inner.amplification > 0.0 {
            // The working set the probe is meant to measure: allocated during `apply` and
            // freed before it returns.
            let bytes = (input.bytes() as f64 * self.inner.amplification) as usize;
            let scratch = vec![0u8; bytes];
            std::hint::black_box(&scratch);
        }
        Ok(input)
    }
}
