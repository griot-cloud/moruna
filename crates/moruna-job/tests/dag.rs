//! SDD 22 k: the job document version 2, a DAG of tasks (JB-T29 to JB-T36).

use moruna_job::{
    BudgetDoc, Catalogue, CheckpointDoc, ContractEntry, Edge, ElasticDoc, ErrorPolicyDoc,
    GuaranteeDoc, HostProfileDoc, JOB_VERSION, Job, JobError, JobSpec, KindDoc, ObjectStoreDoc,
    Refusal, RefusalCode, Reporting, S3Doc, SizerDoc, StagingDoc, Task, TaskIdsDoc, TaskRefDoc,
    check, is_job_flatbuffer,
};
use serde_json::{Value, json};

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn parquet_in(url: &str) -> KindDoc {
    entry(json!({"kind": "parquet", "url": url}))
}

fn parquet_out(url: &str) -> KindDoc {
    entry(json!({"kind": "parquet", "url": url}))
}

fn cast() -> KindDoc {
    entry(json!({"kind": "std", "name": "cast", "args": {"columns": {"a": "double"}}}))
}

fn union() -> KindDoc {
    entry(json!({"kind": "std", "name": "union"}))
}

fn standard() -> Catalogue {
    Catalogue::standard().expect("the standard catalogue")
}

fn refused(job: &Job) -> Vec<Refusal> {
    check(job, &standard()).expect_err("refused")
}

fn text(refusals: &[Refusal]) -> Vec<String> {
    refusals.iter().map(Refusal::to_string).collect()
}

/// Every section of the document set to something other than its default.
fn every_section() -> Job {
    let mut job = Job::new(
        vec![
            Task::source("in", parquet_in("/in")),
            Task {
                members: vec!["host-a".into(), "host-b".into()],
                ..Task::kernel("mask", cast()).barrier()
            },
            Task::kernel_with_inputs("join", union(), &["left", "right"]),
            Task::sink("out", parquet_out("/out")),
        ],
        vec![
            Edge::between("in", "mask"),
            Edge::new("mask", "out", "join", "left"),
            Edge::new("in", "out", "join", "right").barrier(),
            Edge::between("join", "out"),
        ],
    );
    job.id = Some("0123456789abcdef0123456789abcdef".into());
    job.budget = BudgetDoc {
        memory_bytes: Some(1 << 30),
        cpu: Some(2.5),
        elastic: Some(ElasticDoc {
            memory_max_bytes: Some(1 << 32),
            cpu_max: Some(8),
        }),
    };
    job.staging = StagingDoc {
        dir: Some("/staging".into()),
        url: Some("s3://b/p".into()),
        limit_bytes: Some(1 << 33),
        durable: Some(true),
    };
    job.object_store = ObjectStoreDoc {
        s3: Some(S3Doc {
            endpoint: Some("unix:///s".into()),
            region: Some("r".into()),
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            session_token: Some("t".into()),
            bucket: Some("b".into()),
        }),
        gcs: Some(moruna_job::GcsDoc {
            service_account_path: Some("/sa".into()),
            service_account_json: None,
            bucket: Some("g".into()),
        }),
        azure: Some(moruna_job::AzureDoc {
            account: Some("a".into()),
            access_key: None,
            container: Some("c".into()),
        }),
        local_root: Some("/root".into()),
        allow_http: true,
    };
    job.checkpoint = CheckpointDoc {
        enabled: false,
        interval_ms: 900,
        keep: true,
    };
    job.resume = Some("auto".into());
    job.error_policy = ErrorPolicyDoc::Budget(3);
    job.ordered = true;
    job.sizer = SizerDoc::Learned;
    job.trace = Some("/trace".into());
    job.profiles_dir = Some("/profiles".into());
    job.host_profile = Some(HostProfileDoc {
        huge_pages: Some(GuaranteeDoc::Present),
        memlock: Some(GuaranteeDoc::Absent),
        io_uring: None,
        direct_io: Some(GuaranteeDoc::Absent),
        gds: None,
        rdma: None,
        durable_staging: Some(GuaranteeDoc::Present),
        staging_dir: Some("/staging".into()),
    });
    job.allow_gil = true;
    job.report = Reporting {
        socket: Some("unix:///r".into()),
        file: Some("/report.json".into()),
        attempt: Some(2),
        logs: Some("/logs".into()),
    };
    job.contracts = vec![ContractEntry {
        document: json!({"name": "c", "rules": [1, 2.5, null, true, {"x": "y"}]}),
        compiled: "parcel-compiled/1".into(),
        functions: vec![json!({"module": "m"})],
        audiences: vec!["public".into()],
    }];
    job
}

