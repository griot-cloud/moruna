//! 6.3j: the controller's amplification of a ring stage is the function's own.
//!
//! The controller measures a kernel once, with a probe: one morsel through one `apply`, and
//! what that unit held in the arena, its input and the output it gave, over its input
//! (`moruna-controller`, `probe.rs`: `a_k = (bytes_in + bytes_out) / bytes_in`). It sizes the
//! stage's morsels from that figure for the rest of the run.
//!
//! A ring stage on the Moruna kernel (`moruna_micro::task`, `TaskState::pipelined`) does not
//! answer a morsel in the unit that brought it. The morsel goes to the task in pieces of at
//! most its plane's piece, [`layout::IN_FLIGHT`] at once, and the unit returns with the answers
//! that have come: it waits only for the task's first answer, whose schema stands for the rest,
//! and to post the pieces it still has queued. The rest come with later units, or at the end.
//! So the probe, which is the stage's first unit, gave the answer to the first of its morsel's
//! pieces, and the controller read a part of the function's output as the whole.
//!
//! 6.2 run 3 (a 16 MiB probe morsel, pieces of 16 MiB, so two of them): Enrich's output is 1.17
//! times its input, and the controller's amplification should have read 2.17. With one count
//! of its bytes (6.3h, 6.3k) it would have read 1.59, one plus half of 1.17, which is what this
//! test's run read before the rule below: 1.58. (In run 3 itself it read 15.06: half of the
//! answer, counted once for each of its 24 buffers.)
//!
//! The rule (`layout::answers_whole`): the stage's first unit answers its whole morsel. Every
//! later unit returns with what has come, as before.
//!
//! The stage here is a model of the kernel's, which compiles for the kernel alone: it follows
//! `pipelined` step by step with the rules `layout` gives both ([`layout::cut`],
//! [`layout::to_post`], [`layout::answers_whole`], [`layout::waits_for_answer`],
//! [`layout::ring_sizes`], [`layout::Morsels`]), over a task that answers a piece when it is
//! waited for and not before, as a function slower than the engine does. Everything else is
//! real: the Parquet source, the arena, the scheduler and its probe, the controller and its
//! report, the trace and the cost model's store.
//!
//! A later unit of such a stage gives what has come, which is other morsels' answers. 6.3m: an
//! answer belongs to its morsel. The stage gives a morsel whole, under its own number, once
//! its last answer has come, and every reader takes the morsels given, not the units. The test
//! reads what each reader got on this run against the function's own figure (see `readers`).

#![allow(clippy::result_large_err)]

mod support;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_cost::{ObservationSource, Target};
use moruna_kernel::{
    Allocator, CancelToken, Completed, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind,
    KernelState, MorunaError, Payload, PayloadKind, PayloadSpec, SourceSchema, Tier, TierPref,
    TraceRecord, TraceSink, TraceTail,
};
use moruna_micro::layout;
use moruna_runtime::cost::{CostSettings, RunLearning, StageCost};
use moruna_runtime::{Components, RunSpec, Runtime, SinkSpec, SourceSpec};
use moruna_testkit::FakeTrace;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use support::{Scratch, one_run_at_a_time, read_back_rows};

/// The input's columns: 48 bytes a row.
const COLUMNS: usize = 6;

/// A task's share of its stage's budget at which its plane's pieces are 12 MiB
/// ([`layout::native_split`], [`layout::ring_sizes`]): the probe's 16 MiB morsel is two pieces,
/// as it was on run 3's node, with room either side for what the source takes 16 MiB to be.
const SHARE: u64 = 624 << 20;

fn kernel_error(msg: String) -> MorunaError {
    MorunaError::Kernel {
        stage: 1,
        seq: 0,
        msg,
        features: None,
    }
}

fn input_schema() -> SchemaRef {
    Arc::new(Schema::new(
        (0..COLUMNS)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, false))
            .collect::<Vec<_>>(),
    ))
}

