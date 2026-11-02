//! Where warm-up's measurements come from (SDD 24 p.4). Its decisions (which units run, when
//! the sizes stop, when a phase's budget is spent, the ratios) are [`super::Run`]'s and take
//! every measurement and every second from a [`Bench`]: [`Harnessed`] runs the kernel in the
//! check's harness and reads the clock, the counters and the high-water marks; a test's bench
//! gives exact, known values, so the decisions are tested without a clock.

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use moruna_cost::{Data, Origin, Outcome, Target};
use moruna_kernel::{
    InitCtx, Kernel, KernelAlloc, KernelState, NoState, OutsideMemory, Payload, Sampler as _,
};

use super::cpu;
use super::data::{Generator, Keys, Spec};
use super::peak::{self, Process};
use super::{Maker, STREAM_START};
use crate::check::{Harness, land, outside_arena_bytes};

/// One measured unit, as a bench reports it.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::check) struct Measured {
    /// What the unit was given: its rows, bytes, the columns as they were drawn, the columns read.
    pub(super) data: Data,
    /// The instances it ran with.
    pub(super) workers: u32,
    /// What it measured.
    pub(super) outcome: Outcome,
    /// The first instance's checkpoint, seconds and bytes, when one was asked for.
    pub(super) checkpoint: Option<(f64, u64)>,
}

/// What warm-up took from the process, for charging; `None` where it cannot be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub(in crate::check) struct Usage {
    pub(super) worker_seconds: Option<f64>,
    pub(super) memory_peak_bytes: Option<u64>,
    pub(super) memory_byte_seconds: Option<u64>,
}

/// The source of warm-up's measurements and of its time.
pub(in crate::check) trait Bench {
    /// Warm-up's own seconds so far.
    fn elapsed(&self) -> f64;
    /// The check's own batches run from here, and are not warm-up's time.
    fn pause(&mut self);
    /// Warm-up again.
    fn resume(&mut self);
    /// One start (AR section 6.3): seconds from the making through the first one-row output;
    /// `attempt` 0 is cold, 1 warm.
    fn start(&mut self, attempt: usize) -> Result<f64, String>;
    /// One unit of `spec` at `workers` workers; the first instance's checkpoint timed when
    /// `checkpoint`.
    fn unit(&mut self, spec: &Spec, workers: u32, checkpoint: bool) -> Result<Measured, String>;
    /// What warm-up has taken from the process.
    fn usage(&self) -> Usage;
    /// What the bench says of how it measures.
    fn notes(&mut self) -> Vec<String>;
}

/// The kernel in the check's harness: the bench of a real warm-up.
pub(in crate::check) struct Harnessed<'a> {
    kernel: &'a Arc<dyn Kernel>,
    harness: &'a Harness,
    generator: Generator,
    maker: Option<Arc<dyn Maker>>,
    outside: Option<Arc<dyn OutsideMemory>>,
    stateful: bool,
    process: Result<Process, String>,
    accounting: Option<moruna_discovery::Sampler>,
    started: Instant,
    paused: Option<Instant>,
    excluded: Duration,
    /// Said once each: why a mark could not be reset.
    said: Vec<String>,
}