/// JB-T29 job_round_trip: every section of a job survives FlatBuffers and back; the bytes carry
/// the identifier and are the same every time; the JSON is a rendering with every task and edge;
/// the identity is blind to `resume` and `report` and to nothing else.
#[test]
fn jb_t29_job_round_trip() {
    let job = every_section();
    let bytes = job.to_flatbuffer().expect("encodes");
    assert!(is_job_flatbuffer(&bytes));
    assert_eq!(&bytes[4..8], b"MJB2");
    assert_eq!(Job::from_flatbuffer(&bytes).expect("reads back"), job);
    assert_eq!(
        job.to_flatbuffer().expect("encodes"),
        bytes,
        "deterministic"
    );
    let defaults = Job::new(Vec::new(), Vec::new());
    let back = Job::from_flatbuffer(&defaults.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back, defaults);
    assert!(back.checkpoint.enabled, "checkpoint.enabled defaults on");

    let rendered: Value = serde_json::from_str(&job.to_json()).expect("JSON");
    assert_eq!(rendered["moruna_job"], json!(JOB_VERSION));
    assert_eq!(rendered["tasks"][2]["inputs"], json!(["left", "right"]));
    assert_eq!(rendered["tasks"][1]["barrier"], json!(true));
    assert_eq!(rendered["edges"][2]["barrier"], json!(true));
    assert_eq!(
        rendered["edges"][1]["to"],
        json!({"task": "join", "port": "left"})
    );

    let mut moved = job.clone();
    moved.resume = None;
    moved.report = Reporting::default();
    assert_eq!(moved.identity_json(), job.identity_json());
    assert_ne!(moved.canonical_json(), job.canonical_json());
    moved.tasks[0]
        .settings
        .insert("url".into(), json!("/elsewhere"));
    assert_ne!(moved.identity_json(), job.identity_json());
    assert_eq!(job.task_index("join"), Some(2));
    assert_eq!(job.edges_into("join", "right"), vec![2]);
    assert_eq!(job.edges_from("in", "out"), vec![0, 2]);
}

/// SplitMix64, the repository's seeded generator (SDD 01 d.15).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// JB-T30 verifier_refuses_corruption: bytes that are anyone's are verified before they are
/// read. Every seeded corruption of a whole document (one to four bytes set to a random value,
/// and a truncation) reads as a job or as a typed error and never panics; a buffer without the
/// identifier, of another version, or whose settings are not a FlexBuffers map is refused by
/// what it is.
#[test]
fn jb_t30_verifier_refuses_corrupted_bytes() {
    let bytes = every_section().to_flatbuffer().expect("encodes");
    let mut rng = Rng(0x006a_6f62_5f76_3330);
    let mut errors = 0usize;
    let mut panics = Vec::new();
    for case in 0..4000 {
        let mut corrupt = bytes.clone();
        if case % 5 == 4 {
            corrupt.truncate(rng.below(bytes.len()));
        } else {
            for _ in 0..=rng.below(4) {
                let at = rng.below(corrupt.len());
                corrupt[at] = rng.next() as u8;
            }
        }
        match std::panic::catch_unwind(|| Job::from_flatbuffer(&corrupt)) {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => errors += 1,
            Err(_) => panics.push(case),
        }
    }
    assert!(panics.is_empty(), "panicked on cases {panics:?}");
    assert!(errors > 1000, "the corruptions are refused, {errors} were");

    assert_eq!(
        Job::from_flatbuffer(b"{\"moruna_spec\": 1}"),
        Err(JobError::Identifier)
    );
    assert_eq!(Job::from_flatbuffer(&[]), Err(JobError::Identifier));
    let mut short = bytes.clone();
    short.truncate(12);
    assert!(matches!(
        Job::from_flatbuffer(&short),
        Err(JobError::Malformed(_))
    ));

    let refusal = JobError::Version(9);
    assert_eq!(
        refusal.to_string(),
        "job refused: version: this build reads version 2, the document says 9"
    );

    // Settings that are not a map are refused at the task: a job whose source's settings are
    // a FlexBuffers list, written through the generated builder.
    let mut b = flatbuffers::FlatBufferBuilder::new();
    let list = flexbuffers::to_vec(json!(["/in"])).expect("flexbuffers");
    let id = b.create_string("in");
    let kind = b.create_string("parquet");
    let settings = b.create_vector(&list);
    let task = moruna_job::fb::Task::create(
        &mut b,
        &moruna_job::fb::TaskArgs {
            id: Some(id),
            kind: Some(kind),
            settings: Some(settings),
            ..Default::default()
        },
    );
    let tasks = b.create_vector(&[task]);
    let root = moruna_job::fb::Job::create(
        &mut b,
        &moruna_job::fb::JobArgs {
            version: JOB_VERSION,
            tasks: Some(tasks),
            ..Default::default()
        },
    );
    moruna_job::fb::finish_job_buffer(&mut b, root);
    assert_eq!(
        Job::from_flatbuffer(b.finished_data()),
        Err(JobError::Field {
            field: "tasks[0].settings".into(),
            reason: "a map of settings, not a list".into()
        })
    );
    let mut b = flatbuffers::FlatBufferBuilder::new();
    let root = moruna_job::fb::Job::create(
        &mut b,
        &moruna_job::fb::JobArgs {
            version: 9,
            ..Default::default()
        },
    );
    moruna_job::fb::finish_job_buffer(&mut b, root);
    assert_eq!(
        Job::from_flatbuffer(b.finished_data()),
        Err(JobError::Version(9))
    );
}

