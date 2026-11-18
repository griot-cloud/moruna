//! The seven tasks on moruna-function, on the data plane in memory (`moruna_function::host`):
//! each fixture case's input posted as the engine posts a batch, the task run as its `main` runs
//! it, and the records it wrote taken through the engine's record check and read back. Each is
//! compared bit for bit with the native path the tasks ran on before 5.1 (`moruna_gate_record`,
//! the record codec, on the same record) and with the oracle's canonical hash: the same rows.

use std::fs::File;
use std::path::PathBuf;

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{DataType, Float64Type};
use arrow::ipc::reader::{FileReader, StreamReader};
use moruna_function::code;
use moruna_function::host::{Input, Outcome, Plane, record};
use moruna_gate_functions::{batch, canon};
use moruna_gate_record::{Error, Job, compute, decode, encode, parts};
use moruna_ipc_check::frame::END_OF_STREAM;
use moruna_ipc_check::record::{Need, check_record, framing_need, stream_parts};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn manifest() -> Value {
    serde_json::from_reader(File::open(fixtures().join("manifest.json")).unwrap()).unwrap()
}

fn read_ipc(path: &str) -> RecordBatch {
    let reader = FileReader::try_new(File::open(fixtures().join(path)).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

fn join(batches: &[RecordBatch]) -> RecordBatch {
    arrow::compute::concat_batches(&batches[0].schema(), batches).unwrap()
}

/// A record read back as arrow-ipc reads a stream, after the engine's check passed it.
fn read_record(record: &[u8]) -> RecordBatch {
    let Ok(Need::Framing(end)) = framing_need(record) else {
        panic!("no framing")
    };
    check_record(&record[..end], record.len() as u64).expect("the engine's record check");
    let mut stream = record.to_vec();
    stream.extend_from_slice(&END_OF_STREAM);
    let reader = StreamReader::try_new(&stream[..], None).unwrap();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    join(&batches)
}

/// The job on `input` as a task ran it before 5.1: the record codec of `moruna_gate_record`,
/// over the same record the engine posts, into one window.
fn before(name: &str, input: &RecordBatch) -> Result<RecordBatch, Error> {
    let job = Job::by_name(name).unwrap();
    let r = record(input).unwrap();
    // 8-aligned, as a mapped region is.
    let mut words = vec![0u64; r.len().div_ceil(8)];
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
    bytes[..r.len()].copy_from_slice(&r);
    let mut window = vec![0u64; (r.len() * 8 + (1 << 22)).div_ceil(8)];
    let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
    let n = moruna_gate_record::apply(job, &bytes[..r.len()], w)?;
    Ok(read_record(&w[..n]))
}

/// The same columns, bit for bit (a NaN's payload aside, as gate-canon/1 has it).
fn same(got: &RecordBatch, want: &RecordBatch, what: &str) {
    assert_eq!(got.num_rows(), want.num_rows(), "{what}");
    assert_eq!(got.schema().fields(), want.schema().fields(), "{what}");
    for (i, f) in want.schema().fields().iter().enumerate() {
        let (g, w) = (got.column(i), want.column(i));
        if w.data_type() == &DataType::Float64 {
            let (g, w) = (
                g.as_primitive::<Float64Type>(),
                w.as_primitive::<Float64Type>(),
            );
            assert_eq!(g.null_count(), w.null_count(), "{what}: {}", f.name());
            for r in 0..w.len() {
                assert_eq!(g.is_valid(r), w.is_valid(r), "{what}: {} row {r}", f.name());
                let (x, y) = (g.value(r), w.value(r));
                assert!(
                    !w.is_valid(r) || x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
                    "{what}: {} row {r}: {x} and {y}",
                    f.name()
                );
            }
        } else {
            assert_eq!(g.to_data(), w.to_data(), "{what}: {}", f.name());
        }
    }
}

fn run(name: &str, plane: Plane, inputs: Vec<RecordBatch>) -> Outcome {
    let job = batch::by_name(name).unwrap();
    plane
        .named(format!("gate-{name}"))
        .run(inputs, move || moruna_gate_tasks::serve(job))
        .unwrap()
}

/// Every fixture case of `name`, each in a task of its own: the rows the task wrote are the
/// rows the task wrote before 5.1, and their hash is the oracle's; a case Python raised on ends
/// the task as before.
fn check_job(name: &str) {
    let m = manifest();
    for (case, rec) in m["jobs"][name].as_object().unwrap() {
        let input = read_ipc(rec["input"].as_str().unwrap());
        let outcome = run(name, Plane::new(), vec![input.clone()]);
        let what = format!("{name}/{case}");
        if rec.get("error").is_some() {
            let want = batch::by_name(name).unwrap()(&input).unwrap_err();
            assert_eq!(outcome.said, Some(code::REFUSED), "{what}");
            assert_eq!(outcome.log, [format!("gate-{name}: {want}")], "{what}");
            assert!(!outcome.answers[0].released, "{what}");
            assert!(
                matches!(before(name, &input), Err(Error::Function(_))),
                "{what}: refused before too"
            );
            continue;
        }
        assert_eq!(outcome.said, Some(code::DONE), "{what}: {:?}", outcome.log);
        assert_eq!(
            outcome.log,
            [format!("gate-{name}: answered 1 batches")],
            "{what}"
        );
        let answer = &outcome.answers[0];
        assert!(answer.released && outcome.refused.is_empty(), "{what}");
        let got = join(&answer.batches);
        same(&got, &before(name, &input).unwrap(), &what);
        let ours = canon::canonical(name, &[got], false).unwrap();
        assert_eq!(
            ours.hash_exact,
            rec["hash_exact"].as_str().unwrap(),
            "{what}"
        );
        assert_eq!(ours.rows, rec["rows"].as_u64().unwrap(), "{what}");
        eprintln!(
            "{what}: {} rows in {} records, the same as before 5.1; hash_exact matches, hash_all {}",
            ours.rows,
            answer.records.len(),
            ours.hash_all == rec["hash_all"].as_str().unwrap()
        );
    }
}

#[test]
fn enrich_on_moruna_function() {
    check_job("enrich");
}

#[test]
fn audit_on_moruna_function() {
    check_job("audit");
}

#[test]
fn occupancy_on_moruna_function() {
    check_job("occupancy");
}

#[test]
fn score_on_moruna_function() {
    check_job("score");
}

#[test]
fn fw_minhash_on_moruna_function() {
    check_job("fw_minhash");
}

#[test]
fn ov_attrs_on_moruna_function() {
    check_job("ov_attrs");
}

#[test]
fn ov_shape_on_moruna_function() {
    check_job("ov_shape");
}

/// Every case a job answers, posted to one task in order: each input's answer is its own.
#[test]
fn one_task_answers_every_case_in_order() {
    let m = manifest();
    for name in moruna_gate_functions::JOBS {
        let cases: Vec<RecordBatch> = m["jobs"][name]
            .as_object()
            .unwrap()
            .values()
            .filter(|rec| rec.get("error").is_none())
            .map(|rec| read_ipc(rec["input"].as_str().unwrap()))
            .collect();
        let outcome = run(name, Plane::new(), cases.clone());
        assert_eq!(outcome.said, Some(code::DONE), "{name}: {:?}", outcome.log);
        assert_eq!(outcome.answers.len(), cases.len());
        for (i, (answer, input)) in outcome.answers.iter().zip(&cases).enumerate() {
            assert!(answer.released, "{name} case {i}");
            same(
                &join(&answer.batches),
                &before(name, input).unwrap(),
                &format!("{name} case {i}"),
            );
        }
    }
}

/// An answer bigger than a window, in windows a few pages more than one row's record (as the
/// record path's own test cut it): the parts, joined, are the one-record answer, row for row
/// and bit for bit.
#[test]
fn parts_rebuild_the_answer() {
    let cases = [
        ("enrich", "inputs/nyc/main.arrow", 8),
        ("occupancy", "inputs/nyc/main.arrow", 8),
        ("score", "inputs/nyc/main.arrow", 2),
        ("fw_minhash", "inputs/fineweb/main.arrow", 4),
        ("ov_attrs", "inputs/overture/main.arrow", 4),
        ("ov_shape", "inputs/overture/main.arrow", 8),
    ];
    for (name, input, pages) in cases {
        let input = read_ipc(input);
        let whole = before(name, &input).unwrap();
        let one_row = record(&whole.slice(0, 1)).unwrap().len() as u64;
        let window = (one_row + pages * 4096).next_multiple_of(4096);
        let outcome = run(name, Plane::new().windows(2, window), vec![input.clone()]);
        assert_eq!(outcome.said, Some(code::DONE), "{name}: {:?}", outcome.log);
        let answer = &outcome.answers[0];
        assert!(answer.records.len() > 1, "{name}: one record");
        assert!(
            answer.records.iter().all(|r| r.len() as u64 <= window),
            "{name}"
        );
        same(&join(&answer.batches), &whole, name);
        eprintln!(
            "{name}: {} parts in windows of {} KiB, one row's record {} KiB",
            answer.records.len(),
            window >> 10,
            one_row >> 10
        );
    }
}

/// A window smaller than one row's record ends the task with code 8, as before 5.1.
#[test]
fn a_small_window_is_refused() {
    let input = read_ipc("inputs/nyc/main.arrow");
    let outcome = run("audit", Plane::new().windows(1, 4 * 4096), vec![input]);
    assert_eq!(outcome.said, Some(code::WINDOW_SMALL));
    assert!(!outcome.answers[0].released);
    assert!(
        outcome.log[0].starts_with("gate-audit: the output record takes "),
        "{:?}",
        outcome.log
    );
}

/// Each task's program, run on a host outside any plane: it finds none, says so after its name,
/// and ends with code 2, as on the core.
#[test]
fn each_program_runs_and_finds_no_plane_outside_one() {
    let programs = [
        env!("CARGO_BIN_EXE_gate-enrich"),
        env!("CARGO_BIN_EXE_gate-audit"),
        env!("CARGO_BIN_EXE_gate-occupancy"),
        env!("CARGO_BIN_EXE_gate-score"),
        env!("CARGO_BIN_EXE_gate-fw_minhash"),
        env!("CARGO_BIN_EXE_gate-ov_shape"),
        env!("CARGO_BIN_EXE_gate-ov_attrs"),
    ];
    for program in programs {
        let out = std::process::Command::new(program).output().unwrap();
        let name = std::path::Path::new(program)
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert_eq!(out.status.code(), Some(code::NO_PLANE as i32), "{name}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.starts_with(&format!("{name}: no plane")),
            "{name}: {said}"
        );
    }
}

/// `input`'s rows repeated to `n`.
fn repeated(input: &RecordBatch, n: usize) -> RecordBatch {
    let copies: Vec<RecordBatch> = (0..n.div_ceil(input.num_rows()))
        .map(|_| input.clone())
        .collect();
    join(&copies).slice(0, n)
}

/// The bytes of the record of `job`'s output on `input`, and of its first row's: as the record
/// path laid them out before 5.1, and as the SDK lays them out.
fn record_bytes(name: &str, input: &RecordBatch) -> [u64; 4] {
    let r = record(input).unwrap();
    let mut words = vec![0u64; r.len().div_ceil(8)];
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
    bytes[..r.len()].copy_from_slice(&r);
    let read = decode::Batch::read(&bytes[..r.len()]).unwrap();
    let o = compute(Job::by_name(name).unwrap(), &read).unwrap();
    let one = parts::slice_output(&o, 0, 1).unwrap();
    let out = batch::by_name(name).unwrap()(input).unwrap();
    [
        encode::lay_out(&o.fields, &o.columns, o.rows)
            .unwrap()
            .record_len,
        encode::lay_out(&one.fields, &one.columns, 1)
            .unwrap()
            .record_len,
        record(&out).unwrap().len() as u64,
        record(&out.slice(0, 1)).unwrap().len() as u64,
    ]
}

/// A record takes the bytes the record path's did, for one row and for 64Ki rows of input:
/// arrow-ipc's bitmaps of all ones are left out, as that encoder left them out.
#[test]
fn records_take_the_bytes_the_record_path_took() {
    let jobs = [
        ("enrich", "inputs/nyc/main.arrow"),
        ("audit", "inputs/nyc/main.arrow"),
        ("occupancy", "inputs/nyc/main.arrow"),
        ("score", "inputs/nyc/main.arrow"),
        ("fw_minhash", "inputs/fineweb/main.arrow"),
        ("ov_shape", "inputs/overture/main.arrow"),
        ("ov_attrs", "inputs/overture/main.arrow"),
    ];
    for (name, path) in jobs {
        let input = read_ipc(path);
        let [_, before_one, _, sdk_one] = record_bytes(name, &input);
        let [before_all, _, sdk_all, _] = record_bytes(name, &repeated(&input, 65_536));
        assert_eq!(sdk_one, before_one, "{name}: one row");
        assert_eq!(sdk_all, before_all, "{name}: 64Ki rows");
        eprintln!("{name}: one row {sdk_one} bytes, 64Ki rows of input {sdk_all} bytes, as before");
    }
}

/// The record the engine lays out for `input` (`moruna_kernel::ipc::encode_framing`, as it
/// posts a batch to a task), whole: its framing, then each body buffer in its place.
fn engine_record(input: &RecordBatch) -> Vec<u8> {
    let alloc = moruna_testkit::FakeAllocator::new();
    let (framing, bodies) = moruna_kernel::ipc::encode_framing(input, 4096, 0, &alloc).unwrap();
    let Ok(Need::Framing(n)) = framing_need(&framing) else {
        panic!("no framing")
    };
    let (_, batch_at) = stream_parts(&framing[..n]).unwrap();
    let body = moruna_ipc_check::frame::body_length(&framing[batch_at]).unwrap() as usize;
    let mut bytes = framing[..n].to_vec();
    bytes.resize(n + body, 0);
    for (at, buffer) in &bodies {
        bytes[*at..at + buffer.len()].copy_from_slice(buffer.as_slice());
    }
    bytes
}

/// The record arrow-ipc's own stream of `input` takes laid out as it is, every column with its
/// bitmap: what the engine posted before the bitmaps of all ones were left out.
fn as_written(input: &RecordBatch) -> u64 {
    use moruna_ipc_check::frame::message_len;
    let mut writer =
        arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &input.schema()).unwrap();
    writer.write(input).unwrap();
    let bytes = writer.into_inner().unwrap();
    let schema = message_len(&bytes).unwrap() + 8;
    let rb = message_len(&bytes[schema..]).unwrap() + 8;
    moruna_ipc_check::record::reframe(&bytes[..schema], &bytes[schema..schema + rb])
        .unwrap()
        .record_len
}

/// The engine's input records leave the bitmaps of all ones out through the same function as
/// the SDK (`moruna_ipc_check::valid`): each job's input record, as the fixture and at 64Ki
/// rows, is the SDK's byte for byte and smaller than it was, and a task answers the engine's
/// record with the rows it gave before 5.1 and the oracle's hash.
#[test]
fn the_engines_input_records_are_the_sdks_and_answer_the_same() {
    let jobs = [
        ("enrich", "inputs/nyc/main.arrow"),
        ("audit", "inputs/nyc/main.arrow"),
        ("occupancy", "inputs/nyc/main.arrow"),
        ("score", "inputs/nyc/main.arrow"),
        ("fw_minhash", "inputs/fineweb/main.arrow"),
        ("ov_shape", "inputs/overture/main.arrow"),
        ("ov_attrs", "inputs/overture/main.arrow"),
    ];
    let m = manifest();
    for (name, path) in jobs {
        let fixture = read_ipc(path);
        for (label, input) in [
            ("fixture", fixture.clone()),
            ("64Ki", repeated(&fixture, 65_536)),
        ] {
            let engine = engine_record(&input);
            assert_eq!(
                engine,
                record(&input).unwrap(),
                "{name} {label}: the SDK's record"
            );
            let was = as_written(&input);
            assert!(
                (engine.len() as u64) < was,
                "{name} {label}: no bitmap left out"
            );
            eprintln!(
                "{name} input, {label} ({} rows): {was} bytes before, {} after",
                input.num_rows(),
                engine.len()
            );
        }
        let job = batch::by_name(name).unwrap();
        let outcome = Plane::new()
            .named(format!("gate-{name}"))
            .run([Input::Record(engine_record(&fixture))], move || {
                moruna_gate_tasks::serve(job)
            })
            .unwrap();
        assert_eq!(outcome.said, Some(code::DONE), "{name}: {:?}", outcome.log);
        let got = join(&outcome.answers[0].batches);
        same(&got, &before(name, &fixture).unwrap(), name);
        let ours = canon::canonical(name, &[got], false).unwrap();
        assert_eq!(
            ours.hash_exact,
            m["jobs"][name]["main"]["hash_exact"].as_str().unwrap(),
            "{name}"
        );
    }
}
