//! The shape of a run (contracts d.16): sources, kernel stages and sinks joined by queues.
//!
//! A run is a DAG. Each queue is one edge of it: from a producer (a source, or a kernel stage's
//! output) to a consumer (one input port of a kernel stage, or a sink). A source or a stage whose
//! output feeds several consumers has one queue per consumer, and a batch it produces is pushed
//! into every one of them and shared, never copied (placement d.1, `push_to`). A stage that takes
//! several inputs has one input port per queue, numbered from 0, and receives batches tagged by
//! the port they came in on (d.7, `Kernel::apply_port`).
//!
//! Sequence numbers: each source numbers what it reads from 0, and a morsel keeps its number
//! through every stage that takes one input. A stage with `k` input ports numbers its output for
//! input `s` on port `p` as `s * k + p`, so the morsels on any one queue have distinct numbers and
//! the numbers on a sink's queue are a function of the sources' numbers ([`Topology::paths`]).
//! A linear chain of `n` kernels is the topology [`Topology::chain`]: queue `k` from stage `k`
//! (the source when `k` is 0) to stage `k + 1` (the sink when `k` is `n`), every number kept.

use crate::error::MorunaError;
use crate::ids::{Seq, StageId};

/// What produces into a queue.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Producer {
    /// Source `i`, counted from 0.
    Source(u16),
    /// The output of kernel stage `s`, counted from 1.
    Kernel(StageId),
}

/// What consumes from a queue.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Consumer {
    /// Input port `port` of kernel stage `stage`.
    Kernel {
        /// The stage, counted from 1.
        stage: StageId,
        /// The port, counted from 0.
        port: u16,
    },
    /// Sink `i`, counted from 0.
    Sink(u16),
}

/// One queue: an edge of the run's DAG.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Link {
    /// Its producer.
    pub from: Producer,
    /// Its consumer.
    pub to: Consumer,
}

/// How a path maps a source's sequence numbers to a queue's: `seq * a + b`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Affine {
    /// The factor: the product of the port counts of the stages with several inputs on the path.
    pub a: u64,
    /// The offset.
    pub b: u64,
}

impl Affine {
    /// The identity: a path through stages that each take one input.
    pub const IDENTITY: Affine = Affine { a: 1, b: 0 };

    /// The number `seq` maps to, `None` when it would not fit in a `Seq`.
    pub fn apply(&self, seq: Seq) -> Option<Seq> {
        seq.checked_mul(self.a)?.checked_add(self.b)
    }

    /// The largest `seq` whose number is at most `bound`, `None` when there is none.
    pub fn floor_inverse(&self, bound: Seq) -> Option<Seq> {
        if bound < self.b {
            return None;
        }
        Some((bound - self.b) / self.a)
    }

    /// This map followed by the one a stage with `ports` inputs applies on `port`.
    fn then(&self, ports: u64, port: u64) -> Option<Affine> {
        if ports <= 1 {
            return Some(*self);
        }
        Some(Affine {
            a: self.a.checked_mul(ports)?,
            b: self.b.checked_mul(ports)?.checked_add(port)?,
        })
    }
}

/// The most paths from the sources to the sinks a topology may have: each is walked on every
/// commit and every skip, and a DAG with more is refused at build (diamonds stacked twelve deep).
pub const MAX_PATHS: usize = 4096;

/// The shape of a run: how many sources, kernel stages and sinks it has and the queues between
/// them, indexed by queue id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    links: Vec<Link>,
    sources: u16,
    stages: u16,
    sinks: u16,
    stage_inputs: Vec<Vec<StageId>>,
    stage_outputs: Vec<Vec<StageId>>,
    source_outputs: Vec<Vec<StageId>>,
    sink_inputs: Vec<StageId>,
    paths: Vec<Vec<(u16, Affine)>>,
}

fn plan(msg: impl Into<String>) -> MorunaError {
    MorunaError::Plan(msg.into())
}