/// JB-T31 v1_reads_as_one_path: a version 1 document is a DAG with one path: its source, each
/// kernel in stage order and its sink, each with the one port its role has, the ids version 1
/// gave the tasks (or `report.tasks`, members included), an edge of batches from each to the
/// next, and every other field as it was. It checks as the same document did.
#[test]
fn jb_t31_v1_reads_as_one_path() {
    let mut spec = JobSpec::new(parquet_in("/in"), vec![cast(), cast()], parquet_out("/out"));
    spec.ordered = true;
    spec.run_id = Some("ab".repeat(16));
    spec.report.attempt = Some(3);
    let job = Job::from_v1(&spec);
    let ids: Vec<&str> = job.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["source", "kernel-1", "kernel-2", "sink"]);
    assert_eq!(job.tasks[0].outputs, ["out"]);
    assert!(job.tasks[0].inputs.is_empty());
    assert_eq!(job.tasks[1].inputs, ["in"]);
    assert_eq!(job.tasks[3].inputs, ["in"]);
    assert!(job.tasks[3].outputs.is_empty());
    assert_eq!(job.edges.len(), 3);
    assert_eq!(job.edges[1], Edge::between("kernel-1", "kernel-2"));
    assert!(job.ordered);
    assert_eq!(job.id, spec.run_id);
    assert_eq!(job.report.attempt(), 3);
    assert_eq!(check(&job, &standard()), Ok(()));
    assert_eq!(job.topological_order(), Ok(vec![0, 1, 2, 3]));

    let one = |id: &str| TaskRefDoc {
        id: id.into(),
        members: Vec::new(),
    };
    spec.report.tasks = Some(TaskIdsDoc {
        source: one("read"),
        kernels: vec![
            TaskRefDoc {
                id: "fused".into(),
                members: vec!["a".into(), "b".into()],
            },
            one("second"),
        ],
        sink: one("write"),
    });
    let named = Job::from_v1(&spec);
    let ids: Vec<&str> = named.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["read", "fused", "second", "write"]);
    assert_eq!(named.tasks[1].members, ["a", "b"]);
    assert_eq!(named.edges[2], Edge::between("second", "write"));
    let copy = Job::from_v1(&JobSpec::new(
        parquet_in("/i"),
        Vec::new(),
        parquet_out("/o"),
    ));
    assert_eq!(copy.edges, vec![Edge::between("source", "sink")]);
}

/// The fan-out of SDD 22 e.4: one source to two kernels to two sinks.
fn fan_out() -> Job {
    Job::new(
        vec![
            Task::source("in", parquet_in("/in")),
            Task::kernel("a", cast()),
            Task::kernel("b", cast()),
            Task::sink("out-a", parquet_out("/a")),
            Task::sink("out-b", parquet_out("/b")),
        ],
        vec![
            Edge::between("in", "a"),
            Edge::between("in", "b"),
            Edge::between("a", "out-a"),
            Edge::between("b", "out-b"),
        ],
    )
}

