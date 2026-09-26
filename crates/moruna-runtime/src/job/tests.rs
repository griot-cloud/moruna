//! MH: the document's tests that need no run (HO-T1, HO-T2, HO-T3, HO-T4, HO-T9).

use std::sync::Arc;

use moruna_kernel::{ErrorPolicy, Guarantee, Kernel, MorunaError, SizerKind};
use moruna_testkit::FakeKernel;

use super::build::{self, BuildOptions, KernelLoader, LoadedKernel, NoKernels, env_reliance};
use super::*;

/// The example of MH 4.1, with every section present.
const FULL: &str = r#"{
  "moruna_spec": 1,
  "run_id": "0123456789abcdef0123456789abcdef",
  "source": {
    "kind": "parquet",
    "url": "file:///data/raw/events/",
    "options": {"columns": ["id", "text"], "filters": [["id", ">", 5], ["text", "==", "a"]]}
  },
  "kernels": [
    {"kind": "python", "module": "/job/kernels.py", "callable": "shout",
     "expected_amplification": 1.5, "releases_gil": true}
  ],
  "sink": {"kind": "parquet", "url": "file:///data/refined/events/",
           "options": {"row_group_bytes": 33554432, "compression": "snappy"}},
  "budget": {"memory_bytes": 1073741824, "cpu": 2.0,
             "elastic": {"memory_max_bytes": 34359738368, "cpu_max": 32}},
  "staging": {"dir": "/staging", "limit_bytes": 1073741824, "durable": true},
  "object_store": {"s3": {"endpoint": "http://minio:9000", "region": "us-east-1"},
                   "allow_http": true},
  "checkpoint": {"enabled": true, "interval_ms": 5000, "keep": false},
  "resume": null,
  "error_policy": {"budget": 3},
  "ordered": true,
  "sizer": "learned",
  "trace": null,
  "profiles_dir": "/staging/profiles",
  "host_profile": {"huge_pages": "absent", "staging_dir": "/staging"},
  "allow_gil": false,
  "report": {"socket": "vsock://2:5000", "file": "/staging/moruna-<run_id>/report.json"}
}"#;

fn no_env(_: &str) -> Option<String> {
    None
}

fn opts(strict: bool, env: &dyn Fn(&str) -> Option<String>) -> BuildOptions<'_> {
    BuildOptions {
        strict,
        env,
        notes: Vec::new(),
    }
}

/// A loader that hands back a fake kernel for every entry, whatever it names.
struct Fakes;

impl KernelLoader for Fakes {
    fn load(&self, _index: usize, _doc: &KernelDoc) -> moruna_kernel::Result<LoadedKernel> {
        Ok(LoadedKernel {
            kernel: Arc::new(FakeKernel::new()) as Arc<dyn Kernel>,
            #[cfg(feature = "python")]
            python: None,
        })
    }
}

fn minimal(sink: &str) -> JobSpec {
    JobSpec::new(
        SourceDoc::Parquet {
            url: Urls(vec!["file:///data/in/part.parquet".into()]),
            options: ParquetSourceOptions::default(),
        },
        Vec::new(),
        SinkDoc::Parquet {
            url: sink.into(),
            options: ParquetSinkOptions::default(),
        },
    )
}

/// HO-T1 spec_round_trip (H1): every section parses, serialises and parses back to the same
/// document, and the canonical form is a fixed point.
#[test]
fn ho_t1_spec_round_trip() {
    let job = JobSpec::from_json(FULL).expect("the example parses");
    assert_eq!(job.kernels.len(), 1);
    assert_eq!(job.error_policy, ErrorPolicyDoc::Budget(3));
    assert_eq!(job.sizer, SizerDoc::Learned);
    assert_eq!(job.staging.durable, Some(true));
    let again = JobSpec::from_json(&job.to_json_pretty()).expect("pretty parses");
    assert_eq!(again, job);
    let canonical = job.canonical_json();
    let third = JobSpec::from_json(&canonical).expect("canonical parses");
    assert_eq!(third, job);
    assert_eq!(
        third.canonical_json(),
        canonical,
        "the canonical form is a fixed point"
    );
    assert!(
        !canonical.contains(' '),
        "no whitespace in the canonical form"
    );
    assert!(!canonical.contains("null"), "absent fields are omitted");

    // A single url is written as a string, several as a list, and both read back.
    let many = r#"{"moruna_spec":1,"source":{"kind":"tensor","url":["/a","/b"]},
                   "sink":{"kind":"arrow_ipc","url":"/out"}}"#;
    let job = JobSpec::from_json(many).expect("tensor and ipc");
    assert!(job.canonical_json().contains(r#""url":["/a","/b"]"#));
    let iter = r#"{"moruna_spec":1,"source":{"kind":"iterator"},
                   "sink":{"kind":"tensor","url":"/out","options":{"format":"safetensors"}}}"#;
    assert_eq!(
        JobSpec::from_json(iter).expect("iterator").source,
        SourceDoc::Iterator
    );
}

