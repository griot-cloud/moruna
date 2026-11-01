//! Historical rows follow actual logical boundaries while the executable stays fused.
use moruna_kernel::arrow::{
    array::{Int32Array, Int64Array},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use moruna_kernel::{Kernel, LogicalRows, LogicalTaskRows, MorunaError, NoState, Payload};
use moruna_kernels::{StdKernel, fuse};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Rows {
    batches: Mutex<Vec<(String, RecordBatch)>>,
    refuse: bool,
}
impl LogicalRows for Rows {
    fn append(&self, task: &str, port: &str, batch: &RecordBatch) -> moruna_kernel::Result<()> {
        assert_eq!(port, "out");
        if self.refuse {
            return Err(MorunaError::Unsupported(
                "controlled required capture refusal",
            ));
        }
        self.batches
            .lock()
            .unwrap()
            .push((task.to_string(), batch.clone()));
        Ok(())
    }
}
fn input() -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("other", DataType::Int32, false),
        ])),
        vec![
            Arc::new(Int32Array::from(vec![1, 2, 3])),
            Arc::new(Int32Array::from(vec![4, 5, 6])),
        ],
    )
    .unwrap()
}
fn tasks() -> Vec<LogicalTaskRows> {
    ["cast", "select"]
        .into_iter()
        .map(|task| LogicalTaskRows {
            task: task.into(),
            ports: vec!["out".into()],
        })
        .collect()
}
#[test]
fn fused_projection_retains_the_real_intermediate_and_final_rows_without_shared_binding() {
    let cast = StdKernel::new("cast", &json!({"columns":{"id":"int64"}})).unwrap();
    let select = StdKernel::new("select", &json!({"columns":["id"]})).unwrap();
    let fused = fuse(&cast, &select).expect("one fused executable");
    let rows = Arc::new(Rows::default());
    let bound = fused.with_logical_rows(&tasks(), rows.clone()).unwrap();
    assert_eq!(bound.fingerprint(), fused.fingerprint());
    let mut state = NoState;
    let result = bound
        .apply(&mut state, Payload::table(input()).unwrap())
        .unwrap();
    let Payload::Table(final_rows, _) = result else {
        panic!("table");
    };
    assert_eq!(final_rows.num_columns(), 1);
    let saved = rows.batches.lock().unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0].0, "cast");
    assert_eq!(saved[0].1.num_columns(), 2);
    assert_eq!(saved[0].1.column(0).data_type(), &DataType::Int64);
    assert_eq!(
        saved[0]
            .1
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .values(),
        &[1, 2, 3]
    );
    assert_eq!(saved[1].0, "select");
    assert_eq!(saved[1].1, final_rows);
    drop(saved);
    // Another run using the original shared kernel never publishes into this capture.
    fused
        .apply(&mut state, Payload::table(input()).unwrap())
        .unwrap();
    assert_eq!(rows.batches.lock().unwrap().len(), 2);
}
#[test]
fn required_tap_refusal_stops_the_fused_call_instead_of_returning_uncaptured_output() {
    let cast = StdKernel::new("cast", &json!({"columns":{"id":"int64"}})).unwrap();
    let select = StdKernel::new("select", &json!({"columns":["id"]})).unwrap();
    let fused = fuse(&cast, &select).unwrap();
    let bound = fused
        .with_logical_rows(
            &tasks(),
            Arc::new(Rows {
                refuse: true,
                ..Rows::default()
            }),
        )
        .unwrap();
    let error = bound
        .apply(&mut NoState, Payload::table(input()).unwrap())
        .expect_err("required refusal");
    assert!(
        error
            .to_string()
            .contains("controlled required capture refusal")
    );
}