/// `rows` rows of [`input_schema`], plain and uncompressed, so the source's estimate of a
/// row's bytes is what the row takes in memory, in row groups of a million rows (48 MiB), as
/// the benchmark's files have: a probe reads its 16 MiB from one.
fn write_input(path: &Path, rows: usize) {
    let schema = input_schema();
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(1 << 20))
        .set_dictionary_enabled(false)
        .set_compression(Compression::UNCOMPRESSED)
        .build();
    let file = std::fs::File::create(path).expect("the input file");
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props)).expect("a writer");
    let columns: Vec<ArrayRef> = (0..COLUMNS as i64)
        .map(|c| {
            let values: Vec<i64> = (0..rows as i64).map(|r| r * 7 + c).collect();
            Arc::new(Int64Array::from(values)) as ArrayRef
        })
        .collect();
    let batch = RecordBatch::try_new(schema, columns).expect("a batch");
    writer.write(&batch).expect("the batch is written");
    writer.close().expect("the footer is written");
}

/// What one unit of the stage took and gave.
#[derive(Clone, Copy, Debug)]
struct Unit {
    /// The pieces its morsel was cut into.
    pieces: usize,
    rows_in: usize,
    /// The rows of the morsels it gave, whole, each under its own number.
    rows_out: usize,
    /// Of those, its own morsel's.
    own_rows: usize,
}

/// A ring stage as the kernel's image task makes one, of one task, whose function gives its
/// input's columns and one more: seven columns for six, 1.17 times its input.
struct RingStage {
    fingerprint: Fingerprint,
    piece: u64,
    units: Arc<Mutex<Vec<Unit>>>,
}

/// The stage's task state: `TaskState`'s `queued`, `in_flight`, `morsels`, `done`, `schema` and
/// `whole`.
struct Ring {
    alloc: Arc<dyn Allocator>,
    /// Pieces given to the stage and not yet posted to the task.
    queued: VecDeque<RecordBatch>,
    /// Pieces posted and not answered, oldest first: the task's region holds a copy of each.
    in_flight: VecDeque<RecordBatch>,
    /// The morsels given and not answered in full (6.3m), and those answered whole, not given.
    morsels: layout::Morsels<RecordBatch>,
    done: VecDeque<layout::Answered<RecordBatch>>,
    /// The task's answers' schema, once its first has come.
    schema: Option<SchemaRef>,
    /// The unit under way answers its whole morsel.
    whole: bool,
}

impl KernelState for Ring {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

fn int64(batch: &RecordBatch, column: usize) -> moruna_kernel::Result<&Int64Array> {
    batch
        .column(column)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| kernel_error(format!("column {column} is not int64")))
}

fn output_fields(input: &Schema) -> Vec<Arc<Field>> {
    let mut fields: Vec<Arc<Field>> = input.fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new("total", DataType::Int64, false)));
    fields
}

impl Ring {
    /// `TaskState::record_len`: the bytes of `batch`'s record as the engine lays it out.
    fn record_len(&self, batch: &RecordBatch) -> moruna_kernel::Result<u64> {
        let (framing, placed) = moruna_kernel::ipc::encode_framing(
            batch,
            layout::PAGE as usize,
            0,
            self.alloc.as_ref(),
        )?;
        let spans: Vec<(u64, u64)> = placed
            .iter()
            .map(|(at, b)| (*at as u64, b.len() as u64))
            .collect();
        layout::record_len(framing.len() as u64, &spans)
            .ok_or_else(|| kernel_error("a record past u64".to_string()))
    }

    /// `TaskState::pieces`: `batch` itself when its record fits a piece, else slices of its
    /// rows that each do.
    fn pieces(&self, batch: &RecordBatch, piece: u64) -> moruna_kernel::Result<Vec<RecordBatch>> {
        let len = self.record_len(batch)?;
        if len <= piece || batch.num_rows() <= 1 {
            return Ok(vec![batch.clone()]);
        }
        let mut out = Vec::new();
        for (at, rows) in layout::cut(batch.num_rows(), len, piece) {
            let part = batch.slice(at, rows);
            if rows <= 1 || self.record_len(&part)? <= piece {
                out.push(part);
            } else {
                out.extend(self.pieces(&part, piece)?);
            }
        }
        Ok(out)
    }