/// HO-T2 spec_digest: the digest is the SHA-256 of the canonical form, independent of key
/// order, whitespace and spelled-out nulls, blind to `resume` and `report`, and changed by
/// anything that changes the job.
#[test]
fn ho_t2_spec_digest() {
    let job = JobSpec::from_json(FULL).expect("parses");
    let digest = job.digest();
    assert!(
        digest.starts_with("sha256:") && digest.len() == 7 + 64,
        "{digest}"
    );

    let reordered = r#"{"sink":{"url":"/out","kind":"parquet"},"moruna_spec":1,
                        "source":{"url":"/in","kind":"parquet"},"trace":null}"#;
    let plain = r#"{"moruna_spec":1,"source":{"kind":"parquet","url":"/in"},
                    "sink":{"kind":"parquet","url":"/out"}}"#;
    let a = JobSpec::from_json(reordered).expect("a");
    let b = JobSpec::from_json(plain).expect("b");
    assert_eq!(a.digest(), b.digest());

    let mut resumed = b.clone();
    resumed.resume = Some("auto".into());
    resumed.report.file = Some("/elsewhere.json".into());
    assert_eq!(
        resumed.digest(),
        b.digest(),
        "resume and report are not the job"
    );

    let mut changed = b.clone();
    changed.ordered = true;
    assert_ne!(changed.digest(), b.digest());
    let mut hinted = job.clone();
    hinted.kernels[0].expected_amplification = Some(2.0);
    assert_ne!(hinted.digest(), digest);
}

