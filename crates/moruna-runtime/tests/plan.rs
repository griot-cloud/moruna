//! A run starts from its plan (SDD 26 f.5), over real components: PN-T21 a run from a plan,
//! its events and its report; PN-T22's run half, a step made to wait on a slow input; PN-T23 an
//! invalid plan refused before anything runs; PN-T24 the scheduler's starting order is the
//! plan's; PN-T25 `moruna explain` and `moruna run --plan`; PN-T27 nothing fused across a
//! barrier.

#![allow(clippy::result_large_err)]

mod support;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moruna_job::{Edge, Job, KindDoc, Task};
use moruna_kernel::{
    Allocator, BoxFuture, CancelToken, Kernel, Payload, Result as MResult, RowRange, SourceSchema,
    Split, Tier,
};
use moruna_plan::{Plan, Resources};
use moruna_runtime::SourceSpec;
use moruna_runtime::host::exit;
use moruna_runtime::host::session::{self, HEARTBEAT, HostOptions, Mode, Outcome, Wire};
use moruna_runtime::job::{BuildOptions, build};
use moruna_runtime::kinds::Registry;
use serde_json::{Value, json};
use support::{Scratch, Sleeper, one_run_at_a_time, write_parquet};

fn entry(value: Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn resources() -> Resources {
    Resources {
        workers: 4,
        memory_start_bytes: 1 << 30,
        memory_ceiling_bytes: 1 << 30,
    }
}

fn planned(job: &Job) -> Plan {
    moruna_runtime::job::plan::levels(job, resources()).expect("planned")
}

fn opts() -> BuildOptions<'static> {
    BuildOptions {
        strict: false,
        env: &|_| None,
        notes: Vec::new(),
        packages: Default::default(),
        host_ports: None,
        pulled_images: None,
        args: Default::default(),
        held: Vec::new(),
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A Parquet source whose every read waits first: a slow input.
struct Slow {
    inner: Arc<dyn moruna_kernel::Source>,
    wait: Duration,
}

impl moruna_kernel::Source for Slow {
    fn schema(&self) -> SourceSchema {
        self.inner.schema()
    }

    fn plan(&self) -> MResult<Vec<Split>> {
        self.inner.plan()
    }

    fn read<'a>(
        &'a self,
        split: &'a Split,
        rows: Option<RowRange>,
        alloc: &'a dyn Allocator,
        tier: Tier,
    ) -> BoxFuture<'a, MResult<Payload>> {
        let wait = self.wait;
        Box::pin(async move {
            std::thread::sleep(wait);
            self.inner.read(split, rows, alloc, tier).await
        })
    }

    fn repeatable(&self) -> bool {
        self.inner.repeatable()
    }
}

/// The standard kinds; the support kernels as `python` (`pass`, a sleeper of a few
/// milliseconds); and `test.slow`, a Parquet source over `slow_input` whose reads each wait.
fn kinds(slow_input: &Path) -> Registry {
    let mut kinds = support::kinds_with(|index, callable| -> MResult<Arc<dyn Kernel>> {
        match callable {
            "pass" => Ok(Arc::new(Sleeper::new(3))),
            other => Err(moruna_kernel::MorunaError::Plan(format!(
                "kernels[{index}]: no test kernel `{other}`"
            ))),
        }
    });
    let url = format!("file://{}", slow_input.display());
    let slow = support::Handed::new("test.slow", move || {
        let url = url.clone();
        SourceSpec::Build(Box::new(move |ctx| {
            let inner = Arc::new(moruna_sources::ParquetSource::new(
                moruna_sources::ParquetSourceConfig {
                    urls: vec![url.clone()],
                    ..Default::default()
                },
                ctx.reactor.clone(),
                ctx.object_metadata()?,
            )?) as Arc<dyn moruna_kernel::Source>;
            Ok(Arc::new(Slow {
                inner,
                wait: Duration::from_millis(400),
            }) as Arc<dyn moruna_kernel::Source>)
        }))
    });
    if let Err(e) = kinds.register_source(Arc::new(slow)) {
        panic!("{e}");
    }
    kinds
}

fn pass() -> KindDoc {
    entry(json!({"kind": "python", "module": "tests", "callable": "pass"}))
}