/// The diamond: a source to A and B, both to C's two ports, C to a sink.
fn diamond() -> Job {
    Job::new(
        vec![
            Task::source("in", parquet_in("/in")),
            Task::kernel("a", cast()),
            Task::kernel("b", cast()),
            Task::kernel_with_inputs("c", union(), &["left", "right"]),
            Task::sink("out", parquet_out("/out")),
        ],
        vec![
            Edge::between("in", "a"),
            Edge::between("in", "b"),
            Edge::new("a", "out", "c", "left"),
            Edge::new("b", "out", "c", "right"),
            Edge::between("c", "out"),
        ],
    )
}

/// JB-T32 dags_pass: a fan-out, a fan-in of two sources, a diamond and a job of one task in each
/// role pass `check`.
#[test]
fn jb_t32_dags_pass() {
    assert_eq!(check(&fan_out(), &standard()), Ok(()));
    assert_eq!(check(&diamond(), &standard()), Ok(()));
    let fan_in = Job::new(
        vec![
            Task::source("x", parquet_in("/x")),
            Task::source("y", parquet_in("/y")),
            Task::kernel_with_inputs("u", union(), &["0", "1"]),
            Task::sink("out", parquet_out("/out")),
        ],
        vec![
            Edge::new("x", "out", "u", "0"),
            Edge::new("y", "out", "u", "1"),
            Edge::between("u", "out"),
        ],
    );
    assert_eq!(check(&fan_in, &standard()), Ok(()));
    assert_eq!(diamond().topological_order(), Ok(vec![0, 1, 2, 3, 4]));
}

/// JB-T33 cycle: edges that go round are refused naming the tasks on the cycle, and nothing
/// downstream of it is named as on it.
#[test]
fn jb_t33_a_cycle_is_refused() {
    let mut job = diamond();
    // c feeds a back: in -> a -> c -> a.
    job.tasks[1] = Task::kernel_with_inputs("a", union(), &["in", "back"]);
    job.edges[0] = Edge::new("in", "out", "a", "in");
    job.tasks[3].outputs.push("loop".into());
    job.edges.push(Edge::new("c", "loop", "a", "back"));
    let found = refused(&job);
    assert_eq!(
        text(&found),
        [
            "tasks[1]: the edges go round in a cycle through `a`, `c`: a job is a DAG, so no task \
          may feed itself"
        ]
    );
    assert_eq!(found[0].code, RefusalCode::Cycle);
    let mut self_loop = fan_out();
    self_loop.tasks[1] = Task::kernel_with_inputs("a", cast(), &["in", "again"]);
    self_loop.edges.push(Edge::new("a", "out", "a", "again"));
    assert_eq!(refused(&self_loop)[0].code, RefusalCode::Cycle);
}

/// JB-T34 port_refusals: an edge to a task or a port the job does not have, an input port no
/// edge feeds, an output port no edge reads, two edges into one input port, a port named twice
/// and ports a role does not have are each refused at the field, naming the task and the port.
#[test]
fn jb_t34_port_refusals() {
    let mut job = fan_out();
    job.edges[0] = Edge::new("in", "out", "a", "nowhere");
    job.edges[3] = Edge::new("bee", "out", "out-b", "in");
    let found = refused(&job);
    assert_eq!(
        text(&found),
        [
            "edges[0].to: no input port `nowhere` on `a`: `a`'s input ports are `in`",
            "edges[3].from: no task `bee`: an edge joins two tasks of the job",
            "tasks[1].inputs.in: no edge feeds `a`'s input `in`: add an edge into it, or drop \
             the port",
            "tasks[2].outputs.out: no edge reads `b`'s output `out`: add an edge out of it, or \
             drop the port",
        ]
    );
    assert!(
        found[..2]
            .iter()
            .all(|r| r.code == RefusalCode::UnknownPort)
    );
    assert!(
        found[2..]
            .iter()
            .all(|r| r.code == RefusalCode::Unconnected)
    );

    let mut crowded = fan_out();
    crowded.edges[3] = Edge::between("a", "out-a");
    let found = refused(&crowded);
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(
        found[1].to_string(),
        "tasks[3].inputs.in: 2 edges (edges[2], edges[3]) feed `out-a`'s input `in`, which takes \
         one: give the task a port for each input"
    );
    assert_eq!(found[1].code, RefusalCode::Crowded);

    let mut shapes = fan_out();
    shapes.tasks[0].inputs.push("in".into());
    shapes.tasks[3].outputs.push("out".into());
    shapes.tasks[1].inputs.push("in".into());
    let found = refused(&shapes);
    let said = text(&found);
    assert!(
        said.contains(
            &"tasks[0]: `in` is a source with 1 input ports and 1 output ports; a source has none \
              input ports and at least one output ports"
                .to_string()
        ),
        "{said:?}"
    );
    assert!(said.contains(&"tasks[1].inputs.in: `a` names the port `in` twice".to_string()));
    assert!(
        said.iter()
            .any(|s| s.starts_with("tasks[3]: `out-a` is a sink"))
    );
    assert!(found.iter().any(|r| r.code == RefusalCode::Flow));
}