impl<'a> Harnessed<'a> {
    /// The bench of `kernel` in `harness`.
    pub(in crate::check) fn new(
        kernel: &'a Arc<dyn Kernel>,
        harness: &'a Harness,
        generator: Generator,
        maker: Option<Arc<dyn Maker>>,
        outside: Option<Arc<dyn OutsideMemory>>,
        stateful: bool,
    ) -> Harnessed<'a> {
        let accounting = moruna_discovery::discover(&moruna_discovery::DiscoveryInput::default())
            .ok()
            .and_then(|d| moruna_discovery::Sampler::new(&d).ok());
        Harnessed {
            kernel,
            harness,
            generator,
            maker,
            outside,
            stateful,
            process: Process::here(),
            accounting,
            started: Instant::now(),
            paused: None,
            excluded: Duration::ZERO,
            said: Vec::new(),
        }
    }

    fn sample(&self) -> Result<(), String> {
        if let Some(s) = &self.accounting {
            s.sample().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn say(&mut self, note: String) {
        if !self.said.contains(&note) {
            self.said.push(note);
        }
    }

    fn init(&self, kernel: &dyn Kernel, instance: usize) -> Result<Box<dyn KernelState>, String> {
        if !self.stateful {
            return Ok(Box::new(NoState));
        }
        kernel
            .init(&InitCtx {
                instance,
                device: None,
                alloc: self.harness.alloc.clone(),
            })
            .map_err(|e| format!("init of instance {instance} failed: {e}"))
    }
}

/// What a kernel's memory pools held at once over one call: NumPy's, pyarrow's and a wasm
/// instance's linear memory (contracts d.13); `None` when nothing counted the call.
fn pools(alloc: &KernelAlloc) -> Option<u64> {
    alloc
        .measured
        .then(|| alloc.numpy.peak + alloc.arrow.peak + alloc.linear.peak)
}

/// The high-water marks of one call, reset at its start (see `peak`).
struct Marks {
    arena_base: u64,
    counting_base: Option<i64>,
    process_base: Option<u64>,
    child_base: Option<u64>,
}

impl Bench for Harnessed<'_> {
    fn elapsed(&self) -> f64 {
        let now = self.started.elapsed().saturating_sub(self.excluded);
        let paused = self.paused.map_or(Duration::ZERO, |p| p.elapsed());
        now.saturating_sub(paused).as_secs_f64()
    }

    fn pause(&mut self) {
        self.paused = Some(Instant::now());
    }

    fn resume(&mut self) {
        if let Some(paused) = self.paused.take() {
            self.excluded += paused.elapsed();
        }
    }

    fn start(&mut self, attempt: usize) -> Result<f64, String> {
        let label = if attempt == 0 { "cold" } else { "warm" };
        self.sample()?;
        let generated = self.generator.generate(&Spec {
            rows: 1,
            keys: Keys::Uniform,
            distinct: None,
            stream: STREAM_START + attempt as u64,
        })?;
        let alloc = self.harness.alloc.as_ref();
        let landed = land(&generated.batch, alloc).map_err(|e| e.to_string())?;
        drop(generated);
        let started = Instant::now();
        let kernel = match &self.maker {
            Some(maker) => maker
                .make()
                .map_err(|e| format!("start-up ({label}): making the kernel failed: {e}"))?,
            None => Arc::clone(self.kernel),
        };
        let mut state = self
            .init(kernel.as_ref(), 0)
            .map_err(|e| format!("start-up ({label}): {e}"))?;
        let payload = Payload::table_with(landed, alloc).map_err(|e| e.to_string())?;
        let result = kernel.apply(state.as_mut(), payload);
        let seconds = started.elapsed().as_secs_f64();
        let _ = moruna_kernel::take_kernel_alloc();
        let first =
            result.map_err(|e| format!("start-up ({label}): the first invocation failed: {e}"));
        drop(first?);
        kernel
            .finish(state.as_mut())
            .map_err(|e| format!("start-up ({label}): finish failed: {e}"))?;
        self.sample()?;
        Ok(seconds)
    }

    /// The batch generated, landed in the arena and cut into one contiguous part per worker,
    /// each applied on its own thread to its own instance, all started together, as a run runs a
    /// morsel per worker. A failure names the worker and the kernel's error.
    fn unit(&mut self, spec: &Spec, workers: u32, checkpoint: bool) -> Result<Measured, String> {
        self.sample()?;
        let generated = self.generator.generate(spec)?;
        let columns_read = generated.columns.len() as u32;
        let columns = generated.columns;
        let batch = generated.batch;
        let kernel = self.kernel.as_ref();
        let alloc = self.harness.alloc.as_ref();
        let arena = &self.harness.arena;
        let rows_in = batch.num_rows() as u64;
        let arena_base = alloc.stats().host_in_use;
        let landed = land(&batch, alloc).map_err(|e| format!("landing the batch: {e}"))?;
        drop(batch);
        let bytes_in = landed.get_array_memory_size() as u64;
        let n = workers.max(1) as usize;
        let mut states = Vec::with_capacity(n);
        for instance in 0..n {
            states.push(self.init(kernel, instance)?);
        }
        let before = states
            .iter()
            .map(|s| s.footprint())
            .collect::<moruna_kernel::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        let total = landed.num_rows();
        let mut payloads = Vec::with_capacity(n);
        for part in 0..n {
            let (start, end) = (total * part / n, total * (part + 1) / n);
            payloads.push(
                Payload::table_with(landed.slice(start, end - start), alloc)
                    .map_err(|e| e.to_string())?,
            );
        }
        drop(landed);

        // Process-wide marks are valid only when no other warm-up overlaps this interval.
        let interval = peak::Interval::begin()?;
        arena.reset_host_peak();
        let counting_base = peak::counting_installed().then(peak::counting_reset);
        let process_base = match &self.process {
            Ok(process) => match process.reset() {
                Ok(base) => Some(base),
                Err(e) => {
                    self.say(format!(
                        "{} could not be reset ({e}), so the process's part of a unit's peak is \
                         unmeasured (no sampled replacement is used)",
                        process.describe()
                    ));
                    None
                }
            },
            Err(_) => None,
        };
        if let Some(sampler) = &self.harness.sampler {
            sampler.reset_peak();
        }
        let child_base = match &self.outside {
            Some(outside) => {
                let base = outside.in_use().map_err(|e| e.to_string())?;
                match outside.reset_peak() {
                    Ok(true) => Some(base),
                    Ok(false) => {
                        self.say("the packaged stage has no resettable peak; unit PeakMemory is unmeasured".into());
                        None
                    }
                    Err(e) => {
                        self.say(format!(
                            "the packaged stage's cgroup memory.peak could not be reset ({e}), \
                             so its child's part of a unit's peak is not measured"
                        ));
                        None
                    }
                }
            }
            None => None,
        };
        let marks = Marks {
            arena_base,
            counting_base,
            process_base,
            child_base,
        };

        let barrier = Barrier::new(n);
        let work = &|state: &mut Box<dyn KernelState>, payload: Payload| {
            barrier.wait();
            let cpu = cpu::thread_cpu_ns();
            let started = Instant::now();
            let result = kernel.apply(state.as_mut(), payload);
            let ended = Instant::now();
            let cpu = cpu
                .zip(cpu::thread_cpu_ns())
                .map(|(a, b)| b.saturating_sub(a));
            let counted = moruna_kernel::take_kernel_alloc();
            (result, started, ended, cpu, counted)
        };
        let results = std::thread::scope(|scope| {
            let mut slots = states.iter_mut();
            let mut parts = payloads.into_iter();
            let first = (slots.next(), parts.next());
            let handles: Vec<_> = slots
                .zip(parts)
                .map(|(state, payload)| scope.spawn(move || work(state, payload)))
                .collect();
            let mut results = Vec::with_capacity(n);
            if let (Some(state), Some(payload)) = first {
                results.push(Ok(work(state, payload)));
            }
            for handle in handles {
                results.push(handle.join().map_err(|_| "a worker panicked".to_string()));
            }
            results
        });
        let extra = self.extra(&marks);
        let arena_after = alloc.stats().host_in_use;
        let mut first_error: Option<String> = None;
        let mut outputs = Vec::with_capacity(n);
        let (mut started, mut ended): (Option<Instant>, Option<Instant>) = (None, None);
        let mut cpu_total = Some(0u64);
        let mut pooled = Some(0u64);
        for (worker, result) in results.into_iter().enumerate() {
            let (result, s, e, cpu, counted) = match result {
                Ok(r) => r,
                Err(why) => {
                    first_error.get_or_insert(format!("worker {worker}: {why}"));
                    continue;
                }
            };
            started = Some(started.map_or(s, |x| x.min(s)));
            ended = Some(ended.map_or(e, |x| x.max(e)));
            cpu_total = cpu_total.zip(cpu).map(|(a, b)| a + b);
            pooled = pooled.zip(pools(&counted)).map(|(a, b)| a + b);
            match result {
                Ok(Payload::Table(out, _)) => outputs.push(out),
                Ok(Payload::Tensor(_, _)) => {
                    first_error.get_or_insert(format!(
                        "worker {worker}: the kernel returned a tensor for a table declaration"
                    ));
                }
                Err(e) => {
                    first_error.get_or_insert(format!("worker {worker}: {e}"));
                }
            }
        }
        let mut taken = None;
        if first_error.is_none() && checkpoint {
            match checkpoint_of(&mut states) {
                Ok(c) => taken = Some(c),
                Err(e) => first_error = Some(e),
            }
        }
        let rows_out: u64 = outputs.iter().map(|o| o.num_rows() as u64).sum();
        let bytes_out: u64 = outputs
            .iter()
            .map(|o| o.get_array_memory_size() as u64)
            .sum();
        let outside: u64 = outputs.iter().map(|o| outside_arena_bytes(o, alloc)).sum();
        let after = states
            .iter()
            .map(|s| s.footprint())
            .collect::<moruna_kernel::Result<Vec<_>>>();
        drop(outputs);
        let mut finish_error = None;
        for (instance, state) in states.iter_mut().enumerate() {
            if let Err(e) = kernel.finish(state.as_mut()) {
                finish_error.get_or_insert(format!("finish of instance {instance} failed: {e}"));
            }
        }
        if let Err(cause) = &after {
            finish_error = Some(match finish_error {
                Some(previous) => format!("{previous}; state footprint: {cause}"),
                None => format!("state footprint: {cause}"),
            });
        }
        match (first_error, finish_error) {
            (Some(e), Some(f)) => return Err(format!("{e}; and {f}")),
            (Some(e), None) | (None, Some(e)) => return Err(e),
            (None, None) => {}
        }
        let after = after.map_err(|e| e.to_string())?;
        let wall = match (started, ended) {
            (Some(s), Some(e)) => e.duration_since(s).as_secs_f64(),
            _ => return Err("no worker ran".into()),
        };
        self.sample()?;
        // The arena's mark includes the input it holds (it was landed above the base); the
        // process's and the child's are what the call added. They are disjoint memories (the
        // arena is pre-faulted, so its use never moves the process's mark), so their sum bounds
        // the call's peak from above; the pools are a view of the same bytes, taken when larger.
        let peak = if interval.finish()? {
            extra.map(|extra| extra.max(bytes_in + pooled.unwrap_or(0)))
        } else {
            self.say("overlapping warm-ups invalidated process-wide marks; unit PeakMemory is unmeasured".into());
            None
        };
        let retained = outside + arena_after.saturating_sub(arena_base + bytes_in);
        let mut outcome = Outcome::new(Origin::WarmUp);
        outcome
            .set(Target::Duration, wall)
            .set(Target::RetainedMemory, retained as f64)
            .set(Target::RowsOut, rows_out as f64)
            .set(Target::BytesOut, bytes_out as f64);
        if let Some(peak) = peak {
            outcome.set(Target::PeakMemory, peak as f64);
        }
        if let Some(cpu) = cpu_total {
            outcome.set(Target::WorkerTime, cpu as f64 / 1e9);
        }
        let growth = if self.stateful {
            before
                .iter()
                .zip(&after)
                .map(|(b, a)| b.zip(*a).map(|(b, a)| a.saturating_sub(b)))
                .sum::<Option<u64>>()
        } else {
            Some(0)
        };
        if let Some(growth) = growth {
            outcome.set(Target::StateGrowth, growth as f64);
        }
        Ok(Measured {
            data: Data {
                rows: Some(rows_in),
                bytes: Some(bytes_in),
                columns,
                columns_read: Some(columns_read),
                ..Data::default()
            },
            workers: n as u32,
            outcome,
            checkpoint: taken,
        })
    }

    fn usage(&self) -> Usage {
        let usage = self.accounting.as_ref().map(|s| s.process_usage());
        let measured = usage.filter(|u| u.measured);
        Usage {
            worker_seconds: measured.map(|u| u.cpu_ns as f64 / 1e9),
            memory_peak_bytes: self
                .accounting
                .as_ref()
                .map(|s| s.process_peak())
                .filter(|p| p.exact)
                .map(|p| p.bytes),
            memory_byte_seconds: measured.map(|u| u.mem_byte_seconds),
        }
    }

    fn notes(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let process = match &self.process {
            Ok(process) => process.describe().to_string(),
            Err(why) => {
                format!("an unavailable process mark ({why}); unit PeakMemory is unmeasured")
            }
        };
        let counting = if peak::counting_installed() {
            ", or Moruna's counting allocator's mark of the process's heap when that is higher"
        } else {
            "; Moruna's counting allocator is not installed in this program, so memory a library \
             frees and reuses inside a call without returning it to the system can be missed"
        };
        notes.push(format!(
            "a unit's peak memory is an upper bound from resettable high-water marks: the \
             arena's mark, plus {process}{counting}, plus a packaged stage's cgroup \
             memory.peak; or what the kernel's memory pools held at once, when more. Missing \
             required marks or overlapping intervals leave PeakMemory absent, never sampled"
        ));
        if self.accounting.is_none() {
            notes.push(
                "this process's CPU time and memory over warm-up cannot be read here, so they \
                 are not reported"
                    .into(),
            );
        }
        notes.append(&mut self.said);
        notes
    }
}

