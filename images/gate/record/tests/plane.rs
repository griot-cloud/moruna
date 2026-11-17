//! The native path on the host: each fixture's input made into a data-plane record as the engine
//! makes one (an Arrow IPC stream laid out by `moruna_ipc_check::record::reframe`), the job
//! applied to it as a task applies it, and the record it wrote checked by the engine's own
//! record check, read back by arrow-ipc and compared with the Python function's output.

use std::fs::File;
use std::path::PathBuf;

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{DataType, Float64Type};
use arrow::ipc::reader::{FileReader, StreamReader};
use arrow::ipc::writer::StreamWriter;
use moruna_gate_functions::canon;
use moruna_gate_record::{Error, Job, apply};
use moruna_ipc_check::frame::{END_OF_STREAM, body_length, message_len};
use moruna_ipc_check::record::{Need, check_record, framing_need, reframe};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn read_ipc(path: &PathBuf) -> RecordBatch {
    let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

/// An 8-aligned buffer of `n` bytes, as a mapped region is.
fn aligned(n: usize) -> Vec<u64> {
    vec![0u64; n.div_ceil(8)]
}

/// `batch` as a data-plane record, in an 8-aligned buffer.
fn record_of(batch: &RecordBatch) -> (Vec<u64>, usize) {
    let mut stream = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut stream, &batch.schema()).unwrap();
        w.write(batch).unwrap();
        w.finish().unwrap();
    }
    let l1 = message_len(&stream).unwrap();
    let schema = &stream[..8 + l1];
    let l2 = message_len(&stream[8 + l1..]).unwrap();
    let rb = &stream[8 + l1..8 + l1 + 8 + l2];
    let body_at = 8 + l1 + 8 + l2;
    let body_len = body_length(rb).unwrap() as usize;
    let body = &stream[body_at..body_at + body_len];
    let r = reframe(schema, rb).unwrap();
    let mut buf = aligned(r.record_len as usize);
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut buf);
    bytes[..r.framing.len()].copy_from_slice(&r.framing);
    for (from, to, len) in r.moves {
        bytes[to as usize..(to + len) as usize]
            .copy_from_slice(&body[from as usize..(from + len) as usize]);
    }
    (buf, r.record_len as usize)
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
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

fn same(g: &dyn Array, w: &dyn Array, what: &str) {
    if w.data_type() == &DataType::Float64 {
        let (g, w) = (
            g.as_primitive::<Float64Type>(),
            w.as_primitive::<Float64Type>(),
        );
        assert_eq!(g.null_count(), w.null_count(), "{what}");
        for r in 0..w.len() {
            assert_eq!(g.is_valid(r), w.is_valid(r), "{what} row {r}");
            if w.is_valid(r) {
                let (x, y) = (g.value(r), w.value(r));
                assert!(
                    x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
                    "{what} row {r}"
                );
            }
        }
    } else {
        assert_eq!(g.to_data(), w.to_data(), "{what}");
    }
}

fn check_job(name: &str) {
    let job = Job::by_name(name).unwrap();
    assert_eq!(job.name(), name);
    let m: Value =
        serde_json::from_reader(File::open(fixtures().join("manifest.json")).unwrap()).unwrap();
    for (case, rec) in m["jobs"][name].as_object().unwrap() {
        let input = read_ipc(&fixtures().join(rec["input"].as_str().unwrap()));
        let (buf, len) = record_of(&input);
        let record = &bytemuck::cast_slice::<u64, u8>(&buf)[..len];
        let mut window = aligned(len * 8 + (1 << 22));
        let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
        let got = apply(job, record, w);
        if rec.get("error").is_some() {
            assert!(
                matches!(got, Err(Error::Function(_))),
                "{name}/{case}: {got:?}"
            );
            eprintln!("{name}/{case}: refused, as Python raised");
            continue;
        }
        let n = got.unwrap_or_else(|e| panic!("{name}/{case}: {e}"));
        let out = read_record(&w[..n]);
        // Bit for bit against the arrow-rs port (the same core); against Python by the canonical
        // hash, exact columns, and the tolerant ones by the functions crate's own test.
        let want = moruna_gate_functions::batch::by_name(name).unwrap()(&input).unwrap();
        assert_eq!(out.num_rows(), want.num_rows(), "{name}/{case}");
        assert_eq!(out.schema().fields().len(), want.schema().fields().len());
        for (i, (g, w)) in out
            .schema()
            .fields()
            .iter()
            .zip(want.schema().fields())
            .enumerate()
        {
            assert_eq!(g, w, "{name}/{case}: field {i}");
            same(
                out.column(i).as_ref(),
                want.column(i).as_ref(),
                &format!("{name}/{case}: {}", g.name()),
            );
        }
        let ours = canon::canonical(name, &[out], false).unwrap();
        assert_eq!(
            ours.hash_exact,
            rec["hash_exact"].as_str().unwrap(),
            "{name}/{case}"
        );
        assert_eq!(ours.rows, rec["rows"].as_u64().unwrap(), "{name}/{case}");
        let exact = ours.hash_all == rec["hash_all"].as_str().unwrap();
        eprintln!(
            "{name}/{case}: {} rows, the record {n} bytes, hash_exact matches, hash_all {exact}",
            ours.rows
        );
    }
}

#[test]
fn enrich_on_the_plane() {
    check_job("enrich");
}