/// JB-T35 flows_do_not_fit: an edge whose flow is not what its producer gives or its consumer
/// takes is refused at the edge.
#[test]
fn jb_t35_flows_that_do_not_fit() {
    let mut job = fan_out();
    job.edges[2].flow = moruna_job::Flow::Files;
    let found = refused(&job);
    assert_eq!(
        text(&found),
        [
            "edges[2]: carries files, and `a` gives batches: an edge carries what its producer gives \
          and its consumer takes"
        ]
    );
    assert_eq!(found[0].code, RefusalCode::Flow);
}

/// JB-T36 barriers: a barrier task with nothing upstream or downstream is refused; an edge that
/// goes around a barrier task, or around a barrier edge, from something upstream of it to
/// something downstream of it, is refused at the edge naming the barrier; an edge through it is
/// not.
#[test]
fn jb_t36_barriers() {
    let mut job = fan_out();
    job.tasks[0].barrier = true;
    job.tasks[3].barrier = true;
    let found = refused(&job);
    assert_eq!(
        text(&found),
        [
            "tasks[0]: `in` is a barrier with nothing upstream of it: a barrier stands between \
             tasks",
            "tasks[3]: `out-a` is a barrier with nothing downstream of it: a barrier stands \
             between tasks",
        ]
    );
    assert!(found.iter().all(|r| r.code == RefusalCode::Barrier));

    // The diamond with A a barrier: in -> b -> c goes around it.
    let mut masked = diamond();
    masked.tasks[1].barrier = true;
    assert_eq!(
        check(&masked, &standard()),
        Ok(()),
        "b does not reach a: no bypass"
    );
    let mut leak = diamond();
    leak.tasks[1].barrier = true;
    // A third input of c, fed straight from the source, around a.
    leak.tasks[3] = Task::kernel_with_inputs("c", union(), &["left", "right", "around"]);
    leak.edges.push(Edge::new("in", "out", "c", "around"));
    let found = refused(&leak);
    assert_eq!(
        text(&found),
        [
            "edges[5]: goes from `in` to `c` around the barrier `a`: what crosses a barrier passes \
          through it"
        ]
    );

    // A barrier edge: in -> a is the enforcement point and in -> c goes around it.
    let mut gate = leak.clone();
    gate.tasks[1].barrier = false;
    gate.edges[0].barrier = true;
    let found = refused(&gate);
    assert_eq!(
        text(&found),
        [
            "edges[5]: goes from `in` to `c` around the barrier `edges[0]`: what crosses a barrier \
          passes through it"
        ]
    );
    let mut through = diamond();
    through.edges[2].barrier = true;
    assert_eq!(check(&through, &standard()), Ok(()));
}

/// JB-T37 task_ids: a task id that is not one, or names two tasks, and a member that is not a
/// task id are refused at the task.
#[test]
fn jb_t37_task_ids() {
    let mut job = fan_out();
    job.tasks[1].id = "-bad".into();
    job.tasks[2].id = "in".into();
    job.tasks[4].members = vec!["no spaces".into()];
    let said = text(&refused(&job));
    assert!(said.contains(
        &"tasks[1].id: \"-bad\" is not a task id: 1 to 128 of ASCII letters, digits, `_`, `-`, \
          `.` and `:`, starting with a letter or a digit"
            .to_string()
    ));
    assert!(said.contains(
        &"tasks[2].id: \"in\" names tasks[0] already: a task id names one task".to_string()
    ));
    assert!(said.contains(&"tasks[4].members: \"no spaces\" is not a task id".to_string()));
}