impl Topology {
    /// A topology of `sources`, `stages` and `sinks` with the queues `links`, queue `q` being
    /// `links[q]`. Refused (`Plan`) unless: every link names a source, stage and sink that exist;
    /// every stage takes its input ports `0..k` from one queue each and gives at least one; every
    /// source gives at least one queue and every sink takes exactly one; every link between two
    /// stages goes from a lower stage to a higher one, so the stages are numbered in an order in
    /// which every edge goes forward (and the graph has no cycle); and the paths from the sources
    /// to the sinks number at most [`MAX_PATHS`]. A run has at least one source and one sink.
    pub fn new(sources: u16, stages: u16, sinks: u16, links: Vec<Link>) -> crate::Result<Topology> {
        if sources == 0 || sinks == 0 {
            return Err(plan("a run has at least one source and at least one sink"));
        }
        if links.len() > usize::from(StageId::MAX) {
            return Err(plan(format!(
                "{} queues is more than a run holds",
                links.len()
            )));
        }
        let mut stage_inputs: Vec<Vec<Option<StageId>>> = vec![Vec::new(); usize::from(stages)];
        let mut stage_outputs: Vec<Vec<StageId>> = vec![Vec::new(); usize::from(stages)];
        let mut source_outputs: Vec<Vec<StageId>> = vec![Vec::new(); usize::from(sources)];
        let mut sink_inputs: Vec<Vec<StageId>> = vec![Vec::new(); usize::from(sinks)];
        for (q, link) in links.iter().enumerate() {
            let q = q as StageId;
            match link.from {
                Producer::Source(s) if s < sources => source_outputs[usize::from(s)].push(q),
                Producer::Kernel(k) if k >= 1 && k <= stages => {
                    stage_outputs[usize::from(k - 1)].push(q)
                }
                other => {
                    return Err(plan(format!(
                        "queue {q} comes from {other:?}, which the run does not have"
                    )));
                }
            }
            match link.to {
                Consumer::Kernel { stage, port } if stage >= 1 && stage <= stages => {
                    if let Producer::Kernel(from) = link.from
                        && from >= stage
                    {
                        return Err(plan(format!(
                            "queue {q} goes from stage {from} to stage {stage}: stages are \
                             numbered so every queue goes forward, and a run has no cycle"
                        )));
                    }
                    let ports = &mut stage_inputs[usize::from(stage - 1)];
                    let at = usize::from(port);
                    if ports.len() <= at {
                        ports.resize(at + 1, None);
                    }
                    if ports[at].replace(q).is_some() {
                        return Err(plan(format!(
                            "stage {stage} takes two queues on its port {port}"
                        )));
                    }
                }
                Consumer::Sink(t) if t < sinks => sink_inputs[usize::from(t)].push(q),
                other => {
                    return Err(plan(format!(
                        "queue {q} goes to {other:?}, which the run does not have"
                    )));
                }
            }
        }
        let mut inputs = Vec::with_capacity(usize::from(stages));
        for (index, ports) in stage_inputs.into_iter().enumerate() {
            let stage = index + 1;
            if ports.is_empty() {
                return Err(plan(format!("stage {stage} takes no input")));
            }
            let mut queues = Vec::with_capacity(ports.len());
            for (port, queue) in ports.into_iter().enumerate() {
                match queue {
                    Some(q) => queues.push(q),
                    None => {
                        return Err(plan(format!(
                            "stage {stage} has no queue on its port {port}"
                        )));
                    }
                }
            }
            inputs.push(queues);
        }
        for (index, outputs) in stage_outputs.iter().enumerate() {
            if outputs.is_empty() {
                return Err(plan(format!("nothing takes stage {}'s output", index + 1)));
            }
        }
        for (index, outputs) in source_outputs.iter().enumerate() {
            if outputs.is_empty() {
                return Err(plan(format!("nothing takes source {index}'s output")));
            }
        }
        let mut sink_queue = Vec::with_capacity(usize::from(sinks));
        for (index, queues) in sink_inputs.into_iter().enumerate() {
            match queues.as_slice() {
                [one] => sink_queue.push(*one),
                [] => return Err(plan(format!("sink {index} takes no input"))),
                _ => {
                    return Err(plan(format!(
                        "sink {index} takes {} queues, and a sink takes one",
                        queues.len()
                    )));
                }
            }
        }
        let mut topology = Topology {
            links,
            sources,
            stages,
            sinks,
            stage_inputs: inputs,
            stage_outputs,
            source_outputs,
            sink_inputs: sink_queue,
            paths: Vec::new(),
        };
        topology.paths = topology.walk_paths()?;
        Ok(topology)
    }

