//! The job document, version 2 (SDD 22 e.4): a run as a DAG of tasks.
//!
//! [`Job`] is the in-memory model. Its form on the wire is FlatBuffers (`schema/job.fbs`,
//! [`Job::from_flatbuffer`], [`Job::to_flatbuffer`]); its JSON ([`Job::to_json`]) is a rendering
//! for people and its canonical JSON ([`Job::identity_json`]) what the runtime's digest is taken
//! over. A version 1 document, the linear chain of MH 4.1, is read as a DAG with one path
//! ([`Job::from_v1`]), so every version 1 document runs unchanged.
//!
//! A task is a kind of a role with its settings and its named ports: a source has output ports,
//! a sink input ports, a kernel both. An edge joins an output port to an input port and carries
//! the flow its producer declares. One output port may feed many input ports (fan-out); a kernel
//! may take several input ports (fan-in), each fed by exactly one edge. A task or an edge marked
//! as a barrier is an enforcement point: nothing may be moved across it and no edge goes around
//! it (SDD 22 e.5).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::doc::{
    BudgetDoc, CheckpointDoc, ContractEntry, ErrorPolicyDoc, HostProfileDoc, JobSpec, KindDoc,
    ObjectStoreDoc, SizerDoc, StagingDoc, TaskRefDoc,
};
use crate::kind::{Flow, Role};

/// The version of the document this build writes and reads as a DAG.
pub const JOB_VERSION: u32 = 2;

/// The name of a task's one input port when it has one: what a version 1 document's kernels and
/// sink take their input on.
pub const IN: &str = "in";

/// The name of a task's one output port when it has one.
pub const OUT: &str = "out";

/// One run, as a DAG of tasks (SDD 22 e.4).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Job {
    /// The run's identity, 32 lowercase hex characters; absent: minted at start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The tasks, in the order the document lists them.
    pub tasks: Vec<Task>,
    /// The edges, in the order the document lists them.
    pub edges: Vec<Edge>,
    /// Memory and CPU.
    pub budget: BudgetDoc,
    /// The disk tier.
    pub staging: StagingDoc,
    /// Credentials and endpoints for object URLs.
    pub object_store: ObjectStoreDoc,
    /// The manifest cadence.
    pub checkpoint: CheckpointDoc,
    /// `"auto"`, a run id or a manifest path; absent: a fresh run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// What happens after a kernel error.
    pub error_policy: ErrorPolicyDoc,
    /// Deliver morsels to each sink in sequence order.
    pub ordered: bool,
    /// Which decision function sizes morsels.
    pub sizer: SizerDoc,
    /// Where the trace file goes; absent keeps the trace in memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// The profile store; absent is the preamble's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles_dir: Option<String>,
    /// The platform's declared guarantees; absent: discovery probes them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_profile: Option<HostProfileDoc>,
    /// Run Python kernels serialised under a GIL interpreter rather than refusing.
    pub allow_gil: bool,
    /// Where the report goes.
    pub report: Reporting,
    /// The contracts the job's kinds read and write under (MH 4.5).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<ContractEntry>,
}

/// One task: a kind of a role, its settings and its named ports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Task {
    /// Its stable id, which events and the report name it by ([`crate::is_task_id`]).
    pub id: String,
    /// Which part of a run it is.
    pub role: Role,
    /// The kind's name, resolved by the registry of kinds for the role.
    pub kind: String,
    /// The kind's settings, `null`s left out.
    pub settings: Map<String, Value>,
    /// Its input ports by name, port 0 first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Its output ports by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// An enforcement point: nothing is moved across it and no edge goes around it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub barrier: bool,
    /// The embedder's tasks this one carries, when the embedder fused several into it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Where the task is in the document it was read from, when that is not `tasks[i]`: a
    /// version 1 document's `source`, `kernels[i]` and `sink`, which is what a refusal names so
    /// it points at what the author wrote. Not part of the job: neither written nor rendered.
    #[serde(skip)]
    pub at: Option<String>,
}

impl Task {
    /// A source of kind `kind` with one output port, [`OUT`].
    pub fn source(id: impl Into<String>, entry: KindDoc) -> Task {
        Task::of(id, Role::Source, entry, Vec::new(), vec![OUT.to_string()])
    }