/// Two paths: `fast` (Parquet) through `kb` and `kc` into the sink `sb`; `slow` (whose reads
/// wait) straight into the sink `sa`. The levels planner groups them `{fast, slow}`, `{kb, sa}`,
/// `{kc}`, `{sb}`: `sb` is planned after `sa`, and `sa` waits on its slow input.
fn two_paths(scratch: &Scratch) -> Job {
    let dir = scratch.path();
    let input = dir.join("in.parquet");
    write_parquet(&input, 8_000, 8);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).expect("staging");
    let sink = |name: &str| {
        let out = dir.join(name);
        std::fs::create_dir_all(&out).expect("out");
        entry(json!({"kind": "parquet", "url": format!("file://{}", out.display())}))
    };
    let mut job = Job::new(
        vec![
            Task::source(
                "fast",
                entry(json!({"kind": "parquet", "url": format!("file://{}", input.display())})),
            ),
            Task::source("slow", KindDoc::new("test.slow", serde_json::Map::new())),
            Task::kernel("kb", pass()),
            Task::kernel("kc", pass()),
            Task::sink("sa", sink("out-a")),
            Task::sink("sb", sink("out-b")),
        ],
        vec![
            Edge::between("fast", "kb"),
            Edge::between("kb", "kc"),
            Edge::between("kc", "sb"),
            Edge::between("slow", "sa"),
        ],
    );
    job.budget.memory_bytes = Some(1 << 30);
    job.budget.cpu = Some(4.0);
    job.staging.dir = Some(staging.display().to_string());
    job.staging.limit_bytes = Some(1 << 30);
    job.profiles_dir = Some(dir.join("profiles").display().to_string());
    job.report.file = Some(dir.join("report-<run_id>.json").display().to_string());
    job
}

/// Everything the peer reads.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<u8>>>);

impl Write for Heard {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Heard {
    fn lines(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        moruna_runtime::host::protocol::json_messages(&bytes).expect("the peer reads the protocol")
    }
}

/// Run `job` as `moruna run` does, from `plan` when one is given.
fn run(job: Job, plan: Option<Plan>, kinds: &Registry) -> (Outcome, Vec<Value>) {
    let heard = Heard::default();
    let wire = Wire::new(Some(Box::new(heard.clone())), Instant::now());
    let opts = HostOptions {
        cost: None,
        strict: false,
        env: &no_env,
        cancel: CancelToken::new(),
        heartbeat: HEARTBEAT,
        packages: Default::default(),
        capture_process: false,
        events: Default::default(),
        args: Vec::new(),
        pulled_images: None,
    };
    let outcome = match plan {
        Some(plan) => session::run_plan(plan, job, kinds, &opts, &wire, None, Mode::Run),
        None => session::run_job(job, kinds, &opts, &wire, None, Mode::Run),
    };
    (outcome, heard.lines())
}

fn step_of(plan: &Plan, task: &str) -> String {
    plan.step_of(task).expect("a step runs it").id.clone()
}

/// PN-T21 (PN-I10) and PN-T22's run half (PN-I11): a job run with no plan given runs from the
/// default planner's, whose step ids are the ones `explain` gives; every task event names its
/// step; each step is told started and ended; the report names the plan, the order the steps
/// started in, each step's start, end and use; and `sb`, planned after `sa`, starts while `sa`
/// waits on its slow input: a deviation, reported with that reason.
#[test]
fn pn_t21_a_run_from_its_plan() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("pn_t21");
    let job = two_paths(&scratch);
    let kinds = kinds(&scratch.path().join("in.parquet"));
    let expected = planned(&job);
    let (outcome, lines) = run(job.clone(), None, &kinds);
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);

    let events: Vec<&Value> = lines.iter().filter(|l| l["type"] == "event").collect();
    assert!(events.iter().all(|e| e["v"] == 7));
    let attempt = events
        .iter()
        .find(|e| e["kind"] == "attempt_started")
        .expect("the attempt");
    assert_eq!(
        attempt["plan"],
        expected.id.as_str(),
        "the plan's id is the explained one's"
    );
    for event in &events {
        let kind = event["kind"].as_str().expect("a kind");
        if kind.starts_with("task_") && !event["task"].is_null() {
            let task = event["task"].as_str().expect("a task");
            assert_eq!(event["step"], step_of(&expected, task).as_str(), "{event}");
        }
    }
    let started: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "step_started")
        .map(|e| e["step"].as_str().expect("a step"))
        .collect();
    assert_eq!(started.len(), 6, "every step started once: {started:?}");
    let ended = events.iter().filter(|e| e["kind"] == "step_ended").count();
    assert_eq!(ended, 6);

    let ran = outcome.plan.expect("the run's plan");
    assert_eq!(ran.plan, expected.id);
    assert_eq!(
        ran.order, started,
        "the report's order is the order the steps started in"
    );
    for step in &ran.steps {
        assert!(
            step.start_us.is_some() && step.end_us >= step.start_us,
            "{step:?}"
        );
        assert_eq!(
            step.outcome,
            Some(moruna_runtime::host::protocol::StepOutcome::Succeeded)
        );
    }
    let kb = ran.steps.iter().find(|s| s.tasks == ["kb"]).expect("kb");
    assert_eq!(kb.used.rows_in, 8_000);
    assert!(
        kb.used.worker_seconds.is_some(),
        "a kernel step's worker time"
    );
    let sa = ran.steps.iter().find(|s| s.tasks == ["sa"]).expect("sa");
    assert!(
        sa.used.worker_seconds.is_none(),
        "a sink's drive is not a worker"
    );

    let early = ran
        .deviations
        .iter()
        .find(|d| d.step == step_of(&expected, "sb"))
        .unwrap_or_else(|| panic!("sb started ahead of sa: {:?}", ran.deviations));
    let behind = early
        .ahead_of
        .iter()
        .find(|b| b.step == step_of(&expected, "sa"))
        .expect("ahead of sa");
    assert_eq!(
        behind.reason,
        moruna_runtime::host::lifecycle::DeviationReason::AwaitingInput
    );
    assert_eq!(behind.blocking, [step_of(&expected, "slow")]);

    // The report file and the `report` message carry the same account.
    let file = outcome.report_file.expect("a report file");
    let envelope = session::read_report_file(&file).expect("the report file");
    assert_eq!(envelope["plan"]["plan"], expected.id.as_str());
    assert_eq!(envelope["plan"]["order"].as_array().map(Vec::len), Some(6));
    let report = lines
        .iter()
        .find(|l| l["type"] == "report")
        .expect("a report message");
    assert_eq!(report["plan"]["plan"], expected.id.as_str());
}

