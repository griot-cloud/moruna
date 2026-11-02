//! A run of a DAG over the engine, driven the way the scheduler drives it, for PL-T37 and the
//! `known_future` bench (f.18).
//!
//! Sources push batches into every queue they give; a kernel stage takes a batch from any of its
//! input ports, holds it while its unit runs, releases it and pushes one output batch into every
//! queue it gives; sinks take and release. Every push is admitted under a claim reserved first
//! (`Residency::reserve`), as the adaptive scheduler's admission will (adaptive-runtime 9.4), so
//! the bytes the catalog holds and claims never pass the budget. Every batch carries values that
//! name its producer and number, and every take checks them, so a consumer that gets another
//! batch's bytes, or a batch twice, fails the run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moruna_kernel::{CatalogStats, Locality, Morsel, Placement, Producer, Seq, StageId, Topology};
use moruna_kernel::{Reservation, Reserve, Residency};
use moruna_placement::catalog::EvictionOrder;
use moruna_placement::{PlacementConfig, PlacementEngine};
use moruna_testkit::{FakeAllocator, FakeReactor};

use super::Rng;

/// What a run is: its DAG and how many batches each source reads.
#[derive(Clone, Debug)]
pub struct Shape {
    /// The DAG.
    pub topology: Topology,
    /// Batches per source.
    pub batches: Vec<u64>,
    /// Rows per batch: the smallest, and how many more a batch may have.
    pub rows: (usize, usize),
}

/// How a run is driven.
#[derive(Clone, Debug)]
pub struct Drive {
    /// Units running at once at most.
    pub workers: usize,
    /// The host tier's budget in bytes.
    pub budget: u64,
    /// Per queue, staging on (a source's queue with staging off drops and re-reads).
    pub staging: Vec<bool>,
    /// The eviction order.
    pub order: Arc<dyn EvictionOrder>,
    /// Every IO the fake reactor resolves after this long; zero resolves on the calling thread.
    pub latency: Duration,
    /// Every queue's host water marks set to this, when given: with the budget itself, no queue
    /// sheds for its own share and only the catalog's budget holds memory back.
    pub water: Option<u64>,
    /// Sources are tried before anything else, so they run ahead of their consumers until the
    /// catalog refuses their claims, as a source with read-ahead does: the whole working set
    /// presses memory at once.
    pub sources_first: bool,
    /// Every queue's promotion window: the entries from the head a consumer is about to take,
    /// which never leave memory for pressure (PL-I20).
    pub window: u16,
}

/// What a run measured.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The catalog's figures at the end.
    pub catalog: CatalogStats,
    /// Takes checked.
    pub takes: u64,
    /// Batches pushed (one per source read and one per unit).
    pub batches: u64,
    /// Wall time from the first push to the last release.
    pub wall: Duration,
}

impl moruna_testkit::dag::Draw for Rng {
    fn below(&mut self, bound: u64) -> u64 {
        Rng::below(self, bound)
    }
}

/// The values batch `seq` of `producer` carries.
pub fn content(producer: Producer, seq: Seq, rows: (usize, usize)) -> Vec<i32> {
    let tag: u64 = match producer {
        Producer::Source(i) => u64::from(i),
        Producer::Kernel(s) => 100 + u64::from(s),
    };
    let mut x = (tag << 40) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let count = rows.0 + (next() as usize) % rows.1.max(1);
    (0..count).map(|_| next() as i32).collect()
}

/// The largest bytes a batch of this shape counts: what every claim reserves.
pub fn max_bytes(alloc: &FakeAllocator, rows: (usize, usize)) -> u64 {
    let widest = vec![0i32; rows.0 + rows.1.max(1)];
    super::valued_morsel(alloc, 0, 0, &widest).bytes
}

struct Unit {
    stage: StageId,
    port: u16,
    queue: StageId,
    seq: Seq,
    claim: Reservation,
}