impl Harnessed<'_> {
    /// The call's peak from its marks: the arena's above its base (the input with it), plus the
    /// process's and the child's above theirs.
    fn extra(&mut self, marks: &Marks) -> Option<u64> {
        let arena = self
            .harness
            .arena
            .host_peak()
            .saturating_sub(marks.arena_base);
        let os = match (&self.process, marks.process_base) {
            (Ok(process), Some(base)) => match process.peak() {
                Ok(peak) => Some(peak.saturating_sub(base)),
                Err(e) => {
                    self.say(format!(
                        "reading {} failed ({e}); unit PeakMemory is unmeasured",
                        process.describe()
                    ));
                    None
                }
            },
            _ => None,
        };
        let counting = marks
            .counting_base
            .map_or(0, |base| (peak::counting_peak() - base).max(0) as u64);
        let child = match (&self.outside, marks.child_base) {
            (Some(outside), Some(base)) => {
                let mark = match outside.peak() {
                    Ok(mark) => mark,
                    Err(e) => {
                        self.say(format!("reading the packaged stage's memory.peak failed ({e}); unit PeakMemory is unmeasured"));
                        None
                    }
                };
                if mark.is_none() {
                    self.say("the packaged stage returned no peak after reset; unit PeakMemory is unmeasured".into());
                }
                mark.map(|peak| peak.saturating_sub(base))
            }
            (Some(_), None) => None,
            (None, _) => Some(0),
        };
        os.zip(child)
            .and_then(|(os, child)| arena.checked_add(os.max(counting))?.checked_add(child))
    }
}