    /// `TaskState::feed`: posts the task as many of its queued pieces as it has room for. A
    /// piece posted is copied into the task's region (`TaskState::offer`), so the stage holds
    /// nothing of its morsel past the unit.
    fn feed(&mut self) -> moruna_kernel::Result<()> {
        for _ in 0..layout::to_post(self.in_flight.len(), self.queued.len()) {
            let Some(part) = self.queued.pop_front() else {
                break;
            };
            let columns = (0..part.num_columns())
                .map(|c| {
                    let copy = Int64Array::from(int64(&part, c)?.values().to_vec());
                    Ok(Arc::new(copy) as ArrayRef)
                })
                .collect::<moruna_kernel::Result<Vec<ArrayRef>>>()?;
            let posted = RecordBatch::try_new(part.schema(), columns)
                .map_err(|e| kernel_error(format!("posting a piece: {e}")))?;
            self.in_flight.push_back(posted);
        }
        Ok(())
    }

    /// `TaskState::take_oldest` and `TaskState::took`: the answer to the oldest piece in flight,
    /// waited for, to its morsel. The function runs here: its piece's columns, and their first
    /// two added. Its task stamps the piece's function time on its answer, as a ring task will
    /// (item 6.3z); the kernel's own reports none yet, so its morsels give the cost model nothing.
    fn take_oldest(&mut self) -> moruna_kernel::Result<()> {
        let piece = self
            .in_flight
            .pop_front()
            .ok_or_else(|| kernel_error("no piece in flight".to_string()))?;
        let started = std::time::Instant::now();
        let total: Vec<i64> = int64(&piece, 0)?
            .values()
            .iter()
            .zip(int64(&piece, 1)?.values())
            .map(|(a, b)| a.wrapping_add(*b))
            .collect();
        let mut columns = piece.columns().to_vec();
        columns.push(Arc::new(Int64Array::from(total)));
        let schema = Arc::new(Schema::new(output_fields(&piece.schema())));
        let answer = RecordBatch::try_new(schema, columns)
            .map_err(|e| kernel_error(format!("the function's answer: {e}")))?;
        let ns = started.elapsed().as_nanos() as u64;
        self.schema.get_or_insert_with(|| answer.schema());
        if let Some(whole) = self
            .morsels
            .answer(answer, Some((ns, ns)))
            .map_err(|why| kernel_error(why.to_string()))?
        {
            self.done.push_back(whole);
        }
        Ok(())
    }

    /// `TaskState::give`: each morsel answered whole, its answers joined, under its number.
    fn give(&mut self) -> moruna_kernel::Result<Vec<Completed>> {
        self.done
            .drain(..)
            .map(|whole| {
                let joined = concat_batches(&whole.answers[0].schema(), &whole.answers)
                    .map_err(|e| kernel_error(format!("joining a morsel's answers: {e}")))?;
                Ok(Completed {
                    seq: whole.seq,
                    output: Payload::Table(joined, Tier::Host),
                    time: whole.time,
                })
            })
            .collect()
    }
}