/// PN-T23 (PN-I4): a run given a plan that is not a plan of its job is refused before anything
/// runs, naming the step at fault; a plan of another job is refused as such.
#[test]
fn pn_t23_an_invalid_plan_is_refused_naming_the_step() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("pn_t23");
    let job = two_paths(&scratch);
    let kinds = kinds(&scratch.path().join("in.parquet"));
    let mut plan = planned(&job);
    let kc = plan
        .steps
        .iter_mut()
        .find(|s| s.tasks == ["kc"])
        .expect("kc");
    kc.depends_on.clear();
    let kc = kc.id.clone();
    let (outcome, lines) = run(job.clone(), Some(plan), &kinds);
    assert_eq!(outcome.code, exit::SPEC_REFUSED, "{:?}", outcome.diagnostic);
    let diagnostic = outcome.diagnostic.expect("why");
    assert!(diagnostic.contains(&kc), "{diagnostic}");
    assert!(diagnostic.contains("does not wait"), "{diagnostic}");
    assert!(
        !lines
            .iter()
            .any(|l| l["kind"] == "task_started" || l["kind"] == "step_started"),
        "nothing ran"
    );
    let failed = lines
        .iter()
        .find(|l| l["kind"] == "task_failed")
        .expect("the attempt failed");
    assert_eq!(failed["code"], "plan_refused");

    let mut other = job.clone();
    other.tasks.swap(0, 1);
    let (outcome, _) = run(job, Some(planned(&other)), &kinds);
    assert!(
        outcome
            .diagnostic
            .expect("why")
            .contains("this job is sha256:"),
        "another job's plan"
    );
}