/// The first instance's checkpoint, timed: seconds and bytes.
fn checkpoint_of(states: &mut [Box<dyn KernelState>]) -> Result<(f64, u64), String> {
    let state = states
        .first_mut()
        .ok_or_else(|| "a unit with no instance".to_string())?;
    let started = Instant::now();
    match state.checkpoint() {
        Ok(Some(bytes)) => Ok((started.elapsed().as_secs_f64(), bytes.len() as u64)),
        Ok(None) => Err(
            "the kind's resume policy is to checkpoint, and its checkpoint returned nothing"
                .to_string(),
        ),
        Err(e) => Err(format!("the checkpoint failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
    use moruna_testkit::FakeKernel;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct UnmeasuredChild {
        reset: bool,
        reset_error: bool,
        read_error: bool,
        resets: AtomicUsize,
        reads: AtomicUsize,
    }

    impl OutsideMemory for UnmeasuredChild {
        fn in_use(&self) -> moruna_kernel::Result<u64> {
            Ok(1 << 20)
        }
        fn reset_peak(&self) -> moruna_kernel::Result<bool> {
            self.resets.fetch_add(1, Ordering::Relaxed);
            if self.reset_error {
                Err(moruna_kernel::MorunaError::Io {
                    op: "reset memory.peak",
                    target: "controlled child".into(),
                    msg: "controlled child peak reset failure".into(),
                })
            } else {
                Ok(self.reset)
            }
        }
        fn peak(&self) -> moruna_kernel::Result<Option<u64>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.read_error {
                Err(moruna_kernel::MorunaError::Io {
                    op: "memory.peak",
                    target: "controlled child".into(),
                    msg: "controlled child peak read failure".into(),
                })
            } else {
                Ok(None)
            }
        }
    }

    fn spec() -> Spec {
        Spec {
            rows: 8,
            keys: Keys::Uniform,
            distinct: None,
            stream: 45,
        }
    }

    #[test]
    fn cm_t68_an_unavailable_process_mark_does_not_train_sampled_peak() {
        let _serial = peak::TEST_MARK_LOCK.lock().expect("marker fixture lock");
        for (process, reason) in [
            (
                Err("controlled unavailable high-water counter".into()),
                "controlled unavailable high-water counter",
            ),
            (
                Ok(Process::Controlled {
                    reset: Err("controlled high-water reset failure".into()),
                    peak: Ok(0),
                }),
                "controlled high-water reset failure",
            ),
            (
                Ok(Process::Controlled {
                    reset: Ok(0),
                    peak: Err("controlled high-water read failure".into()),
                }),
                "controlled high-water read failure",
            ),
        ] {
            let harness = Harness::start(&[], 1 << 20).expect("real harness");
            let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
            let generator = Generator::new(&schema, None, 45).expect("generator");
            let kernel: Arc<dyn Kernel> = Arc::new(FakeKernel::new());
            let mut bench = Harnessed::new(&kernel, &harness, generator, None, None, false);
            bench.process = process;
            let measured = bench
                .unit(&spec(), 1, false)
                .expect("other quantities still measured");
            assert!(measured.outcome.get(Target::Duration).is_some());
            assert!(measured.outcome.get(Target::BytesOut).is_some());
            assert_eq!(
                measured.outcome.get(Target::PeakMemory),
                None,
                "a sampler/arena reading must not become a total per-unit peak"
            );
            assert!(bench.notes().iter().any(|n| n.contains(reason)));
        }
    }

    #[test]
    fn cm_t68_an_unmeasured_child_does_not_train_partial_peak() {
        let _serial = peak::TEST_MARK_LOCK.lock().expect("marker fixture lock");
        for (reset, reset_error, read_error, reason) in [
            (false, false, false, "no resettable peak"),
            (true, true, false, "controlled child peak reset failure"),
            (true, false, false, "returned no peak after reset"),
            (true, false, true, "controlled child peak read failure"),
        ] {
            let harness = Harness::start(&[], 1 << 20).expect("real harness");
            let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
            let generator = Generator::new(&schema, None, 45).expect("generator");
            let kernel: Arc<dyn Kernel> = Arc::new(FakeKernel::new());
            let counted = Arc::new(UnmeasuredChild {
                reset,
                reset_error,
                read_error,
                resets: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
            });
            let child: Arc<dyn OutsideMemory> = counted.clone();
            let mut bench = Harnessed::new(&kernel, &harness, generator, None, Some(child), false);
            bench.process = Ok(Process::Controlled {
                reset: Ok(0),
                peak: Ok(0),
            });
            let measured = bench
                .unit(&spec(), 1, false)
                .expect("other quantities still measured");
            assert!(measured.outcome.get(Target::Duration).is_some());
            assert_eq!(
                measured.outcome.get(Target::PeakMemory),
                None,
                "a parent-only measurement cannot stand in for the child (reset={reset})"
            );
            assert_eq!(counted.resets.load(Ordering::Relaxed), 1);
            assert_eq!(
                counted.reads.load(Ordering::Relaxed),
                usize::from(reset && !reset_error)
            );
            assert!(bench.notes().iter().any(|n| n.contains(reason)));
        }
    }
}