    /// The linear chain of one source, `kernels` stages and one sink: queue `k` from stage `k`
    /// (the source for 0) to stage `k + 1` (the sink for `kernels`).
    pub fn chain(kernels: u16) -> Topology {
        let n = usize::from(kernels);
        let links = (0..=kernels)
            .map(|k| Link {
                from: if k == 0 {
                    Producer::Source(0)
                } else {
                    Producer::Kernel(k)
                },
                to: if k == kernels {
                    Consumer::Sink(0)
                } else {
                    Consumer::Kernel {
                        stage: k + 1,
                        port: 0,
                    }
                },
            })
            .collect();
        Topology {
            links,
            sources: 1,
            stages: kernels,
            sinks: 1,
            stage_inputs: (0..n).map(|i| vec![i as StageId]).collect(),
            stage_outputs: (0..n).map(|i| vec![i as StageId + 1]).collect(),
            source_outputs: vec![vec![0]],
            sink_inputs: vec![kernels],
            paths: vec![vec![(0, Affine::IDENTITY)]],
        }
    }

    /// Every path from a source to each sink, as the map from the source's numbers to the sink's.
    fn walk_paths(&self) -> crate::Result<Vec<Vec<(u16, Affine)>>> {
        let mut paths: Vec<Vec<(u16, Affine)>> = vec![Vec::new(); usize::from(self.sinks)];
        let mut stack: Vec<(u16, StageId, Affine)> = Vec::new();
        let mut walked = 0usize;
        for (source, outputs) in self.source_outputs.iter().enumerate() {
            for &q in outputs {
                stack.push((source as u16, q, Affine::IDENTITY));
            }
        }
        while let Some((source, q, map)) = stack.pop() {
            walked += 1;
            if walked > MAX_PATHS * 64 {
                return Err(plan(format!(
                    "the run's DAG has more than {MAX_PATHS} paths from its sources to its sinks"
                )));
            }
            match self.links[usize::from(q)].to {
                Consumer::Sink(t) => {
                    let into = &mut paths[usize::from(t)];
                    into.push((source, map));
                    if paths.iter().map(Vec::len).sum::<usize>() > MAX_PATHS {
                        return Err(plan(format!(
                            "the run's DAG has more than {MAX_PATHS} paths from its sources to \
                             its sinks"
                        )));
                    }
                }
                Consumer::Kernel { stage, port } => {
                    let ports = self.inputs(stage).len() as u64;
                    let next = map.then(ports, u64::from(port)).ok_or_else(|| {
                        plan("the run's DAG numbers its morsels past what a sequence number holds")
                    })?;
                    for &out in self.outputs(stage) {
                        stack.push((source, out, next));
                    }
                }
            }
        }
        for sink in &mut paths {
            sink.sort_by_key(|(source, map)| (*source, map.a, map.b));
        }
        Ok(paths)
    }

    /// The number of queues.
    pub fn queue_count(&self) -> u16 {
        self.links.len() as u16
    }

    /// Every queue, by id.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// Queue `q`.
    pub fn link(&self, q: StageId) -> Option<&Link> {
        self.links.get(usize::from(q))
    }

    /// The number of sources.
    pub fn sources(&self) -> u16 {
        self.sources
    }

    /// The number of kernel stages; they are 1 to this.
    pub fn stages(&self) -> u16 {
        self.stages
    }

    /// The number of sinks.
    pub fn sinks(&self) -> u16 {
        self.sinks
    }

    /// The queues stage `stage` takes, by port; empty for a stage the run does not have.
    pub fn inputs(&self, stage: StageId) -> &[StageId] {
        usize::from(stage)
            .checked_sub(1)
            .and_then(|i| self.stage_inputs.get(i))
            .map_or(&[], Vec::as_slice)
    }

    /// The queues stage `stage` gives to.
    pub fn outputs(&self, stage: StageId) -> &[StageId] {
        usize::from(stage)
            .checked_sub(1)
            .and_then(|i| self.stage_outputs.get(i))
            .map_or(&[], Vec::as_slice)
    }

    /// The queues source `source` gives to.
    pub fn source_outputs(&self, source: u16) -> &[StageId] {
        self.source_outputs
            .get(usize::from(source))
            .map_or(&[], Vec::as_slice)
    }

    /// The queue sink `sink` takes.
    pub fn sink_input(&self, sink: u16) -> Option<StageId> {
        self.sink_inputs.get(usize::from(sink)).copied()
    }

