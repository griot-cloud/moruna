//! Each port against the Python function it replaces, on the oracle's fixtures
//! (`bench/vm/gate_oracle.py`, `images/gate/fixtures/`): the same row count, the same content
//! hash (gate-canon/1), the same schema, and every column equal bit for bit, except a column
//! named with a tolerance, which is within it. A case the Python function raised on, the port
//! must refuse.
//!
//! The Rust canonical hash of the oracle's own output is checked against the hash `canon.py`
//! wrote, so the two implementations of gate-canon/1 agree.

use std::fs::File;
use std::path::PathBuf;

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::Float64Type;
use arrow::ipc::reader::FileReader;
use moruna_gate_functions::{batch, canon};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn read(path: &PathBuf) -> RecordBatch {
    let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    match batches.len() {
        0 => RecordBatch::new_empty(schema),
        1 => batches.into_iter().next().unwrap(),
        _ => arrow::compute::concat_batches(&schema, &batches).unwrap(),
    }
}

fn manifest() -> Value {
    serde_json::from_reader(File::open(fixtures().join("manifest.json")).unwrap()).unwrap()
}

/// What one case showed: rows, whether the hashes matched, and tolerant values that differ.
struct Outcome {
    rows: u64,
    exact: bool,
    all: bool,
    differing: usize,
    max_abs: f64,
}

/// Bit for bit, except that any NaN equals any NaN: a NaN's payload is not content (NumPy's
/// sort gives 0x7fff_ffff_ffff_ffff on one machine and another on the next), and gate-canon/1
/// maps every NaN to one.
fn same_column(g: &dyn Array, w: &dyn Array, what: &str) {
    if w.data_type() == &arrow::datatypes::DataType::Float64 {
        let (g, w) = (
            g.as_primitive::<Float64Type>(),
            w.as_primitive::<Float64Type>(),
        );
        assert_eq!(g.nulls(), w.nulls(), "{what}: nulls");
        for r in 0..w.len() {
            if w.is_valid(r) {
                let (x, y) = (g.value(r), w.value(r));
                assert!(
                    x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
                    "{what} row {r}: {x:?} against {y:?}"
                );
            }
        }
    } else {
        assert_eq!(g.to_data(), w.to_data(), "{what}");
    }
}