/// PN-T24 (PN-I12): the build numbers the kernel stages in the order the plan starts them, so
/// the scheduler's starting order is the plan's; a plan that starts them otherwise numbers them
/// otherwise.
#[test]
fn pn_t24_the_stages_follow_the_plan() {
    let source = entry(json!({"kind": "parquet", "url": "/in.parquet"}));
    let sink = |u: &str| entry(json!({"kind": "parquet", "url": u}));
    // `src -> k1 -> k2 -> o1` and `src -> k3 -> o2`: in document order the kernels are k1, k2,
    // k3; by level, k1 and k3, then k2.
    let job = Job::new(
        vec![
            Task::source("src", source),
            Task::kernel("k1", pass()),
            Task::kernel("k2", pass()),
            Task::kernel("k3", pass()),
            Task::sink("o1", sink("/o1")),
            Task::sink("o2", sink("/o2")),
        ],
        vec![
            Edge::between("src", "k1"),
            Edge::between("k1", "k2"),
            Edge::between("k2", "o1"),
            Edge::between("src", "k3"),
            Edge::between("k3", "o2"),
        ],
    );
    let kinds = kinds(Path::new("/nowhere.parquet"));
    let plan = planned(&job);
    let built = build(&job, &plan, &kinds, opts()).expect("builds");
    assert_eq!(built.parts.stages, vec![vec![1], vec![3], vec![2]]);

    // A plan of the same job that starts k3 first.
    let mut reordered = plan.clone();
    let k1 = reordered
        .steps
        .iter()
        .position(|s| s.tasks == ["k1"])
        .expect("k1");
    let k3 = reordered
        .steps
        .iter()
        .position(|s| s.tasks == ["k3"])
        .expect("k3");
    reordered.steps.swap(k1, k3);
    let built = build(&job, &reordered, &kinds, opts()).expect("builds");
    assert_eq!(built.parts.stages, vec![vec![3], vec![1], vec![2]]);
}

/// PN-T27 (PN-I5): the build fuses two standard kernels into one stage, and does not when a
/// barrier stands between them: a barrier task, or a barrier edge.
#[test]
fn pn_t27_nothing_is_fused_across_a_barrier() {
    let cast =
        entry(json!({"kind": "std", "name": "cast", "args": {"columns": {"value": "double"}}}));
    let select = entry(json!({"kind": "std", "name": "select", "args": {"columns": ["value"]}}));
    let job = || {
        Job::new(
            vec![
                Task::source(
                    "src",
                    entry(json!({"kind": "parquet", "url": "/in.parquet"})),
                ),
                Task::kernel("a", cast.clone()),
                Task::kernel("b", select.clone()),
                Task::sink("out", entry(json!({"kind": "parquet", "url": "/out"}))),
            ],
            vec![
                Edge::between("src", "a"),
                Edge::between("a", "b"),
                Edge::between("b", "out"),
            ],
        )
    };
    let kinds = Registry::standard();
    let plain = job();
    let built = build(&plain, &planned(&plain), &kinds, opts()).expect("builds");
    assert_eq!(built.parts.stages, vec![vec![1, 2]], "fused");
    let mut task = job();
    task.tasks[1].barrier = true;
    let built = build(&task, &planned(&task), &kinds, opts()).expect("builds");
    assert_eq!(built.parts.stages, vec![vec![1], vec![2]]);
    let mut edge = job();
    edge.edges[1].barrier = true;
    let built = build(&edge, &planned(&edge), &kinds, opts()).expect("builds");
    assert_eq!(built.parts.stages, vec![vec![1], vec![2]]);
}