#[test]
fn audit_on_the_plane() {
    check_job("audit");
}

#[test]
fn occupancy_on_the_plane() {
    check_job("occupancy");
}

#[test]
fn score_on_the_plane() {
    check_job("score");
}

#[test]
fn fw_minhash_on_the_plane() {
    check_job("fw_minhash");
}

#[test]
fn ov_attrs_on_the_plane() {
    check_job("ov_attrs");
}

#[test]
fn ov_shape_on_the_plane() {
    check_job("ov_shape");
}

#[test]
fn a_small_window_is_refused() {
    let input = read_ipc(&fixtures().join("inputs/nyc/main.arrow"));
    let (buf, len) = record_of(&input);
    let record = &bytemuck::cast_slice::<u64, u8>(&buf)[..len];
    let mut window = aligned(4096 * 4);
    let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
    match apply(Job::Audit, record, w) {
        Err(Error::WindowTooSmall { need, have }) => assert!(need > have),
        other => panic!("{other:?}"),
    }
}

#[test]
fn damaged_records_are_errors_not_panics() {
    let input = read_ipc(&fixtures().join("inputs/nyc/one.arrow"));
    let (buf, len) = record_of(&input);
    let good = bytemuck::cast_slice::<u64, u8>(&buf)[..len].to_vec();
    let mut window = aligned(1 << 20);
    let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
    // Truncated anywhere, and every framing byte flipped: never a panic.
    for cut in [0, 4, 8, 64, 512, len / 2, len - 1] {
        let mut b = aligned(cut);
        let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut b);
        bytes[..cut].copy_from_slice(&good[..cut]);
        let _ = apply(Job::Score, &bytes[..cut], w);
    }
    let Ok(Need::Framing(end)) = framing_need(&good) else {
        panic!()
    };
    for i in 0..end {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut b = aligned(len);
            let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut b);
            bytes[..len].copy_from_slice(&good);
            bytes[i] ^= flip;
            for job in [Job::Enrich, Job::Audit, Job::Occupancy, Job::Score] {
                let _ = apply(job, &bytes[..len], w);
            }
        }
    }
    assert!(Job::by_name("nope").is_none());
}

/// An answer bigger than a window, written part by part into small windows: the parts, read
/// back and joined, are the one-record answer, row for row and bit for bit.
#[test]
fn parts_rebuild_the_answer() {
    use moruna_gate_record::parts::{Carry, Wrote, apply_part};
    // Each window a few pages more than one row's record (a page a buffer, and the framing).
    let cases = [
        ("enrich", "inputs/nyc/main.arrow", 160),
        ("occupancy", "inputs/nyc/main.arrow", 160),
        ("score", "inputs/nyc/main.arrow", 40),
        ("fw_minhash", "inputs/fineweb/main.arrow", 64),
        ("ov_attrs", "inputs/overture/main.arrow", 72),
        ("ov_shape", "inputs/overture/main.arrow", 100),
    ];
    for (name, input, kib) in cases {
        let job = Job::by_name(name).unwrap();
        let batch = read_ipc(&fixtures().join(input));
        let (buf, len) = record_of(&batch);
        let record = &bytemuck::cast_slice::<u64, u8>(&buf)[..len];
        let mut big = aligned(len * 8 + (1 << 22));
        let whole = {
            let w: &mut [u8] = bytemuck::cast_slice_mut(&mut big);
            let n = apply(job, record, w).unwrap();
            read_record(&w[..n])
        };
        let mut window = aligned(kib << 10);
        let mut carry = Carry::new();
        let mut parts = Vec::new();
        for part in 0.. {
            let w: &mut [u8] = bytemuck::cast_slice_mut(&mut window);
            let wrote = apply_part(job, record, w, part, &mut carry)
                .unwrap_or_else(|e| panic!("{name} part {part}: {e}"));
            let (n, last) = match wrote {
                Wrote::Last(n) => (n, true),
                Wrote::More(n) => (n, false),
            };
            parts.push(read_record(&w[..n]));
            assert_eq!(carry.pending(), !last);
            if last {
                break;
            }
        }
        assert!(parts.len() > 1, "{name}: one part");
        let joined = arrow::compute::concat_batches(&whole.schema(), &parts).unwrap();
        assert_eq!(joined.num_rows(), whole.num_rows(), "{name}");
        for i in 0..whole.num_columns() {
            same(
                joined.column(i).as_ref(),
                whole.column(i).as_ref(),
                &format!("{name}: column {i}"),
            );
        }
        eprintln!("{name}: {} rows in {} parts", whole.num_rows(), parts.len());
    }
    // A later part with nothing carried, and a window too small for one row, are errors.
    let mut carry = Carry::new();
    let mut tiny = aligned(4096);
    let w: &mut [u8] = bytemuck::cast_slice_mut(&mut tiny);
    let input = read_ipc(&fixtures().join("inputs/nyc/main.arrow"));
    let (buf, len) = record_of(&input);
    let record = &bytemuck::cast_slice::<u64, u8>(&buf)[..len];
    assert!(apply_part(Job::Audit, record, w, 1, &mut carry).is_err());
    assert!(matches!(
        apply_part(Job::Audit, record, w, 0, &mut carry),
        Err(Error::WindowTooSmall { .. })
    ));
}
