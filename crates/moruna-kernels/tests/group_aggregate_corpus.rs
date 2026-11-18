//! `moruna.group_aggregate@1`'s corpus against its CPU reference, through the `Kernel` contract
//! as the conformance runner drives a kernel: configure by identity, the output schema at plan,
//! one instance, every batch of the input a call, then `finish`; the outputs concatenated must
//! equal the expected file bit for bit (the specification declares no tolerance), or the case's
//! refusal must come at its step. Until 4.1's runner drives every kernel, this does it for this
//! one.

#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::sync::Arc;

use moruna_kernel::arrow::compute::concat_batches;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{InitCtx, MorunaError, Payload, SourceSchema};
use moruna_kernels::catalogue::implementations;
use moruna_kernels::catalogue::{Params, configure};
use moruna_kernels::conformance::{Expect, Step, check, read_corpus, read_source};
use moruna_testkit::FakeAllocator;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance")
        .join("group_aggregate")
}

/// What one case gives: the output, or the step and text of its refusal.
fn run(case: &moruna_kernels::conformance::Case) -> Result<RecordBatch, (Step, MorunaError)> {
    let identity = format!("moruna.group_aggregate@{}", case.version);
    let kernel = configure(&identity, &Params::json(case.params.clone()))
        .map_err(|e| (Step::Configure, e))?;
    let (schema, batches) = read_source(&dir(), &case.inputs[0]).expect("input");
    let SourceSchema::Table(output) = kernel
        .output_schema(&SourceSchema::Table(Arc::clone(&schema)))
        .map_err(|e| (Step::Schema, e))?
    else {
        panic!("a table gives a table");
    };
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(FakeAllocator::new()),
    };
    let apply = |e| (Step::Apply, e);
    let mut state = kernel.init(&ctx).map_err(apply)?;
    let mut out = Vec::new();
    for batch in batches {
        match kernel
            .apply(state.as_mut(), Payload::table(batch).map_err(apply)?)
            .map_err(apply)?
        {
            Payload::Table(b, _) => out.push(b),
            Payload::Tensor(..) => panic!("a tensor"),
        }
    }
    if let Some(Payload::Table(b, _)) = kernel.finish(state.as_mut()).map_err(apply)? {
        out.push(b);
    }
    for b in &out {
        assert_eq!(b.schema().fields(), output.fields(), "{}", case.name);
    }
    Ok(concat_batches(&output, &out).expect("concat"))
}

#[test]
fn the_corpus_passes_on_the_cpu_reference() {
    let corpus = read_corpus(&dir()).unwrap_or_else(|p| panic!("{}", p.join("\n")));
    assert_eq!(corpus.kernel, "moruna.group_aggregate");
    assert!(
        corpus.tolerance.is_empty(),
        "the specification declares none"
    );
    for case in &corpus.cases {
        match (&case.expect, run(case)) {
            (Expect::Output(file), Ok(got)) => {
                let (schema, batches) = read_source(
                    &dir(),
                    &moruna_kernels::conformance::Source {
                        file: file.clone(),
                        slice: None,
                    },
                )
                .expect("expected");
                let want = concat_batches(&schema, &batches).expect("concat");
                // Names, types and nullability; metadata is not compared.
                let fields = |s: &moruna_kernel::arrow::datatypes::Schema| {
                    s.fields()
                        .iter()
                        .map(|f| (f.name().clone(), f.data_type().clone(), f.is_nullable()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(fields(&got.schema()), fields(&schema), "{}", case.name);
                assert_eq!(got.num_rows(), want.num_rows(), "{}", case.name);
                // Arrow's equality compares values by their bytes: NaN matches NaN, -0.0 does
                // not match +0.0.
                for (i, (g, w)) in got.columns().iter().zip(want.columns()).enumerate() {
                    assert_eq!(
                        g.to_data(),
                        w.to_data(),
                        "{}: column {} differs",
                        case.name,
                        schema.field(i).name()
                    );
                }
            }
            (Expect::Error { at, contains }, Err((step, e))) => {
                assert_eq!(*at, step, "{}: refused at the wrong step: {e}", case.name);
                if let Some(text) = contains {
                    assert!(e.to_string().contains(text.as_str()), "{}: {e}", case.name);
                }
            }
            (Expect::Output(_), Err((step, e))) => {
                panic!("{}: refused at {step:?}: {e}", case.name)
            }
            (Expect::Error { at, .. }, Ok(_)) => {
                panic!("{}: expected a refusal at {at:?}", case.name)
            }
        }
    }
}

#[test]
fn the_catalogue_has_the_cpu_reference_and_agrees_with_the_spec() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let catalogue = check(&root, "moruna", implementations());
    let ours: Vec<&String> = catalogue
        .problems
        .iter()
        .filter(|p| p.contains("group_aggregate"))
        .collect();
    assert!(ours.is_empty(), "{ours:?}");
    assert!(catalogue.specs.contains_key("group_aggregate"));
    assert!(catalogue.corpora["group_aggregate"].cases.len() >= 20);
}