    /// The sink that takes queue `q`, when a sink does.
    pub fn sink_of(&self, q: StageId) -> Option<u16> {
        match self.link(q)?.to {
            Consumer::Sink(t) => Some(t),
            Consumer::Kernel { .. } => None,
        }
    }

    /// The source that gives queue `q`, when a source does: its morsels can be read again
    /// from their origin (CT-I12).
    pub fn source_of(&self, q: StageId) -> Option<u16> {
        match self.link(q)?.from {
            Producer::Source(s) => Some(s),
            Producer::Kernel(_) => None,
        }
    }

    /// True for the linear chain: one source, one sink, and every stage taking and giving one
    /// queue.
    pub fn is_chain(&self) -> bool {
        self.sources == 1
            && self.sinks == 1
            && self.stage_inputs.iter().all(|q| q.len() == 1)
            && self.stage_outputs.iter().all(|q| q.len() == 1)
            && self.source_outputs.iter().all(|q| q.len() == 1)
    }

    /// The number stage `stage` gives its output for input `seq` taken on `port`.
    pub fn out_seq(&self, stage: StageId, port: u16, seq: Seq) -> Seq {
        let ports = self.inputs(stage).len() as u64;
        if ports <= 1 {
            seq
        } else {
            seq.saturating_mul(ports).saturating_add(u64::from(port))
        }
    }

    /// The morsel `seq` on queue `q` came from: the queue and number its producer took it as,
    /// or `None` when a source produced it.
    pub fn parent(&self, q: StageId, seq: Seq) -> Option<(StageId, Seq)> {
        let Producer::Kernel(stage) = self.link(q)?.from else {
            return None;
        };
        let inputs = self.inputs(stage);
        let ports = inputs.len() as u64;
        if ports <= 1 {
            return inputs.first().map(|&q| (q, seq));
        }
        let port = (seq % ports) as usize;
        inputs.get(port).map(|&q| (q, seq / ports))
    }

    /// The source morsel `seq` on queue `q` comes from: the source and the number it read it as.
    pub fn root(&self, q: StageId, seq: Seq) -> Option<(u16, Seq)> {
        let (mut q, mut seq) = (q, seq);
        loop {
            if let Some(source) = self.source_of(q) {
                return Some((source, seq));
            }
            (q, seq) = self.parent(q, seq)?;
        }
    }

    /// Every path from a source to sink `sink`, as `(source, map)`: the sink receives
    /// `map.apply(s)` for each morsel `s` the source reads, once per path.
    pub fn paths(&self, sink: u16) -> &[(u16, Affine)] {
        self.paths.get(usize::from(sink)).map_or(&[], Vec::as_slice)
    }

    /// Where the morsel `seq` on queue `q` ends: `(sink, number)` for every path from the queue
    /// to a sink. What a skip of it tells each sink.
    pub fn to_sinks(&self, q: StageId, seq: Seq) -> Vec<(u16, Seq)> {
        let mut out = Vec::new();
        let mut stack = vec![(q, seq)];
        while let Some((q, seq)) = stack.pop() {
            match self.link(q).map(|l| l.to) {
                Some(Consumer::Sink(t)) => out.push((t, seq)),
                Some(Consumer::Kernel { stage, port }) => {
                    let next = self.out_seq(stage, port, seq);
                    for &o in self.outputs(stage) {
                        stack.push((o, next));
                    }
                }
                None => {}
            }
        }
        out.sort_unstable();
        out
    }

    /// Whether a morsel numbered `seq` reaches queue `q`, given how many each source read
    /// (`read[i]`, `None` while source `i` is still reading): `None` while it depends on a
    /// source still reading. What a sink tells a number no morsel will ever have by.
    pub fn reaches(&self, q: StageId, seq: Seq, read: &[Option<Seq>]) -> Option<bool> {
        let (source, root) = self.root(q, seq)?;
        read.get(usize::from(source))
            .copied()
            .flatten()
            .map(|n| root < n)
    }

