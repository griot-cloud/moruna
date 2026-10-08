//! SC-T24 (MH 4.1 `refused_rows`): a kernel that refuses single rows ([`Kernel::judge`]) fails
//! the run naming the first refused row by default; with a set-aside output the refused rows
//! are written there with their row, column and cause and their own values, the rows kept reach
//! the sink, and the run completes.

use std::sync::Arc;

use moruna_kernel::arrow::array::{Array, BooleanArray, Int64Array, StringArray, UInt64Array};
use moruna_kernel::arrow::compute::filter_record_batch;
use moruna_kernel::arrow::ipc::reader::FileReader;
use moruna_kernel::{
    CancelToken, Fingerprint, InitCtx, Judged, Kernel, KernelKind, KernelState, MorunaError,
    NoState, Payload, PayloadSpec, Result, RowRefusal, SourceSchema,
};
use moruna_testkit::FakeSource;

use super::common::RigBuilder;
use crate::RunOutcome;

/// Refuses every row whose value ends in 3, for column `value`.
struct EndsInThree;

impl Kernel for EndsInThree {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint([3; 32])
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: moruna_kernel::TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(&self, state: &mut dyn KernelState, input: Payload) -> Result<Payload> {
        self.judge(state, input)?.into_output()
    }

    fn judge(&self, _state: &mut dyn KernelState, input: Payload) -> Result<Judged> {
        let Payload::Table(batch, _) = &input else {
            return Err(MorunaError::Plan("a table".into()));
        };
        let values = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("the fake source's int64 column");
        let refused: Vec<RowRefusal> = (0..values.len())
            .filter(|&i| values.value(i) % 10 == 3)
            .map(|i| RowRefusal {
                row: i as u64,
                column: Some("value".into()),
                cause: "ends_in_three".into(),
            })
            .collect();
        let keep: BooleanArray = (0..values.len())
            .map(|i| Some(values.value(i) % 10 != 3))
            .collect();
        let kept =
            filter_record_batch(batch, &keep).map_err(|e| MorunaError::Plan(e.to_string()))?;
        Ok(Judged {
            output: Payload::table(kept)?,
            refused,
        })
    }
}

fn run(set_aside: Option<std::path::PathBuf>) -> RunOutcome {
    let rig = RigBuilder::new()
        .cfg(|cfg| {
            cfg.workers_max = 1;
            cfg.workers_active = 1;
            cfg.initial_morsel_target = 32;
            cfg.set_aside = set_aside;
        })
        .source(FakeSource::new().splits(3, 30, 240))
        .kernel(Arc::new(EndsInThree))
        .go();
    let outcome = rig.scheduler.run(CancelToken::new()).expect("the run");
    if let RunOutcome::Completed { .. } = &outcome {
        let summary = rig
            .scheduler
            .set_aside()
            .expect("a set-aside run's summary");
        assert_eq!(summary.rows, 9, "{summary:?}");
    }
    outcome
}

#[test]
fn sc_t24_a_refused_row_fails_the_run_by_default() {
    let outcome = run(None);
    let RunOutcome::Terminated { diagnostic, .. } = outcome else {
        panic!("expected Terminated, got {outcome:?}");
    };
    let text = diagnostic.to_string();
    assert!(
        text.starts_with("kernel stage 1 morsel 0: 1 row(s) refused; the first is row 3 of the source, column `value`: ends_in_three"),
        "{text}"
    );
}

#[test]
fn sc_t24_refused_rows_are_set_aside_and_the_rest_reach_the_sink() {
    let dir = std::env::temp_dir().join(format!(
        "moruna-sc-t24-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let outcome = run(Some(dir.clone()));
    let RunOutcome::Completed { sink } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert_eq!(sink.rows, 90 - 9, "every row kept reached the sink");

    let file = std::fs::File::open(dir.join("stage-1.arrow")).expect("stage 1's set-aside file");
    let reader = FileReader::try_new(file, None).expect("a finished Arrow IPC file");
    let names: Vec<String> = reader
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        [
            "refused_seq",
            "refused_row",
            "refused_source_row",
            "refused_column",
            "refused_cause",
            "value"
        ]
    );
    let mut found: Vec<(u64, i64, String, String)> = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let col = |i: usize| batch.column(i).clone();
        let source_row = col(2);
        let source_row = source_row.as_any().downcast_ref::<UInt64Array>().unwrap();
        let column = col(3);
        let column = column.as_any().downcast_ref::<StringArray>().unwrap();
        let cause = col(4);
        let cause = cause.as_any().downcast_ref::<StringArray>().unwrap();
        let value = col(5);
        let value = value.as_any().downcast_ref::<Int64Array>().unwrap();
        for i in 0..batch.num_rows() {
            found.push((
                source_row.value(i),
                value.value(i),
                column.value(i).to_string(),
                cause.value(i).to_string(),
            ));
        }
    }
    found.sort();
    let expected: Vec<(u64, i64, String, String)> = (0..3u32)
        .flat_map(|split| {
            [3u64, 13, 23].map(move |row| {
                (
                    u64::from(split) * 30 + row,
                    FakeSource::value_at(split, row),
                    "value".to_string(),
                    "ends_in_three".to_string(),
                )
            })
        })
        .collect();
    assert_eq!(
        found, expected,
        "each refused row, where it was, why, and its values"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sc_t24_a_resumed_run_does_not_set_rows_aside() {
    let mut cfg = super::common::cfg();
    cfg.resuming = true;
    cfg.set_aside = Some(std::env::temp_dir());
    match cfg.validate() {
        Err(MorunaError::Config { name, .. }) => assert_eq!(name, "refused_rows"),
        other => panic!("expected a refused configuration, got {other:?}"),
    }
}