struct Run<'a> {
    engine: Arc<PlacementEngine>,
    alloc: &'a FakeAllocator,
    shape: &'a Shape,
    budget: u64,
    claim: u64,
    host: moruna_kernel::Tier,
    next: Vec<u64>,
    running: Vec<Unit>,
    pushed: BTreeMap<StageId, Vec<Seq>>,
    taken: BTreeMap<StageId, Vec<Seq>>,
    seen: BTreeSet<(StageId, Seq)>,
    takes: u64,
    batches: u64,
}

impl Run<'_> {
    fn morsel(&self, producer: Producer, seq: Seq) -> Morsel {
        super::valued_morsel(self.alloc, seq, 0, &content(producer, seq, self.shape.rows))
    }

    fn producer(&self, queue: StageId) -> Producer {
        match self.shape.topology.link(queue) {
            Some(link) => link.from,
            None => panic!("queue {queue} is not in the DAG"),
        }
    }

    fn reserve(&self) -> Option<Reservation> {
        match self.engine.reserve(self.host, self.claim) {
            Ok(Reserve::Granted(claim)) => Some(claim),
            Ok(Reserve::Pending { .. } | Reserve::Refused { .. }) => None,
            Err(e) => panic!("reserve: {e}"),
        }
    }

    /// The hard rule (adaptive-runtime 12.5, PL-I22), after every step.
    fn check(&self, label: &str) {
        let catalog = self.engine.stats().catalog;
        let slot = self.host.index();
        let held = catalog.resident_by_tier[slot] + catalog.reserved_by_tier[slot];
        assert!(
            held <= self.budget,
            "{label}: {} resident + {} reserved > budget {}",
            catalog.resident_by_tier[slot],
            catalog.reserved_by_tier[slot],
            self.budget
        );
        assert!(
            catalog.peak_by_tier[slot] <= self.budget,
            "{label}: the peak {} passed the budget {}",
            catalog.peak_by_tier[slot],
            self.budget
        );
    }

    /// A consumer took `morsel` from `queue`: the right bytes, once, in push order.
    fn took(&mut self, queue: StageId, morsel: &Morsel) {
        let producer = self.producer(queue);
        assert!(
            self.seen.insert((queue, morsel.seq)),
            "batch {} reached queue {queue}'s consumer twice",
            morsel.seq
        );
        assert_eq!(
            super::values_of(morsel),
            content(producer, morsel.seq, self.shape.rows),
            "queue {queue}'s consumer got the wrong bytes for batch {}",
            morsel.seq
        );
        self.taken.entry(queue).or_default().push(morsel.seq);
        self.takes += 1;
    }

    fn push(&mut self, queues: &[StageId], morsel: Morsel, claim: Reservation) {
        let seq = morsel.seq;
        if let Err(e) = self.engine.push_reserved(queues, morsel, claim) {
            panic!("push of {seq} into {queues:?}: {e}");
        }
        for &q in queues {
            self.pushed.entry(q).or_default().push(seq);
        }
        self.batches += 1;
    }

    fn source_push(&mut self, source: u16) -> bool {
        let at = usize::from(source);
        if self.next[at] >= self.shape.batches[at] {
            return false;
        }
        let Some(claim) = self.reserve() else {
            return false;
        };
        let seq = self.next[at];
        self.next[at] += 1;
        let morsel = self.morsel(Producer::Source(source), seq);
        let queues = self.shape.topology.source_outputs(source).to_vec();
        self.push(&queues, morsel, claim);
        true
    }

    fn start(&mut self, stage: StageId, workers: usize, rng: &mut Rng) -> bool {
        if self.running.len() >= workers {
            return false;
        }
        let inputs = self.shape.topology.inputs(stage).to_vec();
        let ready: Vec<(u16, StageId)> = inputs
            .iter()
            .enumerate()
            .filter(|(_, q)| {
                self.engine
                    .peek_resident(**q, super::want_host(), Locality::Any)
            })
            .map(|(port, q)| (port as u16, *q))
            .collect();
        if ready.is_empty() {
            return false;
        }
        let (port, queue) = ready[rng.below(ready.len() as u64) as usize];
        let Some(claim) = self.reserve() else {
            return false;
        };
        let morsel = match self.engine.pop(queue, super::want_host(), Locality::Any) {
            Ok(Some(morsel)) => morsel,
            Ok(None) => return false,
            Err(e) => panic!("pop {queue}: {e}"),
        };
        self.took(queue, &morsel);
        self.running.push(Unit {
            stage,
            port,
            queue,
            seq: morsel.seq,
            claim,
        });
        true
    }

    fn finish(&mut self, at: usize) -> bool {
        let unit = self.running.swap_remove(at);
        if let Err(e) = self.engine.release(unit.queue, unit.seq) {
            panic!("release {}/{}: {e}", unit.queue, unit.seq);
        }
        let seq = self.shape.topology.out_seq(unit.stage, unit.port, unit.seq);
        let morsel = self.morsel(Producer::Kernel(unit.stage), seq);
        let queues = self.shape.topology.outputs(unit.stage).to_vec();
        self.push(&queues, morsel, unit.claim);
        true
    }

    fn sink_take(&mut self, queue: StageId) -> bool {
        let morsel = match self.engine.pop(queue, super::want_host(), Locality::Any) {
            Ok(Some(morsel)) => morsel,
            Ok(None) => return false,
            Err(e) => panic!("pop {queue}: {e}"),
        };
        self.took(queue, &morsel);
        if let Err(e) = self.engine.release(queue, morsel.seq) {
            panic!("release {queue}/{}: {e}", morsel.seq);
        }
        true
    }

    fn replace(&mut self, queue: StageId) -> bool {
        let Some(&(seq, _)) = self.engine.evicted(queue).first() else {
            return false;
        };
        let Some(claim) = self.reserve() else {
            return false;
        };
        let morsel = self.morsel(self.producer(queue), seq);
        if let Err(e) = self.engine.replace_reserved(queue, morsel, claim) {
            panic!("replace {queue}/{seq}: {e}");
        }
        true
    }

    fn done(&self) -> bool {
        self.running.is_empty()
            && self
                .next
                .iter()
                .zip(&self.shape.batches)
                .all(|(next, of)| next >= of)
            && self.engine.stats().queues.iter().all(|q| q.count == 0)
    }
}

