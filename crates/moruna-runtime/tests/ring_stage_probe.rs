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
//! [`layout::ring_sizes`]), over a task that answers a piece when it is waited for and not
//! before, as a function slower than the engine does. Everything else is real: the Parquet
//! source, the arena, the scheduler and its probe, the controller and its report.
//!
//! A unit of such a stage is still not self-contained after its first: what a later unit gives
//! is other morsels' answers. The test prints what each reader of single units gets on this
//! run, against the function's own figure (see `per_unit_readers`).

#![allow(clippy::result_large_err)]

mod support;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use moruna_kernel::{
    Allocator, CancelToken, Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState,
    MorunaError, Payload, PayloadKind, PayloadSpec, SourceSchema, Tier, TierPref,
};
use moruna_micro::layout;
use moruna_runtime::{RunSpec, Runtime, SinkSpec, SourceSpec};
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

/// What one unit of the stage took and gave, as the engine's worker counts it.
#[derive(Clone, Copy, Debug)]
struct Unit {
    /// The pieces its morsel was cut into.
    pieces: usize,
    rows_in: usize,
    rows_out: usize,
    /// Of the rows it gave, those that answer its own morsel.
    own_rows: usize,
    bytes_in: u64,
    bytes_out: u64,
}

/// A ring stage as the kernel's image task makes one, of one task, whose function gives its
/// input's columns and one more: seven columns for six, 1.17 times its input.
struct RingStage {
    fingerprint: Fingerprint,
    piece: u64,
    units: Arc<Mutex<Vec<Unit>>>,
}

/// A piece, or its answer, and the unit whose morsel it is of.
type Of = (usize, RecordBatch);

