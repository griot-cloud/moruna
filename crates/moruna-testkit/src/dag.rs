//! What a test of a whole run's DAG needs (contracts d.15, d.16): random DAGs, and a kernel whose
//! output size no prediction can know, for the property tests of the buffer catalog and the
//! scheduling policies (placement PL-T37, scheduler SC-T33) and the policy matrix (SC-B1).

// `MorunaError` carries a morsel's features (CT-I10), so it is large; its shape is the
// contract's (d.14), not this module's.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moruna_kernel::arrow::array::{Array, ArrayData, ArrayRef, Int64Array, make_array};
use moruna_kernel::arrow::datatypes::DataType;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Consumer, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, Link, MorunaError, NoState,
    Payload, PayloadKind, PayloadSpec, Producer, Result, SourceSchema, TierPref, Topology,
};

use crate::FakeAllocator;

/// A source of draws below a bound, so a test's own generator drives `random_topology`.
pub trait Draw {
    /// A value below `bound` (0 when `bound` is 0).
    fn below(&mut self, bound: u64) -> u64;
}

/// A deterministic xorshift generator: the same seed draws the same numbers on every machine.
pub struct Rng(u64);

impl Rng {
    /// Seed it.
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

impl Draw for Rng {
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

/// A random DAG of one to `sources` sources and one to `stages` stages: a stage takes one or two
/// ports (fan-in), each from a source or an earlier stage, so producers feeding several
/// consumers fan out; every producer nothing else takes gives a sink, and some others do too.
pub fn random_topology(draw: &mut impl Draw, sources: u16, stages: u16) -> Topology {
    let sources = 1 + draw.below(u64::from(sources.max(1))) as u16;
    let stages = 1 + draw.below(u64::from(stages.max(1))) as u16;
    let mut links = Vec::new();
    for stage in 1..=stages {
        let ports = if draw.below(3) == 0 { 2 } else { 1 };
        for port in 0..ports {
            let choices = u64::from(sources) + u64::from(stage - 1);
            let pick = draw.below(choices) as u16;
            let from = if pick < sources {
                Producer::Source(pick)
            } else {
                Producer::Kernel(pick - sources + 1)
            };
            links.push(Link {
                from,
                to: Consumer::Kernel { stage, port },
            });
        }
    }
    let producers: Vec<Producer> = (0..sources)
        .map(Producer::Source)
        .chain((1..=stages).map(Producer::Kernel))
        .collect();
    let mut sinks = 0u16;
    for producer in producers {
        let feeds = links.iter().any(|link| link.from == producer);
        if !feeds || draw.below(3) == 0 {
            links.push(Link {
                from: producer,
                to: Consumer::Sink(sinks),
            });
            sinks += 1;
        }
    }
    match Topology::new(sources, stages, sinks, links) {
        Ok(topology) => topology,
        Err(e) => panic!("random_topology built an invalid DAG: {e}"),
    }
}

/// The split a `FakeSource` value came from (d.15: row `i` of split `s` is `s * 1_000_000 + i`).
pub fn split_of(value: i64) -> i64 {
    value / 1_000_000
}

/// A kernel whose output is its input's rows tiled or cut to a length no prediction can know:
/// each batch's factor is drawn from its first value and the kernel's salt, between `min` and
/// `max` thousandths. It checks every input it is handed: one `Int64` column, every value from
/// one `FakeSource` split, as every batch the run makes from a source batch is; a batch that is
/// not is refused with a `Kernel` error, so a run that handed a consumer another batch's bytes
/// fails. Takes any number of ports, treats them alike, and builds its output in the arena.
///
/// Knobs: `factors(min, max)` in thousandths, `skewed(bool)` (one batch in sixteen at the top of
/// the range, the rest at the bottom), `latency(Duration)`, `salt(u64)`. Observables:
/// `applies()`, `rows_in()`, `rows_out()`.
#[derive(Clone)]
pub struct Resize {
    alloc: FakeAllocator,
    min: u64,
    max: u64,
    skewed: bool,
    latency: Duration,
    salt: u64,
    counts: Arc<[AtomicU64; 3]>,
}

impl Resize {
    /// The identity: a factor of one, no latency.
    pub fn new(alloc: FakeAllocator) -> Resize {
        Resize {
            alloc,
            min: 1000,
            max: 1000,
            skewed: false,
            latency: Duration::ZERO,
            salt: 0,
            counts: Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]),
        }
    }