/// Run `shape` to the end under `drive`, checking the hard rule after every step and the bytes
/// of every take, and every queue's takes against its pushes at the end.
pub fn run(shape: &Shape, drive: &Drive, rng: &mut Rng, label: &str) -> Outcome {
    let scratch = super::Scratch::new(label);
    let alloc = FakeAllocator::new();
    let reactor = FakeReactor::new().with_latency(drive.latency);
    let queues = shape.topology.queue_count();
    let mut cfg: PlacementConfig = super::config(
        1,
        Some(scratch.path().to_path_buf()),
        super::budgets(drive.budget, 0, 1 << 40),
    );
    cfg.topology = shape.topology.clone();
    cfg.fingerprints = (0..shape.topology.stages())
        .map(|s| moruna_kernel::Fingerprint::compute("stage", &s.to_le_bytes()))
        .collect();
    cfg.resume_policy =
        vec![moruna_kernel::ResumePolicy::Reinit; usize::from(shape.topology.stages())];
    cfg.plan_digests = vec![[0; 32]; usize::from(shape.topology.sources())];
    cfg.eviction = Arc::clone(&drive.order);
    let engine = super::engine(cfg, &alloc, &reactor);
    for q in 0..queues {
        engine.set_staging(q, drive.staging[usize::from(q)]);
        engine.set_promotion_window(q, drive.window);
        if let Some(water) = drive.water {
            engine.set_water(q, alloc.host_tier().kind(), water, water);
        }
    }
    let claim = max_bytes(&alloc, shape.rows);
    let mut run = Run {
        engine: Arc::clone(&engine),
        alloc: &alloc,
        shape,
        budget: drive.budget,
        claim,
        host: alloc.host_tier(),
        next: vec![0; usize::from(shape.topology.sources())],
        running: Vec::new(),
        pushed: BTreeMap::new(),
        taken: BTreeMap::new(),
        seen: BTreeSet::new(),
        takes: 0,
        batches: 0,
    };
    let sink_queues: Vec<StageId> = (0..queues)
        .filter(|&q| shape.topology.sink_of(q).is_some())
        .collect();
    let started = Instant::now();
    let mut idle_since: Option<Instant> = None;
    while !run.done() {
        // Every action that could make progress now, in a random order: the arrival order and
        // the interleaving are the property test's to vary.
        let mut actions: Vec<(u8, u16)> = Vec::new();
        for source in 0..shape.topology.sources() {
            actions.push((0, source));
        }
        for stage in 1..=shape.topology.stages() {
            actions.push((1, stage));
        }
        for at in 0..run.running.len() {
            actions.push((2, at as u16));
        }
        for &q in &sink_queues {
            actions.push((3, q));
        }
        for q in 0..queues {
            actions.push((4, q));
        }
        let fixed = if drive.sources_first {
            usize::from(shape.topology.sources())
        } else {
            0
        };
        for i in (fixed + 1..actions.len()).rev() {
            let j = fixed + rng.below((i - fixed) as u64 + 1) as usize;
            actions.swap(i, j);
        }
        let mut progressed = false;
        for (kind, arg) in actions {
            progressed = match kind {
                0 => run.source_push(arg),
                1 => run.start(arg, drive.workers, rng),
                2 => usize::from(arg) < run.running.len() && run.finish(usize::from(arg)),
                3 => run.sink_take(arg),
                _ => run.replace(arg),
            };
            if progressed {
                run.check(label);
                break;
            }
        }
        if progressed {
            idle_since = None;
            continue;
        }
        // Nothing could move: moves in flight will land, or the run is stuck.
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() > Duration::from_secs(10) {
            let snapshot = engine.snapshot();
            panic!(
                "{label}: no step could make progress for 10 s; running {}, next {:?}, \
                 queues {:?}, catalog {:?}, batches {:#?}",
                run.running.len(),
                run.next,
                engine
                    .stats()
                    .queues
                    .iter()
                    .map(|q| q.count)
                    .collect::<Vec<_>>(),
                snapshot.stats,
                snapshot.batches
            );
        }
        if reactor.in_flight() > 0 {
            std::thread::sleep(Duration::from_micros(50));
        } else {
            std::thread::yield_now();
        }
    }
    let wall = started.elapsed();
    super::settle(&reactor);
    run.check(label);
    // Every batch reached every consumer exactly once, in the order it was pushed (PL-I5).
    assert_eq!(
        run.pushed, run.taken,
        "{label}: every push is taken once, in order"
    );
    for source in 0..shape.topology.sources() {
        for &q in shape.topology.source_outputs(source) {
            assert_eq!(
                run.pushed.get(&q).map(Vec::len).unwrap_or(0) as u64,
                shape.batches[usize::from(source)],
                "{label}: queue {q} got every batch of source {source}"
            );
        }
    }
    let catalog = engine.stats().catalog;
    assert_eq!(catalog.buffers, 0, "{label}: every batch freed");
    assert_eq!(catalog.freed, run.batches, "{label}: each batch freed once");
    assert_eq!(
        catalog.released, run.takes,
        "{label}: each take released once"
    );
    assert_eq!(
        catalog.resident_by_tier,
        [0; moruna_kernel::TIER_COUNT],
        "{label}: nothing left"
    );
    assert_eq!(
        catalog.reserved_by_tier,
        [0; moruna_kernel::TIER_COUNT],
        "{label}: no claim left"
    );
    assert_eq!(
        catalog.overrun_bytes, 0,
        "{label}: every batch fit its claim"
    );
    Outcome {
        catalog,
        takes: run.takes,
        batches: run.batches,
        wall,
    }
}