/// PN-T25 (PN-I13): `moruna explain` prints the plan's JSON, writes the plan document with
/// `--out`, refuses an unknown planner and a refused document by exit 2; `moruna run --plan`
/// runs the document from that plan, and refuses bytes that are not a plan by the field `plan`.
#[test]
fn pn_t25_explain_and_run_from_the_plan_it_wrote() {
    let _serial = one_run_at_a_time();
    let scratch = Scratch::new("pn_t25");
    let dir = scratch.path();
    let input = dir.join("in.parquet");
    write_parquet(&input, 2_000, 2);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).expect("out");
    let doc = json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": format!("file://{}", input.display())},
        "kernels": [{"kind": "std", "name": "select", "args": {"columns": ["id"]}}],
        "sink": {"kind": "parquet", "url": format!("file://{}", out.display())},
        "budget": {"memory_bytes": 1u64 << 30, "cpu": 2.0},
        "staging": {"dir": dir.join("staging").to_string_lossy(), "limit_bytes": 1u64 << 30},
        "profiles_dir": dir.join("profiles").to_string_lossy(),
        "report": {"file": dir.join("report.json").to_string_lossy()},
    });
    let spec = dir.join("job.json");
    std::fs::write(&spec, doc.to_string()).expect("the document");
    let kinds = Registry::standard();
    let plan_file = dir.join("job.mplan");
    let (mut printed, mut said) = (Vec::new(), Vec::new());
    let code = moruna_runtime::host::cli::explain(
        &spec,
        None,
        Some(&plan_file),
        &kinds,
        &mut printed,
        &mut said,
    );
    assert_eq!(code, exit::COMPLETED, "{}", String::from_utf8_lossy(&said));
    let json: Value = serde_json::from_slice(&printed).expect("the plan's JSON");
    assert_eq!(json["moruna_plan"], 1);
    assert_eq!(json["steps"].as_array().map(Vec::len), Some(3));
    let written = Plan::from_flatbuffer(&std::fs::read(&plan_file).expect("written"))
        .expect("a plan document");
    assert_eq!(written.id, json["id"].as_str().expect("an id"));

    let (mut printed, mut said) = (Vec::new(), Vec::new());
    let code = moruna_runtime::host::cli::explain(
        &spec,
        Some("no_such_planner"),
        None,
        &kinds,
        &mut printed,
        &mut said,
    );
    assert_eq!(code, exit::SPEC_REFUSED);
    assert!(String::from_utf8_lossy(&said).contains("no planner `no_such_planner`"));
    let bad = dir.join("bad.json");
    let mut refused = doc.clone();
    refused["kernels"][0]["kind"] = json!("no.such");
    std::fs::write(&bad, refused.to_string()).expect("written");
    let (mut printed, mut said) = (Vec::new(), Vec::new());
    assert_eq!(
        moruna_runtime::host::cli::explain(&bad, None, None, &kinds, &mut printed, &mut said),
        exit::SPEC_REFUSED
    );
    assert!(String::from_utf8_lossy(&said).contains("no.such"));
    let (mut printed, mut said) = (Vec::new(), Vec::new());
    assert_eq!(
        moruna_runtime::host::cli::explain(
            &dir.join("absent.json"),
            None,
            None,
            &kinds,
            &mut printed,
            &mut said
        ),
        exit::SPEC_REFUSED
    );

    let opts = HostOptions {
        cost: None,
        strict: false,
        env: &no_env,
        cancel: CancelToken::new(),
        heartbeat: HEARTBEAT,
        packages: Default::default(),
        capture_process: false,
        events: Default::default(),
        args: Vec::new(),
        pulled_images: None,
    };
    let outcome = session::run_file(&spec, Some(&plan_file), &kinds, &opts);
    assert_eq!(outcome.code, exit::COMPLETED, "{:?}", outcome.diagnostic);
    assert_eq!(outcome.plan.expect("the plan").plan, written.id);

    let outcome = session::run_file(&spec, Some(&spec), &kinds, &opts);
    assert_eq!(outcome.code, exit::SPEC_REFUSED);
    assert!(
        outcome
            .diagnostic
            .expect("why")
            .starts_with("spec refused: plan: not a plan document"),
    );
    let outcome = session::run_file(&spec, Some(&dir.join("absent.mplan")), &kinds, &opts);
    assert_eq!(outcome.code, exit::SPEC_REFUSED);

    assert_eq!(
        moruna_runtime::host::cli::parse(&[
            "run".to_string(),
            "job.json".to_string(),
            "--plan".to_string(),
            "job.mplan".to_string()
        ]),
        Ok(moruna_runtime::host::cli::Command::Run {
            spec: "job.json".into(),
            plan: Some("job.mplan".into()),
            strict: false,
            packages: Default::default(),
            events_json: false,
            args: Vec::new(),
        })
    );
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        moruna_runtime::host::cli::parse(&args(&[
            "explain",
            "job.json",
            "--planner",
            "levels",
            "--out",
            "p.mplan"
        ])),
        Ok(moruna_runtime::host::cli::Command::Explain {
            spec: "job.json".into(),
            planner: Some("levels".into()),
            out: Some("p.mplan".into()),
        })
    );
    for bad in [
        &["explain"][..],
        &["explain", "a", "b"][..],
        &["explain", "a", "--fast"][..],
        &["explain", "a", "--planner"][..],
        &["explain", "a", "--out"][..],
        &["run", "a", "--plan"][..],
        &["run", "--vm", "a", "--image", "/i", "--plan", "p"][..],
    ] {
        assert!(
            moruna_runtime::host::cli::parse(&args(bad)).is_err(),
            "{bad:?}"
        );
    }
}
