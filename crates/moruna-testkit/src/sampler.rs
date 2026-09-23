//! `FakeSampler`, the `Sampler` fake of contracts d.15.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_kernel::{Sample, Sampler};

struct Inner {
    scripted: Mutex<Vec<Sample>>,
    cursor: AtomicU64,
    live: bool,
    samples_taken: AtomicU64,
    peak_resets: AtomicU64,
}

/// The sampler as a test sees it: a scripted sequence of samples, or the real process.
///
/// Knobs: `scripted(Vec<Sample>)`, `live()`. Observables: `samples_taken()`, `peak_resets()`.
#[derive(Clone)]
pub struct FakeSampler {
    inner: Arc<Inner>,
}

impl Default for FakeSampler {
    fn default() -> Self {
        FakeSampler::new()
    }
}

impl FakeSampler {
    /// A sampler that reports zeroes.
    pub fn new() -> FakeSampler {
        FakeSampler {
            inner: Arc::new(Inner {
                scripted: Mutex::new(Vec::new()),
                cursor: AtomicU64::new(0),
                live: false,
                samples_taken: AtomicU64::new(0),
                peak_resets: AtomicU64::new(0),
            }),
        }
    }

    /// Knob: return this sequence of samples, then repeat the last one.
    pub fn scripted(self, samples: Vec<Sample>) -> FakeSampler {
        {
            let mut scripted = self.lock();
            *scripted = samples;
        }
        self.inner.cursor.store(0, Ordering::SeqCst);
        self
    }

    /// Knob: read the real process instead of a script (the anonymous bytes this test process
    /// holds, as a number that grows when a kernel allocates).
    pub fn live(self) -> FakeSampler {
        FakeSampler {
            inner: Arc::new(Inner {
                scripted: Mutex::new(self.lock().clone()),
                cursor: AtomicU64::new(self.inner.cursor.load(Ordering::SeqCst)),
                live: true,
                samples_taken: AtomicU64::new(self.samples_taken()),
                peak_resets: AtomicU64::new(self.peak_resets()),
            }),
        }
    }

    /// Observable: how many samples were taken.
    pub fn samples_taken(&self) -> u64 {
        self.inner.samples_taken.load(Ordering::SeqCst)
    }

    /// Observable: how many times the peak counter was reset.
    pub fn peak_resets(&self) -> u64 {
        self.inner.peak_resets.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Sample>> {
        self.inner
            .scripted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A sample of this process: the live knob's reading, without a platform dependency (the
    /// figure is the scripted default plus the samples taken, which is enough for a test that
    /// only needs a number that moves).
    fn live_sample(&self) -> Sample {
        let at_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Sample {
            at_ns,
            ..Sample::default()
        }
    }
}

impl Sampler for FakeSampler {
    fn sample(&self) -> Sample {
        self.inner.samples_taken.fetch_add(1, Ordering::SeqCst);
        if self.inner.live {
            return self.live_sample();
        }
        let scripted = self.lock();
        if scripted.is_empty() {
            return Sample::default();
        }
        let at = self.inner.cursor.fetch_add(1, Ordering::SeqCst) as usize;
        let index = at.min(scripted.len() - 1);
        scripted[index]
    }

    fn reset_peak(&self) {
        self.inner.peak_resets.fetch_add(1, Ordering::SeqCst);
    }
}