    /// A kernel of kind `kind` with one input port, [`IN`], and one output port, [`OUT`].
    pub fn kernel(id: impl Into<String>, entry: KindDoc) -> Task {
        Task::of(
            id,
            Role::Kernel,
            entry,
            vec![IN.to_string()],
            vec![OUT.to_string()],
        )
    }

    /// A kernel with the input ports `inputs`, in port order, and one output port, [`OUT`].
    pub fn kernel_with_inputs(id: impl Into<String>, entry: KindDoc, inputs: &[&str]) -> Task {
        Task::of(
            id,
            Role::Kernel,
            entry,
            inputs.iter().map(|p| p.to_string()).collect(),
            vec![OUT.to_string()],
        )
    }

    /// A sink of kind `kind` with one input port, [`IN`].
    pub fn sink(id: impl Into<String>, entry: KindDoc) -> Task {
        Task::of(id, Role::Sink, entry, vec![IN.to_string()], Vec::new())
    }

    fn of(
        id: impl Into<String>,
        role: Role,
        entry: KindDoc,
        inputs: Vec<String>,
        outputs: Vec<String>,
    ) -> Task {
        Task {
            id: id.into(),
            role,
            kind: entry.kind,
            settings: entry.settings,
            inputs,
            outputs,
            barrier: false,
            members: Vec::new(),
            at: None,
        }
    }

    /// The task as the entry its kind is built from: its kind and its settings.
    pub fn entry(&self) -> KindDoc {
        KindDoc {
            kind: self.kind.clone(),
            settings: self.settings.clone(),
        }
    }

    /// Mark the task an enforcement point.
    pub fn barrier(mut self) -> Task {
        self.barrier = true;
        self
    }
}

/// One end of an edge: a task and one of its ports.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct PortRef {
    /// The task's id.
    pub task: String,
    /// The port's name.
    pub port: String,
}

impl PortRef {
    /// Port `port` of task `task`.
    pub fn new(task: impl Into<String>, port: impl Into<String>) -> PortRef {
        PortRef {
            task: task.into(),
            port: port.into(),
        }
    }
}

impl core::fmt::Display for PortRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.task, self.port)
    }
}

/// An edge: from a task's output port to a task's input port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Edge {
    /// The producer's output port.
    pub from: PortRef,
    /// The consumer's input port.
    pub to: PortRef,
    /// The batch-flow contract the producer declares.
    pub flow: Flow,
    /// An enforcement point between the two tasks.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub barrier: bool,
}

impl Edge {
    /// Batches from `from_task`'s `from_port` to `to_task`'s `to_port`.
    pub fn new(
        from_task: impl Into<String>,
        from_port: impl Into<String>,
        to_task: impl Into<String>,
        to_port: impl Into<String>,
    ) -> Edge {
        Edge {
            from: PortRef::new(from_task, from_port),
            to: PortRef::new(to_task, to_port),
            flow: Flow::Batches,
            barrier: false,
        }
    }

    /// Batches from `from`'s one output port to `to`'s one input port.
    pub fn between(from: impl Into<String>, to: impl Into<String>) -> Edge {
        Edge::new(from, OUT, to, IN)
    }

    /// Mark the edge an enforcement point.
    pub fn barrier(mut self) -> Edge {
        self.barrier = true;
        self
    }
}

/// `report` of version 2: where the report goes. The tasks name themselves, so there is no
/// `tasks` here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Reporting {
    /// A peer to connect to and speak the protocol of MH 4.3 with; `unix://` or `vsock://`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Where the report file goes; `<run_id>` is replaced by the run's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Which attempt of its job this process is; absent: 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// The output store; `<run_id>` replaced by the run's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<String>,
}

impl Reporting {
    /// The attempt the document names; 1 when it names none.
    pub fn attempt(&self) -> u32 {
        self.attempt.unwrap_or(1)
    }
}

/// The source's id when a version 1 document names none.
pub const V1_SOURCE: &str = "source";
/// The sink's id when a version 1 document names none.
pub const V1_SINK: &str = "sink";

