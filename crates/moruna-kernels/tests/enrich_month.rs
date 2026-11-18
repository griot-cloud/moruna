//! Enrich's filter as `moruna.filter@1` on a month (work item 4.3): reads the batches
//! `enrich_month.py prepare` wrote (Enrich's columns and its predicate `ok`), keeps the rows `ok`
//! selects, and writes them for `enrich_month.py verify` to hash against 0.6.6's.
//!
//! Ignored: it needs a month of data. `MORUNA_ENRICH_DIR` names the directory.

#![allow(clippy::result_large_err)]

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::arrow::ipc::writer::FileWriter;
use moruna_kernel::{InitCtx, Payload, SourceSchema};
use moruna_kernels::catalogue::{Params, configure};
use moruna_testkit::FakeAllocator;
use serde_json::json;

#[test]
#[ignore = "needs a month of data: see tests/enrich_month.py"]
fn enrich_filter_on_a_month() {
    let dir = PathBuf::from(std::env::var("MORUNA_ENRICH_DIR").expect("MORUNA_ENRICH_DIR"));
    let kernel =
        configure("moruna.filter@1", &Params::json(json!({"predicate": "ok"}))).expect("configure");
    let reader = FileReader::try_new_buffered(
        File::open(dir.join("enrich_in.arrow")).expect("input"),
        None,
    )
    .expect("an Arrow IPC file");
    let SourceSchema::Table(schema) = kernel
        .output_schema(&SourceSchema::Table(reader.schema()))
        .expect("plan")
    else {
        panic!("a tensor");
    };
    let mut writer = FileWriter::try_new(
        File::create(dir.join("enrich_out.arrow")).expect("output"),
        &schema,
    )
    .expect("writer");
    let alloc: Arc<dyn moruna_kernel::Allocator> = Arc::new(FakeAllocator::new());
    let mut state = kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc,
        })
        .expect("init");
    let (mut rows_in, mut rows_out) = (0, 0);
    for batch in reader {
        let batch = batch.expect("batch");
        rows_in += batch.num_rows();
        let Payload::Table(out, _) = kernel
            .apply(state.as_mut(), Payload::table(batch).expect("payload"))
            .expect("apply")
        else {
            panic!("a tensor");
        };
        rows_out += out.num_rows();
        writer.write(&out).expect("write");
    }
    writer.finish().expect("finish");
    println!("moruna.filter@1: {rows_in} rows in, {rows_out} rows out");
    assert!(rows_out <= rows_in);
}