/// HO-T3 spec_refusals: a refusal names the field it was refused for.
#[test]
fn ho_t3_spec_refusals_name_the_field() {
    let field = |text: &str| JobSpec::from_json(text).expect_err("refused").field;
    assert_eq!(field("not json"), "spec");
    assert_eq!(field("[1]"), "spec");
    assert_eq!(field(r#"{"source":{}}"#), "moruna_spec");
    assert_eq!(field(r#"{"moruna_spec":2}"#), "moruna_spec");
    assert_eq!(field(r#"{"moruna_spec":"1"}"#), "moruna_spec");
    let base = |extra: &str| {
        format!(
            r#"{{"moruna_spec":1,"source":{{"kind":"parquet","url":"/in"}},
                 "sink":{{"kind":"parquet","url":"/out"}}{extra}}}"#
        )
    };
    assert_eq!(field(&base(r#","budgett":{}"#)), "budgett");
    assert_eq!(field(&base(r#","budget":{"memory":1}"#)), "budget");
    assert_eq!(field(&base(r#","staging":{"dir":3}"#)), "staging");
    assert_eq!(field(&base(r#","object_store":{"s4":{}}"#)), "object_store");
    assert_eq!(
        field(&base(r#","checkpoint":{"interval":1}"#)),
        "checkpoint"
    );
    assert_eq!(field(&base(r#","error_policy":"explode""#)), "error_policy");
    assert_eq!(field(&base(r#","sizer":"psychic""#)), "sizer");
    assert_eq!(
        field(&base(r#","host_profile":{"gds":"maybe"}"#)),
        "host_profile"
    );
    assert_eq!(field(&base(r#","report":{"sock":"x"}"#)), "report");
    assert_eq!(field(&base(r#","kernels":{}"#)), "kernels");
    assert_eq!(
        field(&base(r#","kernels":[{"kind":"java"}]"#)),
        "kernels[0]"
    );
    assert_eq!(field(&base(r#","ordered":"yes""#)), "ordered");
    assert_eq!(field(&base(r#","trace":5"#)), "trace");
    let bad_source =
        r#"{"moruna_spec":1,"source":{"kind":"datafusion"},"sink":{"kind":"parquet","url":"/o"}}"#;
    assert_eq!(field(bad_source), "source");
    let bad_sink = r#"{"moruna_spec":1,"source":{"kind":"parquet","url":"/i"},"sink":{"kind":"csv","url":"/o"}}"#;
    assert_eq!(field(bad_sink), "sink");
    let err = JobSpec::from_json(r#"{"moruna_spec":2}"#).expect_err("v2");
    assert_eq!(
        err.to_string(),
        "spec refused: moruna_spec: this build reads version 1, the document says 2"
    );
    let as_error: MorunaError = err.into();
    assert!(matches!(as_error, MorunaError::Config { name: "spec", .. }));
}

/// HO-T4 strict_refusal_texts (H8): each field the environment would fill is named with its
/// variable; the text is exact; without `--strict` each becomes a note and the run is built.
#[test]
fn ho_t4_strict_refusal_texts() {
    let job = minimal("file:///data/out");
    let env = |name: &str| match name {
        "MORUNA_BUDGET" => Some("6GiB".to_string()),
        "MORUNA_SPILL_DIR" => Some("/tmp".to_string()),
        _ => None,
    };
    assert_eq!(
        env_reliance(&job, &env),
        vec![
            ("budget.memory_bytes", "MORUNA_BUDGET"),
            ("staging.dir", "MORUNA_SPILL_DIR")
        ]
    );
    let Err(error) = build::build(&job, &NoKernels, opts(true, &env)) else {
        panic!("strict refuses");
    };
    assert_eq!(
        error.to_string(),
        "configuration error in spec: spec refused: budget.memory_bytes, staging.dir: strict mode \
         resolves no field from the environment, and this document would resolve \
         budget.memory_bytes from MORUNA_BUDGET, staging.dir from MORUNA_SPILL_DIR; set the \
         field in the document or unset the variable"
            .replace("configuration error in spec: ", &config_prefix()),
    );

    // Setting the fields in the document leaves nothing to the environment.
    let mut explicit = job.clone();
    explicit.budget.memory_bytes = Some(1 << 30);
    explicit.staging.dir = Some("/tmp".into());
    assert!(env_reliance(&explicit, &env).is_empty());

    // Every variable, one at a time.
    for (field, var) in build::ENV_FIELDS {
        let one = |name: &str| (name == var).then(|| "x".to_string());
        assert_eq!(env_reliance(&job, &one), vec![(field, var)], "{var}");
    }
    // A complete host profile leaves nothing to MORUNA_HOST_PROFILE; a partial one does.
    let profile_env =
        |name: &str| (name == "MORUNA_HOST_PROFILE").then(|| "gds=absent".to_string());
    let mut partial = job.clone();
    partial.host_profile = Some(HostProfileDoc {
        gds: Some(GuaranteeDoc::Absent),
        ..HostProfileDoc::default()
    });
    assert_eq!(env_reliance(&partial, &profile_env).len(), 1);
    let all = Some(GuaranteeDoc::Absent);
    partial.host_profile = Some(HostProfileDoc {
        huge_pages: all,
        memlock: all,
        io_uring: all,
        direct_io: all,
        gds: all,
        rdma: all,
        durable_staging: all,
        staging_dir: None,
    });
    assert!(env_reliance(&partial, &profile_env).is_empty());

    // Not strict: built, with a note per field.
    let built = build::build(&job, &NoKernels, opts(false, &env)).expect("lenient builds");
    assert_eq!(
        built.spec.notes,
        vec![
            "budget.memory_bytes resolved from MORUNA_BUDGET".to_string(),
            "staging.dir resolved from MORUNA_SPILL_DIR".to_string()
        ]
    );
    assert_eq!(built.env_resolved.len(), 2);
}

/// The prefix `MorunaError::Config` displays before its message.
fn config_prefix() -> String {
    let shown = MorunaError::Config {
        name: "spec",
        msg: "X".into(),
    }
    .to_string();
    shown.trim_end_matches('X').to_string()
}

/// HO-T9 build_translates_every_field (H1): each field of the document arrives in the
/// `RunSpec` field it names, with the clamps and notes the library path makes.
#[test]
fn ho_t9_build_translates_every_field() {
    let dir = std::env::temp_dir();
    let mut job = JobSpec::from_json(FULL).expect("parses");
    job.trace = Some(dir.to_string_lossy().into_owned());
    job.checkpoint.interval_ms = 100;
    job.staging.dir = Some(dir.to_string_lossy().into_owned());
    job.host_profile = None;
    let built = build::build(&job, &Fakes, opts(true, &no_env)).expect("builds");
    let spec = &built.spec;
    assert_eq!(
        built.run_id.map(|r| r.to_hex()).as_deref(),
        job.run_id.as_deref()
    );
    assert_eq!(built.digest, job.digest());
    assert_eq!(spec.spec_digest.as_deref(), Some(built.digest.as_str()));
    assert_eq!(spec.kernels.len(), 1);
    assert_eq!(spec.budget, Some(1 << 30));
    assert_eq!(spec.cpu, Some(2.0));
    assert_eq!(spec.trace_path, Some(dir.clone()));
    assert_eq!(spec.staging_dir, Some(dir.clone()));
    assert_eq!(spec.staging_limit, Some(1 << 30));
    assert_eq!(spec.error_policy, ErrorPolicy::Budget(3));
    assert!(spec.ordered);
    assert_eq!(spec.sizer, SizerKind::Learned);
    assert_eq!(spec.profiles_dir, Some("/staging/profiles".into()));
    assert!(spec.object_store.allow_http);
    assert_eq!(
        spec.object_store
            .s3
            .as_ref()
            .and_then(|s| s.endpoint.as_deref()),
        Some("http://minio:9000")
    );
    let profile = spec.host_profile.as_ref().expect("durable makes a profile");
    assert_eq!(profile.durable_staging, Guarantee::Present);
    assert_eq!(profile.huge_pages, Guarantee::Unknown);
    assert!(!spec.allow_gil);
    assert!(spec.checkpoint);
    assert_eq!(spec.checkpoint_interval_ms, 500);
    assert!(!spec.checkpoint_keep);
    assert!(spec.resume.is_none());
    assert_eq!(
        spec.notes,
        vec![
            "clamped checkpoint.interval_ms from 100 to 500".to_string(),
            "budget.elastic is recorded; this build sizes the run once, at start".to_string(),
        ]
    );
    assert_eq!(built.discovery.explicit_budget, Some(1 << 30));

    // A declared profile carries its keys; one that disagrees with `durable` is refused.
    let mut declared = job.clone();
    declared.host_profile = Some(HostProfileDoc {
        gds: Some(GuaranteeDoc::Absent),
        durable_staging: Some(GuaranteeDoc::Present),
        staging_dir: Some("/staging".into()),
        ..HostProfileDoc::default()
    });
    let built = build::build(&declared, &Fakes, opts(false, &no_env)).expect("declared");
    let profile = built.spec.host_profile.expect("profile");
    assert_eq!(profile.gds, Guarantee::Absent);
    assert_eq!(profile.staging_dir, Some("/staging".into()));
    declared.staging.durable = Some(false);
    assert_refused(&declared, "staging.durable");
    declared.staging.durable = None;
    declared.host_profile = Some(HostProfileDoc {
        staging_dir: Some("relative".into()),
        ..HostProfileDoc::default()
    });
    assert_refused(&declared, "host_profile.staging_dir");
    let mut only_profile = minimal("/out");
    only_profile.host_profile = Some(HostProfileDoc::default());
    assert!(build::build(&only_profile, &NoKernels, opts(false, &no_env)).is_ok());
}

fn assert_refused(job: &JobSpec, field: &str) {
    match build::build(job, &Fakes, opts(false, &no_env)) {
        Err(MorunaError::Config { name: "spec", msg }) => {
            assert!(
                msg.starts_with(&format!("spec refused: {field}: ")),
                "{msg}"
            );
        }
        Err(other) => panic!("{field}: refused as {other}"),
        Ok(_) => panic!("{field}: built"),
    }
}

/// The refusals `build` makes after the document parsed.
#[test]
fn build_refusals() {
    let mut job = minimal("/out");
    job.run_id = Some("xyz".into());
    assert_refused(&job, "run_id");

    let mut job = minimal("/out");
    job.kernels.push(KernelDoc::python("m", "k"));
    match build::build(&job, &NoKernels, opts(false, &no_env)) {
        Err(MorunaError::Config { msg, .. }) => assert!(msg.contains("kernels[0]"), "{msg}"),
        _ => panic!("NoKernels refuses a kernel"),
    }
    job.kernels[0].kind = KernelKindDoc::Rust;
    assert_refused(&job, "kernels[0].kind");
    job.kernels[0].kind = KernelKindDoc::Python;
    job.kernels[0].fingerprint = Some("sha256:00".into());
    assert_refused(&job, "kernels[0].fingerprint");
    let good = FakeKernel::new().fingerprint().to_hex();
    job.kernels[0].fingerprint = Some(format!("blake3:{good}"));
    assert!(build::build(&job, &Fakes, opts(false, &no_env)).is_ok());

    let mut job = minimal("/out");
    job.source = SourceDoc::Parquet {
        url: Urls(Vec::new()),
        options: ParquetSourceOptions::default(),
    };
    assert_refused(&job, "source.url");
    job.source = SourceDoc::Tensor {
        url: Urls(Vec::new()),
        options: TensorSourceOptions::default(),
    };
    assert_refused(&job, "source.url");
    job.source = SourceDoc::Iterator;
    assert_refused(&job, "source.kind");

    let mut job = minimal("/out");
    job.source = SourceDoc::Parquet {
        url: Urls(vec!["/in".into()]),
        options: ParquetSourceOptions {
            columns: None,
            filters: vec![FilterDoc("a".into(), "~".into(), serde_json::json!(1))],
        },
    };
    assert_refused(&job, "source.options.filters[0]");
    job.source = SourceDoc::Parquet {
        url: Urls(vec!["/in".into()]),
        options: ParquetSourceOptions {
            columns: None,
            filters: vec![
                FilterDoc("a".into(), "<".into(), serde_json::json!(1.5)),
                FilterDoc("b".into(), "gt".into(), serde_json::json!(u64::MAX)),
                FilterDoc("c".into(), "eq".into(), serde_json::json!(true)),
                FilterDoc("d".into(), "lt".into(), serde_json::json!(null)),
            ],
        },
    };
    assert_refused(&job, "source.options.filters[3]");

    let mut job = minimal("/out");
    job.sink = SinkDoc::Parquet {
        url: "/out".into(),
        options: ParquetSinkOptions {
            compression: Some("brotli".into()),
            ..ParquetSinkOptions::default()
        },
    };
    assert_refused(&job, "sink.options.compression");
    job.sink = SinkDoc::Tensor {
        url: "/out".into(),
        options: TensorSinkOptions {
            format: Some("npy".into()),
            ..TensorSinkOptions::default()
        },
    };
    assert_refused(&job, "sink.options.format");

    let mut job = minimal("/out");
    job.budget.cpu = Some(0.0);
    assert_refused(&job, "budget.cpu");
    let mut job = minimal("/out");
    job.budget.memory_bytes = Some(1 << 30);
    job.budget.elastic = Some(ElasticDoc {
        memory_max_bytes: Some(1 << 20),
        cpu_max: None,
    });
    assert_refused(&job, "budget.elastic.memory_max_bytes");

    // The sink equals source rule is the library's: a `Plan` error (12 f.3, PY-T14).
    let same = minimal("file:///data/in");
    assert!(matches!(
        build::build(&same, &NoKernels, opts(false, &no_env)),
        Err(MorunaError::Plan(_))
    ));
    // A trace path whose directory does not exist is the library's `trace.path` refusal.
    let mut job = minimal("/out");
    job.trace = Some("/no-such-moruna-dir/t.arrow".into());
    assert!(matches!(
        build::build(&job, &NoKernels, opts(false, &no_env)),
        Err(MorunaError::Config {
            name: "trace.path",
            ..
        })
    ));
    // Resume by run id with no staging directory is a `Resume` refusal (exit 5).
    let mut job = minimal("/out");
    job.resume = Some("auto".into());
    assert!(matches!(
        build::build(&job, &NoKernels, opts(false, &no_env)),
        Err(MorunaError::Resume(_))
    ));
    assert_eq!(build::resume_shape(&job), Some(translate::ResumeArg::Auto));
}

/// Every sink kind builds, and the sink sizes are clamped with the library's notes.
#[test]
fn every_sink_kind_builds() {
    let mut job = minimal("/out");
    job.sink = SinkDoc::Parquet {
        url: "/out".into(),
        options: ParquetSinkOptions {
            row_group_bytes: Some(1 << 20),
            file_bytes: Some(1 << 20),
            compression: Some("none".into()),
        },
    };
    let built = build::build(&job, &NoKernels, opts(false, &no_env)).expect("parquet");
    assert_eq!(
        built.spec.notes,
        vec![
            "clamped sink.row_group_bytes from 1048576 to 16777216".to_string(),
            "clamped sink.file_bytes from 1048576 to 67108864".to_string()
        ]
    );
    job.sink = SinkDoc::Tensor {
        url: "file:///out".into(),
        options: TensorSinkOptions {
            format: Some("safetensors".into()),
            one_file_per_morsel: true,
            name: Some("t".into()),
        },
    };
    assert!(build::build(&job, &NoKernels, opts(false, &no_env)).is_ok());
    job.sink = SinkDoc::ArrowIpc {
        url: "/out".into(),
        options: ArrowIpcSinkOptions { file_bytes: None },
    };
    assert!(build::build(&job, &NoKernels, opts(false, &no_env)).is_ok());
    job.source = SourceDoc::Tensor {
        url: Urls(vec!["file:///in/w.safetensors".into()]),
        options: TensorSourceOptions {
            tensors: Some(vec!["w".into()]),
        },
    };
    let mut store = ObjectStoreDoc {
        gcs: Some(GcsDoc {
            service_account_path: Some("/sa.json".into()),
            ..GcsDoc::default()
        }),
        azure: Some(AzureDoc {
            account: Some("acct".into()),
            ..AzureDoc::default()
        }),
        local_root: Some("/root".into()),
        ..ObjectStoreDoc::default()
    };
    store.s3 = Some(S3Doc::default());
    job.object_store = store;
    let built = build::build(&job, &NoKernels, opts(false, &no_env)).expect("tensor source");
    assert!(built.spec.object_store.gcs.is_some() && built.spec.object_store.azure.is_some());

    for (name, back) in [
        ("zstd", "zstd"),
        ("snappy", "snappy"),
        ("gzip", "gzip"),
        ("lz4", "lz4"),
        ("none", "none"),
    ] {
        let c = build::parse_compression(name).expect("known");
        assert_eq!(build::compression_name(c), back);
    }
    for name in ["mrb1", "safetensors"] {
        let f = build::parse_tensor_format(name).expect("known");
        assert_eq!(build::tensor_format_name(f), name);
    }
    assert!(build::is_spec_refusal(&MorunaError::Plan(String::new())));
    assert!(!build::is_spec_refusal(&MorunaError::Cancelled));
}

/// `KernelDoc::hints_set` lists the decorator arguments an entry sets.
#[test]
fn hints_are_listed() {
    let mut doc = KernelDoc::python("m", "k");
    assert!(doc.hints_set().is_empty());
    doc.stateful = Some(true);
    doc.instances = Some(2);
    doc.device_memory = Some(false);
    doc.accepts = Some("table".into());
    doc.tier = Some("host".into());
    doc.releases_gil = Some(true);
    doc.expected_amplification = Some(1.0);
    doc.preferred_rows = Some(10);
    doc.resume = Some("reinit".into());
    doc.state_bytes = Some(1);
    assert_eq!(doc.hints_set().len(), 10);
}