/// The stage's task state: `TaskState`'s `queued`, `in_flight`, `ready`, `schema` and `whole`.
struct Ring {
    alloc: Arc<dyn Allocator>,
    /// Units applied so far.
    units: usize,
    /// Pieces given to the stage and not yet posted to the task.
    queued: VecDeque<Of>,
    /// Pieces posted and not answered, oldest first: the task's region holds a copy of each.
    in_flight: VecDeque<Of>,
    /// Answers taken and not yet given.
    ready: VecDeque<Of>,
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
            let Some((unit, part)) = self.queued.pop_front() else {
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
            self.in_flight.push_back((unit, posted));
        }
        Ok(())
    }

    /// `TaskState::take_oldest`: the answer to the oldest piece in flight, waited for, onto
    /// `ready`. The function runs here: its piece's columns, and their first two added.
    fn take_oldest(&mut self) -> moruna_kernel::Result<()> {
        let (unit, piece) = self
            .in_flight
            .pop_front()
            .ok_or_else(|| kernel_error("no piece in flight".to_string()))?;
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
        self.schema.get_or_insert_with(|| answer.schema());
        self.ready.push_back((unit, answer));
        Ok(())
    }

    /// The answers taken and not given, as one batch (`TaskState::join_ready`), and how many
    /// of its rows answer unit `unit`'s morsel.
    fn give(&mut self, unit: usize) -> moruna_kernel::Result<Option<(RecordBatch, usize)>> {
        let ready: Vec<Of> = std::mem::take(&mut self.ready).into();
        let own = ready
            .iter()
            .filter(|(of, _)| *of == unit)
            .map(|(_, answer)| answer.num_rows())
            .sum();
        let mut answers: Vec<RecordBatch> = ready.into_iter().map(|(_, answer)| answer).collect();
        match answers.len() {
            0 => Ok(None),
            1 => Ok(answers.pop().map(|answer| (answer, own))),
            _ => concat_batches(&answers[0].schema(), &answers)
                .map(|joined| Some((joined, own)))
                .map_err(|e| kernel_error(format!("joining answers: {e}"))),
        }
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
            units: 0,
            queued: VecDeque::new(),
            in_flight: VecDeque::new(),
            ready: VecDeque::new(),
            schema: None,
            whole: false,
        }))
    }

    /// `TaskState::pipelined`, with a task that has answered nothing it was not waited for:
    /// what `gather` finds is nothing, so no answer `came` between two looks.
    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> moruna_kernel::Result<Payload> {
        let st = state
            .as_any_mut()
            .downcast_mut::<Ring>()
            .ok_or_else(|| kernel_error("a state this stage did not make".to_string()))?;
        let Payload::Table(batch, _) = &input else {
            return Err(kernel_error("the stage takes tables".to_string()));
        };
        let unit = st.units;
        st.units += 1;
        let pieces = st.pieces(batch, self.piece)?;
        let cut = pieces.len();
        st.queued
            .extend(pieces.into_iter().map(|piece| (unit, piece)));
        st.whole = layout::answers_whole(st.schema.is_some());
        st.feed()?;
        if st.schema.is_none() && st.ready.is_empty() {
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
        let (answer, own_rows) = match st.give(unit)? {
            Some(given) => given,
            // No answer has come: a batch of no rows, and the answers come with a later one.
            None => (
                RecordBatch::new_empty(
                    st.schema
                        .clone()
                        .ok_or_else(|| kernel_error("no answer's schema yet".to_string()))?,
                ),
                0,
            ),
        };
        let (rows_in, rows_out, bytes_in) = (batch.num_rows(), answer.num_rows(), input.bytes());
        let answer = Payload::Table(answer, Tier::Host);
        self.units
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Unit {
                pieces: cut,
                rows_in,
                rows_out,
                own_rows,
                bytes_in,
                bytes_out: answer.bytes(),
            });
        Ok(answer)
    }

    /// `ImageTask::finish`: every answer still in flight, joined.
    fn finish(&self, state: &mut dyn KernelState) -> moruna_kernel::Result<Option<Payload>> {
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
        Ok(st
            .give(usize::MAX)?
            .map(|(rest, _)| Payload::Table(rest, Tier::Host)))
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

/// What each reader of single units gets on this run, printed against the function's own
/// figure `real` (its bytes out over its bytes in, over the run). After its first, a unit of a
/// ring stage gives other morsels' answers, so these are not the function's: they are what
/// the engine's worker hands, unit by unit, to
/// - the stage's learned growth (`moruna-scheduler`, `StageEntry::learn_growth`: the largest
///   bytes out over bytes in of any one unit, by which reads are cut),
/// - the trace's record of each morsel (its rows and bytes out beside its rows and bytes in),
/// - the cost model's observation of each unit (`learning::settle`: bytes out and rows out for
///   a unit's bytes in and rows in).
fn per_unit_readers(units: &[Unit], real: f64) {
    let ratio = |u: &Unit| u.bytes_out as f64 / u.bytes_in as f64;
    for (n, u) in units.iter().enumerate() {
        eprintln!(
            "  unit {n}: {} pieces, {} rows in, {} rows out ({} of them its own), {:.2} bytes \
             out a byte in",
            u.pieces,
            u.rows_in,
            u.rows_out,
            u.own_rows,
            ratio(u)
        );
    }
    let growth = units.iter().map(ratio).fold(0.0, f64::max);
    eprintln!(
        "learned growth (the most any one unit gave over what it took): {growth:.2}, where the \
         function's is {real:.2}: {:.1} times it",
        growth / real
    );
    let own = units
        .iter()
        .filter(|u| u.own_rows == u.rows_out && u.rows_out > 0)
        .count();
    let none = units.iter().filter(|u| u.rows_out == 0).count();
    let mut ratios: Vec<f64> = units.iter().map(ratio).collect();
    ratios.sort_by(f64::total_cmp);
    eprintln!(
        "the trace's record of a morsel: {own} of {} units gave their own morsel's answer and \
         nothing else, {none} gave no rows, {} gave another morsel's; a record's bytes out over \
         its bytes in runs from {:.2} to {:.2}, median {:.2}, where the function's is {real:.2}",
        units.len(),
        units.len() - own - none,
        ratios[0],
        ratios[ratios.len() - 1],
        ratios[ratios.len() / 2]
    );
    // The line through the origin that fits the units' (bytes in, bytes out) best, which is
    // what a model of one unit learns of the function, and how far each unit lies from the
    // function's own line.
    let (xy, xx) = units.iter().fold((0.0, 0.0), |(xy, xx), u| {
        let (x, y) = (u.bytes_in as f64, u.bytes_out as f64);
        (xy + x * y, xx + x * x)
    });
    let mut off: Vec<f64> = units
        .iter()
        .map(|u| (u.bytes_out as f64 - real * u.bytes_in as f64).abs() / (real * u.bytes_in as f64))
        .collect();
    off.sort_by(f64::total_cmp);
    eprintln!(
        "the cost model's units: fitted through them, bytes out are {:.2} times bytes in, where \
         the function's are {real:.2}; a unit's bytes out are off its own morsel's by {:.0}% at \
         the median and {:.0}% at the most; their sum is {:.2} of the stage's bytes out, the \
         rest given at the stage's end",
        xy / xx,
        100.0 * off[off.len() / 2],
        100.0 * off[off.len() - 1],
        units.iter().map(|u| u.bytes_out).sum::<u64>() as f64
            / (real * units.iter().map(|u| u.bytes_in).sum::<u64>() as f64)
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

    let report = match Runtime::run(spec, CancelToken::new()) {
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
    let totals = report
        .stages
        .iter()
        .find(|s| s.stage == 1)
        .expect("the stage's totals");
    let real = totals.bytes_out as f64 / totals.bytes_in as f64;
    assert!(
        (real - 1.17).abs() < 0.01,
        "the function's output is 1.17 times its input over the run: {} bytes out for {} in, \
         {real:.2}",
        totals.bytes_out,
        totals.bytes_in
    );

    // The probe is the stage's first unit: two pieces, as on run 3's node.
    let units = units.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let probe = units.first().expect("the stage ran");
    assert_eq!(
        probe.pieces, 2,
        "the probe's morsel of {} rows is two pieces of at most {piece} bytes",
        probe.rows_in
    );
    per_unit_readers(&units, real);

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
        "the stage's first unit answers its whole morsel"
    );
}