fn check_case(job: &str, case: &str, rec: &Value) -> Option<Outcome> {
    let dir = fixtures().join(job);
    let input = read(&fixtures().join(rec["input"].as_str().unwrap()));
    assert_eq!(
        input.num_rows() as u64,
        rec["rows_in"].as_u64().unwrap(),
        "{job}/{case}"
    );
    let f = batch::by_name(job).unwrap();
    let got = f(&input);
    if let Some(err) = rec.get("error") {
        assert!(
            got.is_err(),
            "{job}/{case}: Python raised {err}, the port gave a batch"
        );
        return None;
    }
    let got = got.unwrap_or_else(|e| panic!("{job}/{case}: {e}"));
    let want = read(&dir.join(format!("{case}.expected.arrow")));
    assert_eq!(got.num_rows(), want.num_rows(), "{job}/{case}: rows");
    // The schema: names, types, nullability.
    let (gs, ws) = (got.schema(), want.schema());
    assert_eq!(
        gs.fields().len(),
        ws.fields().len(),
        "{job}/{case}: columns"
    );
    for (g, w) in gs.fields().iter().zip(ws.fields()) {
        assert_eq!(g.name(), w.name(), "{job}/{case}");
        assert_eq!(g.data_type(), w.data_type(), "{job}/{case}: {}", g.name());
        assert_eq!(
            g.is_nullable(),
            w.is_nullable(),
            "{job}/{case}: {}",
            g.name()
        );
    }
    let spec = canon::spec(job).unwrap();
    let mut differing = 0;
    let mut max_abs = 0.0f64;
    for (i, field) in ws.fields().iter().enumerate() {
        let (g, w) = (got.column(i), want.column(i));
        let tolerant = spec
            .tolerant
            .iter()
            .find(|(n, _, _)| n == field.name())
            .filter(|_| w.data_type() == &arrow::datatypes::DataType::Float64);
        match tolerant {
            None => same_column(
                g.as_ref(),
                w.as_ref(),
                &format!("{job}/{case}: column {}", field.name()),
            ),
            Some((_, abs, rel)) => {
                let (mut col_diff, mut col_rel) = (0usize, 0.0f64);
                let (g, w) = (
                    g.as_primitive::<Float64Type>(),
                    w.as_primitive::<Float64Type>(),
                );
                for r in 0..w.len() {
                    let (x, y) = (
                        g.is_valid(r).then(|| g.value(r)),
                        w.is_valid(r).then(|| w.value(r)),
                    );
                    assert!(
                        canon::within(x, y, *abs, *rel),
                        "{job}/{case}: {} row {r}: {x:?} against {y:?}",
                        field.name()
                    );
                    if x.map(f64::to_bits) != y.map(f64::to_bits) {
                        differing += 1;
                        col_diff += 1;
                        if let (Some(x), Some(y)) = (x, y) {
                            max_abs = max_abs.max((x - y).abs());
                            if y != 0.0 && x.is_finite() && y.is_finite() {
                                col_rel = col_rel.max(((x - y) / y).abs());
                            }
                        }
                    }
                }
                eprintln!(
                    "{job}/{case}: {}: {col_diff} of {} values differ, max relative {col_rel:e}",
                    field.name(),
                    w.len()
                );
            }
        }
    }
    // The canonical hashes: Rust's of the oracle's output equal canon.py's, and the port's
    // equal the oracle's.
    let theirs = canon::canonical(job, &[want], true).unwrap();
    assert_eq!(
        theirs.rows,
        rec["rows"].as_u64().unwrap(),
        "{job}/{case}: canonical rows"
    );
    assert_eq!(
        theirs.hash_exact,
        rec["hash_exact"].as_str().unwrap(),
        "{job}/{case}: Rust and Python canon"
    );
    assert_eq!(
        theirs.hash_all,
        rec["hash_all"].as_str().unwrap(),
        "{job}/{case}: Rust and Python canon"
    );
    let ours = canon::canonical(job, &[got], true).unwrap();
    let cmp = canon::compare(job, &ours, &theirs).unwrap();
    assert!(cmp.ok, "{job}/{case}: {cmp}");
    Some(Outcome {
        rows: ours.rows,
        exact: cmp.hash_exact,
        all: cmp.hash_all,
        differing,
        max_abs,
    })
}

fn check_job(job: &str) {
    let m = manifest();
    let cases = m["jobs"][job]
        .as_object()
        .unwrap_or_else(|| panic!("no fixtures for {job}"));
    for (case, rec) in cases {
        match check_case(job, case, rec) {
            None => eprintln!("{job}/{case}: refused, as Python raised"),
            Some(o) => eprintln!(
                "{job}/{case}: {} rows, hash_exact {}, hash_all {}, tolerant values differing {} (max {:e})",
                o.rows, o.exact, o.all, o.differing, o.max_abs
            ),
        }
    }
}

#[test]
fn enrich_matches_python() {
    check_job("enrich");
}

#[test]
fn audit_matches_python() {
    check_job("audit");
}

#[test]
fn occupancy_matches_python() {
    check_job("occupancy");
}

#[test]
fn score_matches_python() {
    check_job("score");
}

#[test]
fn fw_minhash_matches_python() {
    check_job("fw_minhash");
}

#[test]
fn ov_attrs_matches_python() {
    check_job("ov_attrs");
}

#[test]
fn ov_shape_matches_python() {
    check_job("ov_shape");
}

#[test]
fn round2_matches_python() {
    let table = std::fs::read(fixtures().join("pyround.bin")).unwrap();
    let mut bad = Vec::new();
    for pair in table.as_chunks::<16>().0 {
        let x = f64::from_le_bytes(pair[..8].try_into().unwrap());
        let want = f64::from_le_bytes(pair[8..].try_into().unwrap());
        let got = moruna_gate_functions::pyfloat::py_round2(x);
        if got.to_bits() != want.to_bits() && !(got.is_nan() && want.is_nan()) {
            bad.push((x, got, want));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} differ: {:?}",
        bad.len(),
        table.len() / 16,
        &bad[..bad.len().min(10)]
    );
}
