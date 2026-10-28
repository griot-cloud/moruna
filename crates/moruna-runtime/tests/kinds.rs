//! KD-T5 add_a_kind (SDD 18 KD-I5): a kind this repository does not have is written here, in a
//! test, registered, and run by a job document, with no other change to any code. Its adapter
//! passes the runnable-kind suite first, which is what accepts a kind (decision AP1).

#![allow(clippy::result_large_err)]

mod support;

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Int64Array};
use moruna_job::{Declaration, Fact, Flow, Isolation, JobSpec, Needs, Role, settings_schema};
use moruna_kernel::{
    Fingerprint, InitCtx, Kernel, KernelHints, KernelKind, KernelState, MorunaError, NoState,
    Payload, PayloadSpec, SourceSchema, TierPref,
};
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::conformance::{self, Harness};
use moruna_runtime::kinds::{Context, Entry, Registry, RunnableKind, Stage, check_pin};
use moruna_runtime::{CancelToken, Runtime};
use serde_json::json;
use support::{Scratch, one_run_at_a_time, read_back_rows, write_parquet};

/// The kind: `{"kind": "test.scale", "factor": n}` multiplies the `value` column by `n`.
struct Scale;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScaleSettings {
    /// What `value` is multiplied by.
    factor: i64,
    /// The fingerprint the kernel must have.
    #[serde(default)]
    fingerprint: Option<String>,
}

impl RunnableKind for Scale {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "test.scale".into(),
            role: Role::Kernel,
            summary: "multiplies the `value` column by a factor".into(),
            settings: settings_schema::<ScaleSettings>("test.scale", vec![json!({"factor": 3})]),
            inputs: Flow::Batches,
            outputs: Flow::Batches,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
        }
    }

    fn stage(&self, entry: &Entry<'_>, _cx: &mut Context) -> moruna_kernel::Result<Stage> {
        let settings: ScaleSettings = entry.settings()?;
        if settings.factor == 0 {
            return Err(entry.refuse("factor", "0 would erase every value"));
        }
        let kernel = ScaleKernel {
            factor: settings.factor,
            fingerprint: Fingerprint::compute("test.scale", &settings.factor.to_le_bytes()),
        };
        check_pin(entry, settings.fingerprint.as_deref(), &kernel)?;
        Ok(Stage::of(Arc::new(kernel) as Arc<dyn Kernel>))
    }
}

struct ScaleKernel {
    factor: i64,
    fingerprint: Fingerprint,
}

impl Kernel for ScaleKernel {
    fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }

    fn hints(&self) -> KernelHints {
        KernelHints {
            expected_amplification: Some(1.0),
            ..Default::default()
        }
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: moruna_kernel::PayloadKind::Table,
            tier: TierPref::Host,
        }
    }

    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }

    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }

    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        let refuse = |msg: &str| MorunaError::Kernel {
            stage: 1,
            seq: 0,
            msg: msg.to_string(),
            features: None,
        };
        let Payload::Table(batch, _) = &input else {
            return Err(refuse("test.scale takes a table"));
        };
        let values = batch
            .column_by_name("value")
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .ok_or_else(|| refuse("no Int64 `value` column"))?;
        let scaled: Int64Array = values
            .iter()
            .map(|v| v.map(|v| v.wrapping_mul(self.factor)))
            .collect();
        input.with_column("value", Arc::new(scaled) as ArrayRef)
    }
}

/// The sum of `value` over every Parquet file under `dir`.
fn sum_of_values(dir: &std::path::Path) -> i64 {
    let mut sum = 0i64;
    for entry in std::fs::read_dir(dir).expect("the output directory") {
        let path = entry.expect("an entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        let file = std::fs::File::open(&path).expect("an output file");
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("a reader")
            .build()
            .expect("batches");
        for batch in reader {
            let batch = batch.expect("a batch");
            let values = batch
                .column_by_name("value")
                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                .expect("value");
            sum += (0..values.len()).map(|i| values.value(i)).sum::<i64>();
        }
    }
    sum
}

/// KD-T5 add_a_kind (KD-I5): the kind passes the runnable-kind suite, registers beside the
/// standard kinds, and a strict document that names it runs end to end and writes what it
/// computes; a registry without it refuses the same document by kind.
#[test]
fn kd_t5_add_a_kind() {
    let passed = conformance::kernel(&Scale, &Harness::new()).unwrap_or_else(|v| panic!("{v}"));
    assert!(passed.properties.contains(&"KD-C7"), "{passed:?}");

    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("kd_t5_add_a_kind");
    let rows = 12_000u64;
    let input = scratch.path().join("in.parquet");
    write_parquet(&input, rows, 4);
    let out = scratch.path().join("out");
    std::fs::create_dir_all(&out).expect("out");
    let staging = scratch.path().join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let job = JobSpec::from_value(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [{"kind": "test.scale", "factor": 3}],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": staging.to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": scratch.path().join("profiles").to_string_lossy(),
    }))
    .expect("the document parses");
    let opts = || BuildOptions {
        strict: true,
        env: &|_| None,
        notes: Vec::new(),
        packages: Default::default(),
        host_ports: None,
    };
    let refused = build(
        &moruna_job::Job::from_v1(&job),
        &Registry::standard(),
        opts(),
    )
    .err()
    .expect("the standard kinds do not have it");
    assert!(
        refused
            .to_string()
            .contains("kernels[0].kind: no kernel kind `test.scale` is registered"),
        "{refused}"
    );

    let mut kinds = Registry::standard();
    kinds.register_kernel(Arc::new(Scale)).expect("registers");
    assert!(
        kinds
            .listing()
            .kernels
            .iter()
            .any(|d| d.name == "test.scale"),
        "it is listed"
    );
    let built =
        build(&moruna_job::Job::from_v1(&job), &kinds, opts()).expect("the document builds");
    let report = Runtime::run(built.spec, CancelToken::new()).expect("the run completes");
    assert_eq!(read_back_rows(&out), rows);
    let written: i64 = (0..rows as i64).map(|v| v * 2 * 3).sum();
    assert_eq!(sum_of_values(&out), written, "every value was scaled by 3");
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("the report");
    let stage = json["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .find(|s| s["stage"] == 1)
        .expect("the kind's stage");
    assert_eq!(stage["rows_in"], rows);
}