impl Kernel for RingStage {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateful {
            max_instances: core::num::NonZeroUsize::MIN,
        }
    }

    /// The function holds little of its own beside its batches, and says so, so the run keeps
    /// most of its allowance for the arena: the run is then never short of arena, which is not
    /// what this test is about. It seeds the controller's figure only until the probe.
    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            ..Default::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        match input {
            SourceSchema::Table(schema) => Ok(SourceSchema::Table(Arc::new(Schema::new(
                output_fields(schema),
            )))),
            SourceSchema::Tensor { .. } => Err(MorunaError::Plan("the stage takes tables".into())),
        }
    }

    fn init(&self, ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(Ring {
            alloc: ctx.alloc.clone(),
            queued: VecDeque::new(),
            in_flight: VecDeque::new(),
            morsels: layout::Morsels::new(),
            done: VecDeque::new(),
            schema: None,
            whole: false,
        }))
    }

    /// The scheduler gives a stage of one input its morsels (`apply_morsels`).
    fn apply(&self, _: &mut dyn KernelState, _: Payload) -> moruna_kernel::Result<Payload> {
        Err(kernel_error("a ring stage gives morsels".to_string()))
    }

    /// `TaskState::pipelined`, with a task that has answered nothing it was not waited for:
    /// what `gather` finds is nothing, so no answer `came` between two looks.
    fn apply_morsels(
        &self,
        state: &mut dyn KernelState,
        input: Payload,
        seq: moruna_kernel::Seq,
    ) -> moruna_kernel::Result<Vec<Completed>> {
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this stage did not make".to_string()))?;
        let Payload::Table(batch, _) = &input else {
            return Err(kernel_error("the stage takes tables".to_string()));
        };
        let pieces = st.pieces(batch, self.piece)?;
        let cut = pieces.len();
        st.morsels.open(seq, cut);
        st.queued.extend(pieces);
        st.whole = layout::answers_whole(st.schema.is_some());
        st.feed()?;
        if st.schema.is_none() {
            st.take_oldest()?;
            st.feed()?;
        }
        loop {
            st.feed()?;
            // Nothing stays queued past this unit ([`layout::QUEUED_MAX`]), and the stage's
            // first unit leaves nothing in flight.
            if layout::waits_for_answer(st.queued.len(), st.in_flight.len(), st.whole) {
                st.take_oldest()?;
                st.feed()?;
            } else {
                break;
            }
        }
        st.whole = false;
        let given = st.give()?;
        let rows = |c: &Completed| c.output.rows() as usize;
        self.units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Unit {
                pieces: cut,
                rows_in: batch.num_rows(),
                rows_out: given.iter().map(rows).sum(),
                own_rows: given.iter().filter(|c| c.seq == seq).map(rows).sum(),
            });
        Ok(given)
    }

    /// `ImageTask::finish_morsels`: every morsel still open, answered whole.
    fn finish_morsels(
        &self,
        state: &mut dyn KernelState,
        _after: moruna_kernel::Seq,
    ) -> moruna_kernel::Result<Vec<Completed>> {
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this stage did not make".to_string()))?;
        loop {
            st.feed()?;
            if st.in_flight.is_empty() {
                break;
            }
            st.take_oldest()?;
        }
        st.give()
    }
}

/// The amplification the controller's note of stage 1 gives.
fn amplification(notes: &[String]) -> f64 {
    let note = notes
        .iter()
        .find(|n| n.starts_with("controller stage 1: amplification "))
        .unwrap_or_else(|| panic!("the controller says what it measured: {notes:?}"));
    note["controller stage 1: amplification ".len()..]
        .split(',')
        .next()
        .and_then(|figure| figure.trim().parse().ok())
        .unwrap_or_else(|| panic!("a figure in `{note}`"))
}

