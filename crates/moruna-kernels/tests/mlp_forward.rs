//! `moruna.mlp_forward@1`'s conformance corpus against its CPU reference, through the `Kernel`
//! trait, element by element within each case's allowed error (spec/mlp_forward.md, Tolerance).
//! This drives the cases until the catalogue's own runner drives every kernel (item 4.1).

#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use moruna_kernel::arrow::compute::concat_batches;
use moruna_kernel::arrow::datatypes::{DataType, Float32Type, Float64Type, SchemaRef};
use moruna_kernel::{InitCtx, Payload, SourceSchema};
use moruna_kernels::catalogue::{Params, configure};
use moruna_kernels::conformance::{Case, Expect, Step, read_corpus, read_source};
use moruna_testkit::FakeAllocator;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance")
        .join("mlp_forward")
}

fn one(dir: &Path, source: &moruna_kernels::conformance::Source) -> (SchemaRef, RecordBatch) {
    let (schema, batches) = read_source(dir, source).expect("a source");
    let batch = concat_batches(&schema, &batches).expect("one batch");
    (schema, batch)
}

/// The values of a Float or a list of Floats, as f64, row-major.
fn leaves(array: &ArrayRef) -> Vec<f64> {
    match array.data_type() {
        DataType::Float64 => array.as_primitive::<Float64Type>().values().to_vec(),
        DataType::Float32 => array
            .as_primitive::<Float32Type>()
            .values()
            .iter()
            .map(|&v| v as f64)
            .collect(),
        DataType::FixedSizeList(..) => {
            // A list's values are sliced with it.
            leaves(array.as_fixed_size_list().values())
        }
        other => panic!("no float leaves in {other}"),
    }
}

fn within(got: f64, want: f64, allowed: Option<f64>) -> bool {
    if want.is_nan() {
        return got.is_nan();
    }
    if want.is_infinite() {
        return got == want;
    }
    match allowed {
        Some(a) => (got - want).abs() <= a,
        None => got.to_bits() == want.to_bits() || (got == 0.0 && want == 0.0),
    }
}

