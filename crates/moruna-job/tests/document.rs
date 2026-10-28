//! SDD 22 k: the document and the declaration types, read and written without a run (JB-T18 to JB-T24).

use moruna_job::{
    BudgetDoc, CheckpointDoc, Declaration, ErrorPolicyDoc, Fact, Flow, HostProfileDoc, Isolation,
    JobSpec, KindDoc, LISTING_VERSION, Listing, Needs, ReportDoc, Role, SCHEMA_DIALECT, SizerDoc,
    SpecError, TaskIdsDoc, TaskRefDoc, Urls, is_kind_name, is_task_id, settings_schema,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn minimal() -> JobSpec {
    JobSpec::new(
        entry(json!({"kind": "parquet", "url": "/in"})),
        Vec::new(),
        entry(json!({"kind": "parquet", "url": "/out"})),
    )
}

/// JB-T21 document_round_trip: every section parses, serialises and parses back to the same
/// document, the canonical form is a fixed point, and the identity is blind to `resume` and
/// `report`.
#[test]
fn jb_t21_document_round_trip() {
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
    assert_eq!(third.canonical_json(), canonical);
    assert!(!canonical.contains(' ') && !canonical.contains("null"));

    let many = r#"{"moruna_spec":1,"source":{"kind":"tensor","url":["/a","/b"]},
                   "sink":{"kind":"arrow_ipc","url":"/out"}}"#;
    let job = JobSpec::from_json(many).expect("tensor and ipc");
    assert!(job.canonical_json().contains(r#""url":["/a","/b"]"#));

    let plain = minimal();
    let mut moved = plain.clone();
    moved.resume = Some("auto".into());
    moved.report.file = Some("/elsewhere.json".into());
    assert_eq!(moved.identity_json(), plain.identity_json());
    let mut changed = plain.clone();
    changed.ordered = true;
    assert_ne!(changed.identity_json(), plain.identity_json());
    assert_eq!(
        JobSpec::from_json(
            r#"{"moruna_spec":1,"source":{"kind":"parquet","url":"/in","x":null},
                              "sink":{"kind":"parquet","url":"/out"}}"#
        )
        .expect("a null setting is left out")
        .identity_json(),
        plain.identity_json()
    );
}

/// JB-T22 document_refusals: a refusal names the field it was refused for; the text is the
/// runtime's, `spec refused: <field>: <reason>`.
#[test]
fn jb_t22_document_refusals_name_the_field() {
    let field = |text: &str| JobSpec::from_json(text).expect_err("refused").field;
    assert_eq!(field("not json"), "spec");
    assert_eq!(field("[1]"), "spec");
    assert_eq!(field(r#"{"source":{}}"#), "moruna_spec");
    assert_eq!(field(r#"{"moruna_spec":2}"#), "moruna_spec");
    let base = |extra: &str| {
        format!(
            r#"{{"moruna_spec":1,"source":{{"kind":"parquet","url":"/in"}},
                 "sink":{{"kind":"parquet","url":"/out"}}{extra}}}"#
        )
    };
    assert_eq!(field(&base(r#","budgett":{}"#)), "budgett");
    assert_eq!(field(&base(r#","budget":{"memory":1}"#)), "budget");
    assert_eq!(field(&base(r#","staging":{"dir":3}"#)), "staging");
    assert_eq!(field(&base(r#","kernels":{}"#)), "kernels");
    assert_eq!(
        field(&base(r#","kernels":[{"name":"cast"}]"#)),
        "kernels[0]"
    );
    assert_eq!(field(&base(r#","kernels":[{"kind":5}]"#)), "kernels[0]");
    assert_eq!(field(&base(r#","kernels":[{"kind":""}]"#)), "kernels[0]");
    assert_eq!(field(&base(r#","contracts":{}"#)), "contracts");
    assert_eq!(
        field(&base(r#","contracts":[{"document":{}}]"#)),
        "contracts[0]"
    );
    assert_eq!(field(&base(r#","ordered":"yes""#)), "ordered");
    assert_eq!(field(&base(r#","trace":5"#)), "trace");
    assert_eq!(
        field(&base(r#","host_profile":{"gds":"maybe"}"#)),
        "host_profile"
    );
    assert_eq!(
        field(r#"{"moruna_spec":1,"source":"parquet","sink":{"kind":"parquet"}}"#),
        "source"
    );
    let err = JobSpec::from_json(r#"{"moruna_spec":2}"#).expect_err("v2");
    assert_eq!(
        err.to_string(),
        "spec refused: moruna_spec: this build reads version 1, the document says 2"
    );
    // An entry's settings are an object.
    assert!(KindDoc::of("parquet", &3).is_err());
    let made = KindDoc::of("parquet", &json!({"url": "/in", "options": null})).expect("entry");
    assert_eq!(made, entry(json!({"kind": "parquet", "url": "/in"})));
    assert!(
        KindDoc::of("parquet", &f64::NAN)
            .expect_err("not an object")
            .to_string()
            .contains("parquet")
    );
    assert!(KindDoc::from_value(json!("parquet")).is_err());
    let missing = KindDoc::from_value(json!({"url": "/in"})).expect_err("no kind");
    assert!(missing.contains("missing `kind`"), "{missing}");
    assert_eq!(SpecError::new("x", "y").to_string(), "spec refused: x: y");
}

/// JB-T23 report_tasks: `report.tasks` names the chain as it is, each task once, and attempts
/// count from 1.
#[test]
fn jb_t23_report_tasks_name_the_chain() {
    let with = |report: Value| {
        let mut job = json!({"moruna_spec":1,"source":{"kind":"parquet","url":"/i"},
            "kernels":[{"kind":"std","name":"cast"}],
            "sink":{"kind":"parquet","url":"/o"}});
        job["report"] = report;
        JobSpec::from_value(job)
    };
    let tasks = |source: &str, kernel: &str, sink: &str| {
        json!({"tasks": {"source": {"id": source}, "kernels": [{"id": kernel}],
                          "sink": {"id": sink}}})
    };
    let ok = with(tasks("read", "cast", "write")).expect("one id each");
    assert_eq!(ok.report.attempt(), 1);
    assert_eq!(
        ok.report.tasks,
        Some(TaskIdsDoc {
            source: TaskRefDoc {
                id: "read".into(),
                members: Vec::new()
            },
            kernels: vec![TaskRefDoc {
                id: "cast".into(),
                members: Vec::new()
            }],
            sink: TaskRefDoc {
                id: "write".into(),
                members: Vec::new()
            },
        })
    );
    let refused = |report: Value| with(report).expect_err("refused").field;
    assert_eq!(refused(json!({"attempt": 0})), "report.attempt");
    assert_eq!(
        refused(json!({"tasks": {"source": {"id": "a"}, "kernels": [], "sink": {"id": "b"}}})),
        "report.tasks.kernels"
    );
    assert_eq!(refused(tasks("a", "b", "a")), "report.tasks.sink.id");
    assert_eq!(refused(tasks("-a", "b", "c")), "report.tasks.source.id");
    let mut many = tasks("a", "b", "c");
    many["tasks"]["source"]["members"] = json!(vec!["m"; 257]);
    assert_eq!(refused(many), "report.tasks.source.members");
    let mut bad = tasks("a", "b", "c");
    bad["tasks"]["kernels"][0]["members"] = json!(["fine", "not fine"]);
    assert_eq!(refused(bad), "report.tasks.kernels[0].members");
    assert!(is_task_id("a:b-c.d_1") && !is_task_id("") && !is_task_id(&"a".repeat(129)));
    assert_eq!(ReportDoc::default().attempt(), 1);
}

/// JB-T24 section_defaults: what a document leaves out is what the runtime fills, and the host
/// profile says when nothing is left for the environment to fill.
#[test]
fn jb_t24_section_defaults() {
    let job = minimal();
    assert_eq!(job.moruna_spec, 1);
    assert_eq!(job.budget, BudgetDoc::default());
    assert_eq!(job.checkpoint, CheckpointDoc::default());
    assert_eq!(
        job.checkpoint.interval_ms,
        moruna_job::CHECKPOINT_INTERVAL_MS
    );
    assert!(job.checkpoint.enabled && !job.checkpoint.keep);
    assert!(!HostProfileDoc::default().is_complete());
    let complete: HostProfileDoc = serde_json::from_value(json!({
        "huge_pages": "absent", "memlock": "absent", "io_uring": "present",
        "direct_io": "present", "gds": "absent", "rdma": "absent", "durable_staging": "absent"
    }))
    .expect("every guarantee");
    assert!(complete.is_complete());
}

/// A kind written in a test: a settings type, and the declaration its schema is derived for.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Probe {
    url: Urls,
    #[serde(default)]
    depth: Option<u8>,
}

fn declared(role: Role) -> Declaration {
    Declaration {
        name: "probe".into(),
        role,
        summary: "a kind the tests declare".into(),
        settings: settings_schema::<Probe>("probe", vec![json!({"url": "/a"})]),
        inputs: if role == Role::Source {
            Flow::Files
        } else {
            Flow::Batches
        },
        outputs: if role == Role::Sink {
            Flow::Files
        } else {
            Flow::Batches
        },
        needs: Needs::nothing(),
        deterministic: Fact::Yes,
        preserves_columns: Fact::Yes,
        isolation: Isolation::Builtin,
    }
}

/// JB-T18 declaration_check: a declaration is one the registry accepts for its role (SDD 18
/// KD-I2), each refusal naming why; and what it declares reads back as itself.
#[test]
fn jb_t18_declaration_check() {
    for role in [Role::Source, Role::Kernel, Role::Sink] {
        let good = declared(role);
        assert_eq!(good.check(role), Ok(()));
        assert_eq!(good.examples(), vec![json!({"url": "/a"})]);
        let text = serde_json::to_string(&good).expect("writes");
        assert_eq!(
            serde_json::from_str::<Declaration>(&text).expect("reads"),
            good
        );
    }
    let wrong = |role: Role, edit: &dyn Fn(&mut Declaration), says: &str| {
        let mut d = declared(role);
        edit(&mut d);
        let e = d.check(role).expect_err("refused");
        assert!(e.contains(says), "{e}");
    };
    wrong(
        Role::Kernel,
        &|d| d.name = "Probe".into(),
        "is not a kind name",
    );
    wrong(Role::Kernel, &|d| d.name = "9".into(), "is not a kind name");
    wrong(
        Role::Kernel,
        &|d| d.name = "a".repeat(65),
        "is not a kind name",
    );
    wrong(
        Role::Kernel,
        &|d| d.role = Role::Sink,
        "registered as a kernel",
    );
    wrong(Role::Kernel, &|d| d.summary = " ".into(), "has no summary");
    wrong(
        Role::Kernel,
        &|d| d.inputs = Flow::Files,
        "which a kernel does not",
    );
    wrong(
        Role::Source,
        &|d| d.inputs = Flow::Batches,
        "which a source does not",
    );
    wrong(
        Role::Sink,
        &|d| d.outputs = Flow::Batches,
        "which a sink does not",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["$schema"] = json!("other"),
        "is not in the dialect",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["type"] = json!("array"),
        "does not describe an object",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["additionalProperties"] = json!(true),
        "is not closed",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["properties"]["kind"] = json!({"type": "string"}),
        "has a property `kind`",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["examples"] = json!([]),
        "has no `examples`",
    );
    wrong(
        Role::Kernel,
        &|d| d.settings["examples"] = json!([3]),
        "has no `examples`",
    );
    let mut none = declared(Role::Kernel);
    none.settings = json!({});
    assert!(none.examples().is_empty());
    assert!(is_kind_name("a") && is_kind_name("a-b_c.d9") && !is_kind_name(""));
    assert_eq!(
        SCHEMA_DIALECT,
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(Role::Sink.to_string(), "sink");
    assert_eq!(
        (Role::Source.verb(), Role::Kernel.verb(), Role::Sink.verb()),
        ("reads", "runs", "writes")
    );
    assert_eq!(
        (
            Flow::Batches.as_str(),
            Flow::Files.as_str(),
            Flow::Nothing.as_str()
        ),
        ("batches", "files", "nothing")
    );
}

/// JB-T19 settings_schema_is_derived: the schema is the type's, titled with the kind, with its
/// examples; `Urls` is one string or a list of them and reads and writes as such.
#[test]
fn jb_t19_settings_schema_is_derived() {
    let schema = settings_schema::<Probe>("probe", vec![json!({"url": ["/a", "/b"]})]);
    assert_eq!(schema["title"], "probe");
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["url"]));
    assert_eq!(schema["examples"], json!([{"url": ["/a", "/b"]}]));
    assert!(schema["$defs"]["Urls"]["anyOf"].is_array());
    let one: Urls = serde_json::from_value(json!("/a")).expect("one");
    let many: Urls = serde_json::from_value(json!(["/a", "/b"])).expect("many");
    assert_eq!(one.0, vec!["/a"]);
    assert_eq!(serde_json::to_value(&one).expect("one"), json!("/a"));
    assert_eq!(
        serde_json::to_value(&many).expect("many"),
        json!(["/a", "/b"])
    );
    assert!(serde_json::from_value::<Urls>(json!(3)).is_err());
}

/// JB-T20 listing_reads_back: a listing is what `moruna kinds` prints, and reads back as itself.
#[test]
fn jb_t20_listing_reads_back() {
    let listing = Listing {
        moruna_kinds: LISTING_VERSION,
        moruna: "0.0.0".into(),
        sources: vec![declared(Role::Source)],
        kernels: vec![declared(Role::Kernel)],
        sinks: vec![declared(Role::Sink)],
    };
    let text = listing.to_json();
    assert_eq!(
        serde_json::from_str::<Listing>(&text).expect("reads"),
        listing
    );
    assert!(text.contains("\n  "), "indented for a person");
}