    /// The highest number a morsel reaches sink `sink` with, given every source's count, or
    /// `None` when no morsel reaches it.
    pub fn last_at_sink(&self, sink: u16, read: &[Seq]) -> Option<Seq> {
        self.paths(sink)
            .iter()
            .filter_map(|(source, map)| {
                let n = *read.get(usize::from(*source))?;
                map.apply(n.checked_sub(1)?)
            })
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(stage: StageId, port: u16) -> Consumer {
        Consumer::Kernel { stage, port }
    }

    /// CT-T23: the chain is the topology of a version 1 document: queue k from stage k to k + 1, every
    /// number kept, one path.
    #[test]
    fn ct_t23_the_chain_keeps_every_number() {
        let chain = Topology::chain(2);
        assert_eq!(chain.queue_count(), 3);
        assert!(chain.is_chain());
        assert_eq!(chain.inputs(1), &[0]);
        assert_eq!(chain.outputs(2), &[2]);
        assert_eq!(chain.source_outputs(0), &[0]);
        assert_eq!(chain.sink_input(0), Some(2));
        assert_eq!(chain.parent(2, 7), Some((1, 7)));
        assert_eq!(chain.parent(0, 7), None);
        assert_eq!(chain.root(2, 7), Some((0, 7)));
        assert_eq!(chain.paths(0), &[(0, Affine::IDENTITY)]);
        assert_eq!(chain.to_sinks(0, 4), vec![(0, 4)]);
        let built = Topology::new(1, 2, 1, chain.links().to_vec()).expect("the chain is a DAG");
        assert_eq!(built, chain);
        let copy = Topology::chain(0);
        assert_eq!(copy.sink_input(0), Some(0));
        assert_eq!(copy.source_of(0), Some(0));
        assert_eq!(copy.sink_of(0), Some(0));
    }

    /// A diamond: the source feeds stages 1 and 2, both feed stage 3's two ports, stage 3 the
    /// sink. Stage 3 numbers port 0's `s` as `2s` and port 1's as `2s + 1`.
    #[test]
    fn ct_t23_a_diamond_numbers_its_join() {
        let links = vec![
            Link {
                from: Producer::Source(0),
                to: k(1, 0),
            },
            Link {
                from: Producer::Source(0),
                to: k(2, 0),
            },
            Link {
                from: Producer::Kernel(1),
                to: k(3, 0),
            },
            Link {
                from: Producer::Kernel(2),
                to: k(3, 1),
            },
            Link {
                from: Producer::Kernel(3),
                to: Consumer::Sink(0),
            },
        ];
        let t = Topology::new(1, 3, 1, links).expect("a diamond is a DAG");
        assert!(!t.is_chain());
        assert_eq!(t.inputs(3), &[2, 3]);
        assert_eq!(t.out_seq(3, 1, 5), 11);
        assert_eq!(t.parent(4, 11), Some((3, 5)));
        assert_eq!(t.parent(4, 10), Some((2, 5)));
        assert_eq!(t.root(4, 11), Some((0, 5)));
        assert_eq!(
            t.paths(0),
            &[(0, Affine { a: 2, b: 0 }), (0, Affine { a: 2, b: 1 })]
        );
        assert_eq!(t.to_sinks(0, 5), vec![(0, 10)]);
        assert_eq!(t.to_sinks(1, 5), vec![(0, 11)]);
        assert_eq!(t.reaches(4, 11, &[Some(6)]), Some(true));
        assert_eq!(t.reaches(4, 13, &[Some(6)]), Some(false));
        assert_eq!(t.reaches(4, 13, &[None]), None);
        assert_eq!(t.last_at_sink(0, &[6]), Some(11));
        assert_eq!(t.last_at_sink(0, &[0]), None);
    }

    /// Fan-out of one source to two sinks, and fan-in of two sources into one stage.
    #[test]
    fn ct_t23_fan_out_and_fan_in() {
        let out = Topology::new(
            1,
            2,
            2,
            vec![
                Link {
                    from: Producer::Source(0),
                    to: k(1, 0),
                },
                Link {
                    from: Producer::Source(0),
                    to: k(2, 0),
                },
                Link {
                    from: Producer::Kernel(1),
                    to: Consumer::Sink(0),
                },
                Link {
                    from: Producer::Kernel(2),
                    to: Consumer::Sink(1),
                },
            ],
        )
        .expect("fan-out");
        assert_eq!(out.source_outputs(0), &[0, 1]);
        assert_eq!(out.to_sinks(0, 3), vec![(0, 3)]);
        assert_eq!(out.paths(1), &[(0, Affine::IDENTITY)]);
        let fan_in = Topology::new(
            2,
            1,
            1,
            vec![
                Link {
                    from: Producer::Source(0),
                    to: k(1, 0),
                },
                Link {
                    from: Producer::Source(1),
                    to: k(1, 1),
                },
                Link {
                    from: Producer::Kernel(1),
                    to: Consumer::Sink(0),
                },
            ],
        )
        .expect("fan-in");
        assert_eq!(fan_in.root(2, 7), Some((1, 3)));
        assert_eq!(fan_in.reaches(2, 7, &[Some(1), Some(4)]), Some(true));
        assert_eq!(fan_in.reaches(2, 6, &[Some(1), Some(4)]), Some(false));
        assert_eq!(fan_in.last_at_sink(0, &[1, 4]), Some(7));
    }

    /// Every shape a run cannot have is refused, naming what is wrong.
    #[test]
    fn ct_t23_shapes_a_run_cannot_have_are_refused() {
        let refused = |sources, stages, sinks, links: Vec<Link>| match Topology::new(
            sources, stages, sinks, links,
        ) {
            Err(MorunaError::Plan(msg)) => msg,
            other => panic!("refused as a plan, got {other:?}"),
        };
        let to_sink = Link {
            from: Producer::Source(0),
            to: Consumer::Sink(0),
        };
        assert!(refused(0, 0, 1, vec![]).contains("at least one source"));
        assert!(refused(1, 0, 0, vec![]).contains("at least one sink"));
        assert!(
            refused(
                1,
                0,
                1,
                vec![Link {
                    from: Producer::Source(1),
                    to: Consumer::Sink(0)
                }]
            )
            .contains("comes from")
        );
        assert!(
            refused(
                1,
                0,
                1,
                vec![Link {
                    from: Producer::Source(0),
                    to: Consumer::Sink(2)
                }]
            )
            .contains("goes to")
        );
        assert!(refused(1, 1, 1, vec![to_sink]).contains("stage 1 takes no input"));
        assert!(refused(1, 0, 1, vec![]).contains("nothing takes source 0"));
        assert!(refused(1, 0, 1, vec![to_sink, to_sink]).contains("sink 0 takes 2 queues"));
        assert!(refused(1, 0, 2, vec![to_sink]).contains("sink 1 takes no input"));
        assert!(
            refused(
                1,
                1,
                1,
                vec![
                    Link {
                        from: Producer::Source(0),
                        to: k(1, 1)
                    },
                    Link {
                        from: Producer::Kernel(1),
                        to: Consumer::Sink(0)
                    }
                ]
            )
            .contains("no queue on its port 0")
        );
        assert!(
            refused(
                1,
                1,
                1,
                vec![
                    Link {
                        from: Producer::Source(0),
                        to: k(1, 0)
                    },
                    Link {
                        from: Producer::Source(0),
                        to: k(1, 0)
                    },
                    Link {
                        from: Producer::Kernel(1),
                        to: Consumer::Sink(0)
                    }
                ]
            )
            .contains("two queues on its port 0")
        );
        assert!(
            refused(
                1,
                1,
                1,
                vec![
                    Link {
                        from: Producer::Source(0),
                        to: k(1, 0)
                    },
                    to_sink
                ]
            )
            .contains("nothing takes stage 1")
        );
        assert!(
            refused(
                1,
                2,
                1,
                vec![
                    Link {
                        from: Producer::Source(0),
                        to: k(2, 0)
                    },
                    Link {
                        from: Producer::Kernel(2),
                        to: k(1, 0)
                    },
                    Link {
                        from: Producer::Kernel(1),
                        to: Consumer::Sink(0)
                    },
                ]
            )
            .contains("goes forward")
        );
    }

    /// The maps compose, invert and refuse what overflows.
    #[test]
    fn ct_t23_affine_maps() {
        let map = Affine { a: 4, b: 3 };
        assert_eq!(map.apply(5), Some(23));
        assert_eq!(map.floor_inverse(23), Some(5));
        assert_eq!(map.floor_inverse(25), Some(5));
        assert_eq!(map.floor_inverse(2), None);
        assert_eq!(Affine { a: 2, b: 0 }.apply(u64::MAX), None);
        assert_eq!(Affine { a: u64::MAX, b: 0 }.then(2, 1), None);
        assert_eq!(map.then(1, 0), Some(map));
    }
}