fn run_case(dir: &Path, case: &Case) -> Result<(), String> {
    let mut params = Params::json(case.params.clone());
    for (name, source) in &case.arrays {
        params.arrays.insert(name.clone(), one(dir, source).1);
    }
    let refused = |at: Step, err: String| match &case.expect {
        Expect::Error { at: want, contains } if *want == at => match contains {
            Some(text) if !err.contains(text.as_str()) => {
                Err(format!("refused at {at:?} with `{err}`, without `{text}`"))
            }
            _ => Ok(()),
        },
        _ => Err(format!("refused at {at:?}: {err}")),
    };
    let kernel = match configure("moruna.mlp_forward@1", &params) {
        Ok(k) => k,
        Err(e) => return refused(Step::Configure, e.to_string()),
    };
    let mut outputs = Vec::new();
    for source in &case.inputs {
        let (schema, batches) = read_source(dir, source)?;
        let out_schema = match kernel.output_schema(&SourceSchema::Table(schema)) {
            Ok(SourceSchema::Table(s)) => s,
            Ok(_) => return Err("a tensor schema".into()),
            Err(e) => return refused(Step::Schema, e.to_string()),
        };
        let ctx = InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::new(FakeAllocator::new()),
        };
        let mut state = kernel.init(&ctx).map_err(|e| e.to_string())?;
        for batch in batches {
            let payload = Payload::table(batch).map_err(|e| e.to_string())?;
            match kernel.apply(state.as_mut(), payload) {
                Ok(Payload::Table(out, _)) => {
                    if out.schema() != out_schema {
                        return Err(format!(
                            "gave {:?}, and declared {out_schema:?}",
                            out.schema()
                        ));
                    }
                    outputs.push(out)
                }
                Ok(_) => return Err("a tensor".into()),
                Err(e) => return refused(Step::Apply, e.to_string()),
            }
        }
    }
    let Expect::Output(expected) = &case.expect else {
        return Err("expected a refusal, and the case ran".into());
    };
    let (want_schema, want) = one(
        dir,
        &moruna_kernels::conformance::Source {
            file: expected.clone(),
            slice: None,
        },
    );
    let declared =
        match kernel.output_schema(&SourceSchema::Table(read_source(dir, &case.inputs[0])?.0)) {
            Ok(SourceSchema::Table(s)) => s,
            _ => return Err("no output schema".into()),
        };
    let strip = |s: &SchemaRef| {
        s.fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().clone(), f.is_nullable()))
            .collect::<Vec<_>>()
    };
    if strip(&declared) != strip(&want_schema) {
        return Err(format!("declared {declared:?}, expected {want_schema:?}"));
    }
    let got = concat_batches(&declared, &outputs).map_err(|e| e.to_string())?;
    if got.num_rows() != want.num_rows() {
        return Err(format!(
            "{} rows, expected {}",
            got.num_rows(),
            want.num_rows()
        ));
    }
    for (c, field) in want_schema.fields().iter().enumerate() {
        let (g, w) = (got.column(c), want.column(c));
        let allowed: Option<Vec<Option<f64>>> = case.allowed.get(field.name()).map(|source| {
            let a = one(dir, source).1.column(0).clone();
            match a.data_type() {
                DataType::FixedSizeList(..) => {
                    let list = a.as_fixed_size_list();
                    list.values().as_primitive::<Float64Type>().iter().collect()
                }
                _ => a.as_primitive::<Float64Type>().iter().collect(),
            }
        });
        for r in 0..w.len() {
            if g.is_null(r) != w.is_null(r) {
                return Err(format!(
                    "row {r}: null {} where expected {}",
                    g.is_null(r),
                    w.is_null(r)
                ));
            }
        }
        let (gv, wv) = (leaves(g), leaves(w));
        let width = wv.len().checked_div(w.len()).unwrap_or(1).max(1);
        for (i, (x, e)) in gv.iter().zip(&wv).enumerate() {
            if w.is_null(i / width) {
                continue;
            }
            let a = allowed.as_ref().and_then(|a| a[i]);
            if !within(*x, *e, a) {
                return Err(format!(
                    "{} element {i}: {x} against {e}, allowed {a:?}",
                    field.name()
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn every_case_passes_on_the_cpu_reference() {
    let dir = dir();
    let corpus = read_corpus(&dir).unwrap_or_else(|p| panic!("{}", p.join("\n")));
    assert!(corpus.cases.len() >= 20);
    let failures: Vec<String> = corpus
        .cases
        .iter()
        .filter_map(|case| {
            run_case(&dir, case)
                .err()
                .map(|e| format!("{}: {e}", case.name))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_score_cases_use_their_tolerance_lightly() {
    // How close the reference comes to Score's float64 and float32 expectations, in units of the
    // allowed error: the tolerance is measured, so the reference should sit well inside it.
    let dir = dir();
    let corpus = read_corpus(&dir).expect("corpus");
    for name in ["score_float64", "score_float32"] {
        let case = corpus.cases.iter().find(|c| c.name == name).expect(name);
        let mut params = Params::json(case.params.clone());
        for (n, source) in &case.arrays {
            params.arrays.insert(n.clone(), one(&dir, source).1);
        }
        let kernel = configure("moruna.mlp_forward@1", &params).expect("configure");
        let ctx = InitCtx {
            instance: 0,
            device: None,
            alloc: Arc::new(FakeAllocator::new()),
        };
        let mut state = kernel.init(&ctx).expect("init");
        let input = one(&dir, &case.inputs[0]).1;
        let Payload::Table(out, _) = kernel
            .apply(state.as_mut(), Payload::table(input).expect("payload"))
            .expect("apply")
        else {
            panic!("a tensor")
        };
        let Expect::Output(file) = &case.expect else {
            panic!("an output case")
        };
        let want = one(
            &dir,
            &moruna_kernels::conformance::Source {
                file: file.clone(),
                slice: None,
            },
        )
        .1;
        let allowed: Vec<f64> = one(&dir, &case.allowed["predicted_tip"])
            .1
            .column(0)
            .as_primitive::<Float64Type>()
            .values()
            .to_vec();
        let worst = leaves(out.column(0))
            .iter()
            .zip(leaves(want.column(0)))
            .zip(&allowed)
            .map(|((g, w), a)| (g - w).abs() / a)
            .fold(0.0f64, f64::max);
        assert!(
            worst < 0.5,
            "{name}: the worst element uses {worst} of its allowed error"
        );
    }
}