impl Job {
    /// A job of `tasks` and `edges` with every other field absent or at its default.
    pub fn new(tasks: Vec<Task>, edges: Vec<Edge>) -> Job {
        Job {
            id: None,
            tasks,
            edges,
            budget: BudgetDoc::default(),
            staging: StagingDoc::default(),
            object_store: ObjectStoreDoc::default(),
            checkpoint: CheckpointDoc::default(),
            resume: None,
            error_policy: ErrorPolicyDoc::default(),
            ordered: false,
            sizer: SizerDoc::default(),
            trace: None,
            profiles_dir: None,
            host_profile: None,
            allow_gil: false,
            report: Reporting::default(),
            contracts: Vec::new(),
        }
    }

    /// A version 1 document as a DAG with one path: its source, its kernels in stage order and
    /// its sink, each with one input port [`IN`] and one output port [`OUT`] where its role has
    /// them, joined by an edge of batches from each to the next. The tasks' ids are the
    /// document's `report.tasks` (with their members) when it names them, otherwise `source`,
    /// `kernel-1` .. `kernel-n` and `sink`, the ids version 1 gave them; every other field
    /// carries over as it is.
    pub fn from_v1(spec: &JobSpec) -> Job {
        let one = |id: String| TaskRefDoc {
            id,
            members: Vec::new(),
        };
        let (source, kernels, sink) = match &spec.report.tasks {
            Some(tasks) => (
                tasks.source.clone(),
                tasks.kernels.clone(),
                tasks.sink.clone(),
            ),
            None => (
                one(V1_SOURCE.into()),
                (1..=spec.kernels.len())
                    .map(|k| one(format!("kernel-{k}")))
                    .collect(),
                one(V1_SINK.into()),
            ),
        };
        let named = |task: Task, named: &TaskRefDoc, at: String| Task {
            members: named.members.clone(),
            at: Some(at),
            ..task
        };
        let mut tasks = Vec::with_capacity(spec.kernels.len() + 2);
        tasks.push(named(
            Task::source(source.id.clone(), spec.source.clone()),
            &source,
            "source".into(),
        ));
        for (i, (entry, named_as)) in spec.kernels.iter().zip(&kernels).enumerate() {
            tasks.push(named(
                Task::kernel(named_as.id.clone(), entry.clone()),
                named_as,
                format!("kernels[{i}]"),
            ));
        }
        tasks.push(named(
            Task::sink(sink.id.clone(), spec.sink.clone()),
            &sink,
            "sink".into(),
        ));
        let edges = tasks
            .windows(2)
            .map(|pair| Edge::between(pair[0].id.clone(), pair[1].id.clone()))
            .collect();
        Job {
            id: spec.run_id.clone(),
            tasks,
            edges,
            budget: spec.budget.clone(),
            staging: spec.staging.clone(),
            object_store: spec.object_store.clone(),
            checkpoint: spec.checkpoint.clone(),
            resume: spec.resume.clone(),
            error_policy: spec.error_policy,
            ordered: spec.ordered,
            sizer: spec.sizer,
            trace: spec.trace.clone(),
            profiles_dir: spec.profiles_dir.clone(),
            host_profile: spec.host_profile.clone(),
            allow_gil: spec.allow_gil,
            report: Reporting {
                socket: spec.report.socket.clone(),
                file: spec.report.file.clone(),
                attempt: spec.report.attempt,
                logs: spec.report.logs.clone(),
            },
            contracts: spec.contracts.clone(),
        }
    }