    /// Knob: the factor's range, in thousandths of the input's rows.
    pub fn factors(mut self, min: u64, max: u64) -> Resize {
        self.min = min.min(max);
        self.max = max.max(min);
        self
    }

    /// Knob: most batches at the bottom of the range and one in sixteen at the top.
    pub fn skewed(mut self, skewed: bool) -> Resize {
        self.skewed = skewed;
        self
    }

    /// Knob: how long each `apply` spins, the worker time a batch costs.
    pub fn latency(mut self, latency: Duration) -> Resize {
        self.latency = latency;
        self
    }

    /// Knob: what makes two stages' factors differ for the same batch.
    pub fn salt(mut self, salt: u64) -> Resize {
        self.salt = salt;
        self
    }

    /// Observable: applies so far.
    pub fn applies(&self) -> u64 {
        self.counts[0].load(Ordering::SeqCst)
    }

    /// Observable: rows taken in.
    pub fn rows_in(&self) -> u64 {
        self.counts[1].load(Ordering::SeqCst)
    }

    /// Observable: rows made.
    pub fn rows_out(&self) -> u64 {
        self.counts[2].load(Ordering::SeqCst)
    }

    fn factor(&self, first: i64) -> u64 {
        let mut x = (first as u64) ^ self.salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let span = self.max - self.min + 1;
        if self.skewed {
            if x.is_multiple_of(16) {
                self.max
            } else {
                self.min
            }
        } else {
            self.min + x % span
        }
    }

    fn resize(&self, input: Payload) -> Result<Payload> {
        let Payload::Table(batch, _) = &input else {
            return Err(refused("a tensor, not a table"));
        };
        let Some(column) = batch
            .columns()
            .first()
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        else {
            return Err(refused("not one Int64 column"));
        };
        let rows = column.len();
        if rows == 0 {
            return Err(refused("an empty batch"));
        }
        let values: Vec<i64> = (0..rows).map(|i| column.value(i)).collect();
        let split = split_of(values[0]);
        if values.iter().any(|v| split_of(*v) != split) {
            return Err(refused("values of two splits in one batch"));
        }
        if !self.latency.is_zero() {
            let until = std::time::Instant::now() + self.latency;
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        }
        let out_rows = ((rows as u64 * self.factor(values[0])).div_ceil(1000)).max(1) as usize;
        let bytes: Vec<u8> = (0..out_rows)
            .flat_map(|i| values[i % rows].to_le_bytes())
            .collect();
        let buffer = self.alloc.arrow_buffer(&bytes, input.tier());
        let data = ArrayData::builder(DataType::Int64)
            .len(out_rows)
            .add_buffer(buffer)
            .build()
            .map_err(|e| refused_owned(format!("the output: {e}")))?;
        let column: ArrayRef = make_array(data);
        let out = RecordBatch::try_new(batch.schema(), vec![column])
            .map_err(|e| refused_owned(format!("the output: {e}")))?;
        self.counts[0].fetch_add(1, Ordering::SeqCst);
        self.counts[1].fetch_add(rows as u64, Ordering::SeqCst);
        self.counts[2].fetch_add(out_rows as u64, Ordering::SeqCst);
        Payload::table(out)
    }
}

fn refused(what: &str) -> MorunaError {
    refused_owned(what.to_string())
}

fn refused_owned(what: String) -> MorunaError {
    MorunaError::Kernel {
        stage: 0,
        seq: 0,
        msg: format!("Resize was handed {what}"),
        features: None,
    }
}

impl Kernel for Resize {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("moruna-testkit::Resize", &self.salt.to_le_bytes())
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.resize(input)
    }

    fn max_inputs(&self) -> u16 {
        8
    }

    fn apply_port(
        &self,
        _state: &mut dyn KernelState,
        _port: u16,
        input: Payload,
    ) -> Result<Payload> {
        self.resize(input)
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn output_schema_of(&self, inputs: &[SourceSchema]) -> Result<SourceSchema> {
        // Every input of a run built over `FakeSource` carries its one schema; the first is the
        // output's.
        match inputs.first() {
            Some(first) => Ok(first.clone()),
            None => Err(MorunaError::Plan("Resize takes at least one input".into())),
        }
    }
}