/// What each reader of the stage's morsels got on this run, against the function's own figure
/// `real` (its bytes out over its bytes in, over the run), each as the engine handed it:
/// - the trace's record of each morsel (its rows and bytes out beside its rows and bytes in);
/// - the stage's learned growth (`moruna-scheduler`, `StageEntry::learn_growth`, by which reads
///   are cut): the most bytes out over bytes in of any morsel given, which the scheduler learns
///   from the same pair it records, so it is the most of the records';
/// - the cost model's observation of each morsel (`learning::settle`), as its store kept it.
///
/// Each must be the function's (item 6.3m). The trace's records are printed with the units
/// that gave them.
fn readers(units: &[Unit], records: &[TraceRecord], observed: &[(f64, f64)], real: f64) {
    for (n, u) in units.iter().enumerate() {
        eprintln!(
            "  unit {n}: {} pieces, {} rows in, gave {} rows of whole morsels ({} of them its own)",
            u.pieces, u.rows_in, u.rows_out, u.own_rows
        );
    }
    let ratio = |r: &TraceRecord| r.bytes_out as f64 / r.bytes_in as f64;
    let mut ratios: Vec<f64> = records.iter().map(ratio).collect();
    ratios.sort_by(f64::total_cmp);
    let growth = ratios[ratios.len() - 1];
    eprintln!(
        "the trace's records of {} morsels: bytes out over bytes in from {:.3} to {:.3}, where \
         the function's is {real:.3}; learned growth {growth:.3}",
        records.len(),
        ratios[0],
        growth
    );
    let wrong: Vec<String> = records
        .iter()
        .filter(|r| (ratio(r) - real).abs() > 0.01 || r.rows_out != r.rows_in)
        .map(|r| {
            format!(
                "morsel {}: {} rows in, {} out, {:.3}",
                r.seq,
                r.rows_in,
                r.rows_out,
                ratio(r)
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "every morsel's record is the function's: {wrong:?}"
    );
    assert!(
        (growth - real).abs() < 0.01,
        "the stage's learned growth is {growth:.3}, where the function's is {real:.3}"
    );
    // The line through the origin that fits the cost model's (bytes in, bytes out) best, and
    // how far each lies from the function's own.
    let (xy, xx) = observed
        .iter()
        .fold((0.0, 0.0), |(xy, xx), (x, y)| (xy + x * y, xx + x * x));
    let fitted = xy / xx;
    let off = observed
        .iter()
        .map(|(x, y)| (y / x - real).abs())
        .fold(0.0, f64::max);
    eprintln!(
        "the cost model's {} morsels: fitted, bytes out are {fitted:.3} times bytes in, each \
         within {off:.3} of the function's {real:.3}",
        observed.len()
    );
    assert!(
        (fitted - real).abs() < 0.01 && off < 0.01,
        "the cost model learns the function's out/in: fitted {fitted:.3}, the farthest off by \
         {off:.3}, where it is {real:.3}"
    );
}
#[test]
fn a_ring_stage_s_amplification_is_its_function_s_output_over_its_input() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("ring_stage_probe");
    let input = scratch.path().join("in.parquet");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).expect("the output directory");
    // 64 MiB: four morsels of the probe's size.
    let rows: usize = 1_400_000;
    write_input(&input, rows);

    let units = Arc::new(Mutex::new(Vec::new()));
    let piece = layout::ring_sizes(layout::native_split(SHARE).0).piece;
    assert_eq!(piece, 12 << 20, "the plane's piece at this share");
    let stage: Arc<dyn Kernel> = Arc::new(RingStage {
        fingerprint: Fingerprint::compute("moruna-runtime::tests::RingStage", b"v1"),
        piece,
        units: units.clone(),
    });
    let source_path = input.clone();
    let sink_url = format!("file://{}", out_dir.display());
    let mut spec = RunSpec::new(
        SourceSpec::Build(Box::new(move |ctx| {
            moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![format!("file://{}", source_path.display())],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )
            .map(|source| Arc::new(source) as Arc<dyn moruna_kernel::Source>)
        })),
        vec![stage],
        SinkSpec::Build(Box::new(move |ctx| {
            moruna_sinks::ParquetSink::new(
                moruna_sinks::ParquetSinkConfig {
                    url: sink_url.clone(),
                    // Room for a morsel's whole answer: the probe's is 19 MiB of batch.
                    row_group_bytes: 8 << 20,
                    file_bytes: 32 << 20,
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.alloc.clone(),
            )
            .map(|sink| Box::new(sink.with_run_id(ctx.run_id)) as Box<dyn moruna_kernel::Sink>)
        })),
    );
    spec.budget = Some(1 << 30);
    spec.cpu = Some(2.0);
    spec.staging_dir = Some(scratch.path().join("staging"));
    spec.staging_limit = Some(1 << 30);
    spec.profiles_dir = Some(scratch.path().join("profiles"));
    std::fs::create_dir_all(scratch.path().join("staging")).expect("the staging directory");

    // The run's trace, and a store for what its cost model learns.
    let trace = Arc::new(FakeTrace::new().capacity(1 << 16));
    let observations = scratch.path().join("observations");
    let learning = RunLearning {
        online: CostSettings::directory(&observations)
            .open_online()
            .expect("the store opens for the run"),
        stages: vec![StageCost {
            kind: "ring".into(),
            settings: moruna_cost::Digest([0; 32]),
            fingerprint: None,
        }],
    };
    let components = Components {
        trace: Some((
            trace.clone() as Arc<dyn TraceSink>,
            trace.clone() as Arc<dyn TraceTail>,
        )),
        learning: Some(learning),
        ..Components::default()
    };
    let report = match Runtime::run_with(spec, CancelToken::new(), components) {
        Ok(report) => report,
        Err(error) => panic!(
            "the run did not complete: {error}; notes: {:?}",
            error.shutdown_notes
        ),
    };

    // The stage gave every row, in its units and at its end.
    assert_eq!(
        read_back_rows(&out_dir),
        rows as u64,
        "every row is written"
    );
    // The stage's totals over the run, from its records (the run's own trace is the test's).
    let all: Vec<TraceRecord> = trace
        .records()
        .into_iter()
        .filter(|r| r.stage == 1)
        .collect();
    let (bytes_in, bytes_out) = all
        .iter()
        .fold((0, 0), |(i, o), r| (i + r.bytes_in, o + r.bytes_out));
    let real = bytes_out as f64 / bytes_in as f64;
    assert!(
        (real - 1.17).abs() < 0.01,
        "the function's output is 1.17 times its input over the run: {bytes_out} bytes out for \
         {bytes_in} in, {real:.2}"
    );

    // The probe is the stage's first unit: two pieces, as on run 3's node.
    let units = units.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let probe = units.first().expect("the stage ran");
    assert_eq!(
        probe.pieces, 2,
        "the probe's morsel of {} rows is two pieces of at most {piece} bytes",
        probe.rows_in
    );
    // The stage's morsels, each once, as their records say: every one taken has one.
    let mut records: Vec<TraceRecord> = all.into_iter().filter(|r| r.rows_in > 0).collect();
    records.sort_by_key(|r| r.seq);
    let mut seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
    seqs.dedup();
    assert_eq!(seqs.len(), records.len(), "a record a morsel");
    assert_eq!(
        (
            records.iter().map(|r| r.rows_in).sum::<u64>(),
            records.iter().map(|r| r.rows_out).sum::<u64>()
        ),
        (rows as u64, rows as u64),
        "every row in a morsel's record, in and out"
    );
    let store = moruna_cost::LocalStore::open(&observations).expect("the store opens");
    let mut observed = Vec::new();
    store
        .scan_all(&mut |history| {
            observed.extend(history.observations.iter().filter_map(|o| {
                Some((
                    o.inputs.data.bytes? as f64,
                    o.outcome.get(Target::BytesOut)?,
                ))
            }));
            Ok(())
        })
        .expect("the store scans");
    assert!(
        observed.len() + 1 >= records.len(),
        "a cost-model observation for every morsel but the probe's: {} of {}",
        observed.len(),
        records.len()
    );
    readers(&units, &records, &observed, real);

    let measured = amplification(&report.notes);
    assert!(
        (measured - (1.0 + real)).abs() < 0.05,
        "the controller's amplification of the stage is {measured:.2}, where a morsel holds its \
         input and {real:.2} times it of output in the arena, {:.2}; the probe's unit gave {} \
         of its morsel's {} rows",
        1.0 + real,
        probe.rows_out,
        probe.rows_in
    );
    assert_eq!(
        (probe.rows_out, probe.own_rows),
        (probe.rows_in, probe.rows_in),
        "the stage's first unit answers its whole morsel, and gives nothing else"
    );
}