    /// The job as indented JSON, for a person to read. JSON is a rendering of the document, not
    /// a form of it: nothing reads it back.
    pub fn to_json(&self) -> String {
        let mut value = self.rendered();
        if let Value::Object(map) = &mut value {
            map.insert("moruna_job".into(), Value::from(JOB_VERSION));
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// The canonical form: the rendering with its keys sorted, no whitespace, absent fields
    /// omitted.
    pub fn canonical_json(&self) -> String {
        crate::canonical::to_string(&self.rendered())
    }

    /// The canonical form of what the job is: without `resume` and `report`, which say how this
    /// process continues and where it reports, and with each task named by its place rather than
    /// its id and without its members, which name it for the events and are not what it does. A
    /// host that renames its tasks has the same job. The runtime's digest of a job is the
    /// `sha256` of this text.
    pub fn identity_json(&self) -> String {
        let mut identity = self.clone();
        identity.resume = None;
        identity.report = Reporting::default();
        let place = |id: &str| match self.task_index(id) {
            Some(at) => format!("#{at}"),
            None => id.to_string(),
        };
        for edge in &mut identity.edges {
            edge.from.task = place(&edge.from.task);
            edge.to.task = place(&edge.to.task);
        }
        for (at, task) in identity.tasks.iter_mut().enumerate() {
            task.id = format!("#{at}");
            task.members.clear();
        }
        identity.canonical_json()
    }

    fn rendered(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|e| Value::String(e.to_string()))
    }

    /// Where task `index` is in the document, for a refusal: `tasks[i]`, or for a task read
    /// from a version 1 document its `source`, `kernels[i]` or `sink`.
    pub fn field(&self, index: usize) -> String {
        self.tasks
            .get(index)
            .and_then(|t| t.at.clone())
            .unwrap_or_else(|| format!("tasks[{index}]"))
    }

    /// The index of the task with id `id`, the first when several have it.
    pub fn task_index(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    /// The task with id `id`.
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// The edges into port `port` of task `task`, by index.
    pub fn edges_into(&self, task: &str, port: &str) -> Vec<usize> {
        (0..self.edges.len())
            .filter(|&i| self.edges[i].to.task == task && self.edges[i].to.port == port)
            .collect()
    }

    /// The edges out of port `port` of task `task`, by index, in document order.
    pub fn edges_from(&self, task: &str, port: &str) -> Vec<usize> {
        (0..self.edges.len())
            .filter(|&i| self.edges[i].from.task == task && self.edges[i].from.port == port)
            .collect()
    }

    /// The tasks in an order in which every edge goes forward: each task after every task with
    /// an edge into it, ties in document order (Kahn's algorithm). `Err` with the indices of the
    /// tasks on a cycle, in document order, when there is one. Edges naming no task are not
    /// followed.
    pub fn topological_order(&self) -> Result<Vec<usize>, Vec<usize>> {
        let n = self.tasks.len();
        let mut indegree = vec![0usize; n];
        let mut next: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &self.edges {
            if let (Some(a), Some(b)) = (
                self.task_index(&edge.from.task),
                self.task_index(&edge.to.task),
            ) {
                next[a].push(b);
                indegree[b] += 1;
            }
        }
        let mut order = Vec::with_capacity(n);
        let mut ready: std::collections::BTreeSet<usize> =
            (0..n).filter(|&i| indegree[i] == 0).collect();
        while let Some(i) = ready.pop_first() {
            order.push(i);
            for &j in &next[i] {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    ready.insert(j);
                }
            }
        }
        if order.len() == n {
            return Ok(order);
        }
        // What is left holds every cycle, and the tasks downstream of one. A task is on a cycle
        // when it reaches itself.
        let left: Vec<usize> = (0..n).filter(|&i| indegree[i] > 0).collect();
        let on_cycle: Vec<usize> = left
            .iter()
            .copied()
            .filter(|&i| self.reaches_through(&next, i, i))
            .collect();
        Err(on_cycle)
    }

    /// Whether a path of one or more edges leads from task `from` to task `to`.
    fn reaches_through(&self, next: &[Vec<usize>], from: usize, to: usize) -> bool {
        let mut seen = vec![false; self.tasks.len()];
        let mut stack: Vec<usize> = next[from].clone();
        while let Some(i) = stack.pop() {
            if i == to {
                return true;
            }
            if !std::mem::replace(&mut seen[i], true) {
                stack.extend(next[i].iter().copied());
            }
        }
        false
    }

    /// For each task, by index, the tasks a path of one or more edges leads to from it.
    pub fn descendants(&self) -> Vec<std::collections::BTreeSet<usize>> {
        let n = self.tasks.len();
        let mut next: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &self.edges {
            if let (Some(a), Some(b)) = (
                self.task_index(&edge.from.task),
                self.task_index(&edge.to.task),
            ) {
                next[a].push(b);
            }
        }
        (0..n)
            .map(|start| {
                let mut seen = std::collections::BTreeSet::new();
                let mut stack: Vec<usize> = next[start].clone();
                while let Some(i) = stack.pop() {
                    if seen.insert(i) {
                        stack.extend(next[i].iter().copied());
                    }
                }
                seen
            })
            .collect()
    }
}
