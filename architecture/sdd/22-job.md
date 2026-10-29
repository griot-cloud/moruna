# Moruna SDD 22: The job document and the catalogue, for embedders (`moruna-job`)

**Document type:** software design document, component 22
**Status:** DRAFT · 2026-10-28, amended 2026-10-29 by KA5 (the job document version 2, a DAG of tasks: d.4, e.4, e.5, JB-I10 to JB-I14, JB-T29 to JB-T39) (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** board items KA4 and KA5 (`BOARD.md`, wave B); SDD 18 (the kinds a run is made of); `architecture/moruna-hosted-engine.md` (MH) section 4.1; brief MC2 (`architecture/agents/briefs/waves-a-b-c.md` section 5)
**Preamble:** `00-preamble.md`; **Cited:** 18 (KD-I1, KD-I2, KD-I4, KD-I8, e.1 to e.3, decision KD1), 12 f.9, MH 4.1, MH 4.5
**Component location:** `crates/moruna-job/` (`src/kind.rs`, `src/doc.rs`, `src/job.rs`, `src/flatbuffer.rs`, `src/flex.rs`, `src/canonical.rs`, `src/catalogue.rs`, `src/check.rs`, `src/schema.rs`, `schema/job.fbs`, `src/generated/job_generated.rs` (generated, committed), `catalogue/standard.json`, `catalogue/peql.json`), `tools/flatc/generate.sh`, `crates/moruna-runtime/src/host/cli.rs` (`moruna check-job`), `crates/moruna-runtime/tests/job.rs` (the tests that need the registry)
**Consumes:** `serde`, `serde_json`, `schemars`, `thiserror`, `flatbuffers` and `flexbuffers` (25.12.19, the version arrow-ipc already brings), and nothing from Arrow, DataFusion or the runtime. **Consumed by:** `moruna-runtime` (it builds a run from the document and registers the kinds the declarations describe), `moruna-py`, and every embedder that reads Moruna's kinds or checks a job document without running it.

**Decisions worth your eye:** (1) an embedder that never runs a job, a service that authors and accepts jobs, depends on one small crate and gets the declaration of every kind, the job document and a checker; it does not compile Arrow; (2) the types have one home: `moruna-runtime` uses them from `moruna-job` and re-exports none; (3) the catalogue is two files `moruna kinds` printed, embedded, and a test in the runtime proves each file equals the registry's listing, so the files cannot drift from the code; (4) the checker reads the settings schema of each kind with a validator for exactly the subset of JSON Schema `schemars` writes for the settings types of this repository, and a test proves that validator and the kind give one verdict on seeded mutations of every example; (5) a schema outside the subset is refused when it is added to a catalogue, so the subset cannot silently grow.

---

## a. Purpose and boundary

SDD 18 made "a kind Moruna can run" a declaration an embedder reads (`moruna kinds`), and MH 4.1 made a run a document. Both lived in `moruna-runtime`, so an embedder that only needed to read the kinds, to draw a screen from a declaration, or to know before it dispatched whether a document names kinds Moruna has and settings those kinds accept, had to depend on the runtime and with it on Arrow, DataFusion, Parquet and the engines. This component moves the types an embedder reads into a crate of their own and adds what it does with them without running anything: the catalogue of every declaration it can name, and `check`.

It owns: the declaration types (`Role`, `Flow`, `Fact`, `Isolation`, `Needs`, `Declaration`, `Listing`), the settings schema format (`settings_schema`, `SCHEMA_DIALECT`, `Urls`), the job document (`JobSpec` and every type it contains, its parse and its two serialisations), the `Catalogue` and the two files that fill it, `check` and its refusals, and the validator for the settings schemas.

It refuses to know: how a kind is built or run (the runtime's), the registry (the runtime's: a kind is code there and a declaration here), a hash (the document's digest is `sha256` of `JobSpec::identity_json`, which the runtime computes), and any embedder.

## b. Vocabulary

**Catalogue.** Every declaration an embedder can name, by role and name: Moruna's from the embedded files, and the embedder's own added to them.

**Listing.** What `moruna kinds` prints (SDD 18 e.3); `Listing` is its type.

**Refusal.** One thing wrong with a document: the field, a code, a sentence.

**The subset.** The keywords of JSON Schema 2020-12 the validator reads (e.3).

**Verdict.** Whether a kind's settings are accepted: by the validator, by the kind's own type (`Entry::settings::<T>()`, SDD 18 KD-I4).

## c. Invariants

**JB-I1. The crate depends on six crates.** `serde`, `serde_json`, `schemars`, `thiserror`, `flatbuffers` and `flexbuffers`, in `[dependencies]`, and nothing else. In particular nothing from Arrow, DataFusion, the runtime or a hash. (Amended 2026-10-29, KA5: the document version 2 is FlatBuffers and its tasks' settings FlexBuffers; `flexbuffers` is used to write them and not to read them, JB-I11.)

**JB-I2. The types have one home.** `Role`, `Flow`, `Fact`, `Isolation`, `Needs`, `Declaration`, `Listing`, `Urls`, `JobSpec`, `KindDoc`, `SpecError`, `ContractEntry` and every `*Doc` type are defined in `moruna-job`. `moruna-runtime`, `moruna-py` and the embedders use them from there; no crate re-exports them under another path, and none defines a second.

**JB-I3. The embedded files are the registry's listing.** `catalogue/standard.json` is what `moruna kinds` lists for a build of `moruna-runtime` with default features, declaration for declaration; `catalogue/peql.json` is what a build with the `peql` feature lists beyond it. A runtime test builds `Registry::standard()`'s listing, reads it as a catalogue, and asserts it equals `Catalogue::standard()` (without `peql`) or `Catalogue::standard_with_peql()` (with it).

**JB-I4. A catalogue holds only declarations it accepts.** `Catalogue::add` takes a declaration when it passes `Declaration::check` for its role (SDD 18 KD-I2), no kind of that role has its name, and its settings schema is in the subset (JB-I7). Otherwise it refuses, naming the role and the name; the catalogue is unchanged.

**JB-I5. `check` collects every refusal.** It returns every refusal of a document, not the first, in the order the document is read: the source, each kernel, the sink, then the contracts, and within one entry's settings the keys in key order, so the list does not depend on whether `serde_json` keeps a map in the order it was written (its `preserve_order` feature, which any crate in a build may switch on). It builds nothing, runs nothing, reads no file and opens no connection.

**JB-I6. The validator and the kind give one verdict on settings.** For every declared kind, the validator refuses an entry's settings exactly when the kind cannot read them as the type its schema was derived from (SDD 18 KD-I4), with the two differences f.3 states: a value the schema leaves open and the kind judges after reading it, which the validator accepts; and an enum an adapter adds to its schema, which the validator refuses and the kind refuses by its own check. The runtime's JB-T26 proves it on every example and on seeded mutations of each.

**JB-I7. A schema outside the subset does not enter a catalogue.** A keyword the validator does not read, a `$ref` it cannot follow, or a keyword whose value is not the shape it needs, is refused by `Catalogue::add` at the place it is, so the subset cannot grow by a schema using what the validator would ignore.

**JB-I8. A refusal says where and what.** Its `field` names the part of the document at fault (`source.kind`, `kernels[2]`, `sink.settings.url`), its code the kind of fault, and its message one sentence that names the kind and the value and says what to change, in the voice of the runtime's refusals (SDD 18 KD-I1, KD-I4).

**JB-I9. The document the runtime builds is the document this crate reads.** `moruna-runtime`'s build takes a `moruna_job::Job`, a version 1 document as `Job::from_v1` reads it; a document `check` passes is a document the build reads. The build refuses more than `check` does: what a kind judges of a value (a kernel's arguments, a file that is not there, a pin) is not the schema's, nor is how many inputs a kernel takes (`Kernel::max_inputs`) or a flow other than batches.

**JB-I10. A version 1 document is a DAG with one path (KA5).** `Job::from_v1` reads `source`, `kernels` and `sink` as tasks in that order, each with the one port its role has (`in`, `out`), joined by an edge of batches from each to the next; the tasks' ids are `report.tasks`'s when the document names them (with their members), else `source`, `kernel-1` .. `kernel-n`, `sink`; every other field carries over. A refusal of one of its tasks names the field the document wrote it under (`source`, `kernels[i]`, `sink`); a version 2 task is `tasks[i]` (`Job::field`). Every version 1 document runs unchanged through the one build.

**JB-I11. Bytes from anyone are verified before they are read (KA5).** `Job::from_flatbuffer` checks the file identifier (`MJB2`), runs the FlatBuffers verifier (bounded depth, tables and size) and checks the version before it reads a field; a task's settings and a contract's document are read from FlexBuffers by this crate's own reader (`flex::decode`), which checks every offset, width, length and type and refuses what JSON cannot hold, because the `flexbuffers` crate's reader indexes without checking and panics on hostile bytes. Whatever the bytes are, the result is a job or a typed `JobError`, never a panic.

**JB-I12. The generated code is what the schema generates (KA5).** `src/generated/job_generated.rs` is the output of flatc 25.12.19 (the version of the `flatbuffers` crate in `Cargo.lock`) for `schema/job.fbs`, committed; `tools/flatc/generate.sh --check` regenerates it in a container and fails on a difference, and CI runs it on every pull request.

**JB-I13. A job is a DAG whose ports and edges fit (KA5).** `check` and `check_graph` refuse, each at its field: a task id that is not one or names two tasks; ports a role does not have (a source has output ports and no input port, a sink input ports and no output port, a kernel both) or a port named twice; an edge to a task or a port the job does not have; an edge whose flow is not what its producer gives or its consumer takes; an input port no edge feeds, an output port no edge reads; two edges into one input port (a task that takes several inputs has a port for each); a cycle; a barrier with nothing on one side; an edge that goes around a barrier (e.5).

**JB-I14. The identity is what the job does (KA5).** `Job::identity_json` is the canonical JSON rendering without `resume` and `report`, with each task named by its place (`#i`) rather than its id and without its members: a host that renames its tasks has the same job, as a version 1 document's `report.tasks` never changed its identity.

## d. Interfaces

### d.1 The crate

```rust
// the declaration (SDD 18 e.1 to e.3) and the document (MH 4.1), moved here unchanged
pub enum Role { Source, Kernel, Sink }
pub enum Flow { Batches, Files, Nothing }
pub enum Fact { Yes, No, BySettings }
pub enum Isolation { Builtin, Imported, Caller, Confined }
pub struct Needs { pub network: Fact, pub egress: Vec<String>, pub gpu: Fact, pub model_port: Fact, pub contracts: Fact }
pub struct Declaration { pub name: String, pub role: Role, pub summary: String, pub settings: Value,
                         pub inputs: Flow, pub outputs: Flow, pub needs: Needs,
                         pub deterministic: Fact, pub preserves_columns: Fact, pub isolation: Isolation }
impl Declaration { pub fn check(&self, role: Role) -> Result<(), String>; pub fn examples(&self) -> Vec<Value>; }
pub struct Listing { pub moruna_kinds: u32, pub moruna: String, pub sources: Vec<Declaration>, pub kernels: Vec<Declaration>, pub sinks: Vec<Declaration> }
pub fn settings_schema<T: JsonSchema>(name: &str, examples: Vec<Value>) -> Value;
pub fn is_kind_name(name: &str) -> bool;
pub struct Urls(pub Vec<String>);

pub struct JobSpec { /* MH 4.1, field for field */ }
impl JobSpec {
    pub fn new(source: KindDoc, kernels: Vec<KindDoc>, sink: KindDoc) -> JobSpec;
    pub fn from_json(text: &str) -> Result<JobSpec, SpecError>;
    pub fn from_value(value: Value) -> Result<JobSpec, SpecError>;
    pub fn to_json_pretty(&self) -> String;
    pub fn canonical_json(&self) -> String;
    pub fn identity_json(&self) -> String;          // the canonical form without `resume` and `report`
}
pub struct KindDoc { pub kind: String, pub settings: Map<String, Value> }
pub struct SpecError { pub field: String, pub reason: String }   // "spec refused: <field>: <reason>"
// ContractEntry, BudgetDoc, ElasticDoc, StagingDoc, ObjectStoreDoc, S3Doc, GcsDoc, AzureDoc,
// CheckpointDoc, ErrorPolicyDoc, SizerDoc, GuaranteeDoc, HostProfileDoc, ReportDoc,
// TaskIdsDoc, TaskRefDoc, is_task_id, SPEC_VERSION, CHECKPOINT_INTERVAL_MS, SCHEMA_DIALECT, LISTING_VERSION
```

What changed in moving: `Role::verb` and `Flow::as_str` are public, `Role` has `Display`, and `SpecError` is a `thiserror` error. `JobSpec::digest` is the runtime's `moruna_runtime::job::digest(&job)`, because a hash is not a dependency of this crate; it is `sha256` of `identity_json`. The conversion of a `SpecError` into the runtime's error is `moruna_runtime::job::refusal`, because the runtime's error type is not this crate's.

### d.2 The catalogue

```rust
pub struct Catalogue { /* declarations by role and name */ }
impl Catalogue {
    pub fn empty() -> Catalogue;
    pub fn standard() -> Result<Catalogue, CatalogueError>;            // catalogue/standard.json
    pub fn standard_with_peql() -> Result<Catalogue, CatalogueError>;              // standard.json and peql.json
    pub fn from_listing(listing: Listing) -> Result<Catalogue, CatalogueError>;
    pub fn add(&mut self, declaration: Declaration) -> Result<(), CatalogueError>;   // JB-I4
    pub fn get(&self, role: Role, name: &str) -> Option<&Declaration>;
    pub fn names(&self, role: Role) -> impl Iterator<Item = &str>;
    pub fn declarations(&self, role: Role) -> impl Iterator<Item = &Declaration>;
}
pub enum CatalogueError {
    Refused { role: Role, why: String },       // fails Declaration::check for the role
    Taken { role: Role, name: String },        // a kind of that role and name is declared
    Schema { role: Role, name: String, fault: SchemaFault },   // JB-I7
    Format { found: u32 },                     // a listing of another format
}
```

An embedder adds its own kinds with `add`, or builds a catalogue from the `Listing` of its own binary's `moruna kinds`. `from_listing` takes each of a listing's three lists as a kind of that list's role, so a declaration listed under another role is refused.

### d.3 Checking

```rust
pub fn check(job: &JobSpec, catalogue: &Catalogue) -> Result<(), Vec<Refusal>>;
pub struct Refusal { pub field: String, pub code: RefusalCode, pub message: String }
pub enum RefusalCode {
    UnknownKind { role: Role, known: Vec<String> },
    Settings { path: String },       // a JSON pointer into the entry's settings
    Flow,
    Contract,
}
```

and the command, in `moruna-runtime`:

```
moruna check-job <file.json>
```

reads the document, checks it against the catalogue of the kinds of the binary's own registry (`Catalogue::from_listing(registry.listing())`, which for the standard registry is `Catalogue::standard()`, JB-T25, and which includes the kinds a binary adds, such as the Python adapter's), and writes each refusal to standard error as `<field>: <message>`. It exits 0 and writes nothing when there is none, with the exit code of a refused document (2) when there is one or the file is not there or not a document, and 1 when the binary's kinds cannot be checked against (a kind whose schema is outside the subset). `moruna kinds` is unchanged.

### d.4 The job document version 2 (KA5, 2026-10-29)

```rust
pub const JOB_VERSION: u32 = 2;
pub const JOB_IDENTIFIER: &str = "MJB2";       // bytes 4 to 8 of every buffer
pub const IN: &str = "in";                      // the one input port of a task that has one
pub const OUT: &str = "out";                    // the one output port

pub struct Job {                                // the in-memory model
    pub id: Option<String>,                     // the run's identity, 32 lowercase hex
    pub tasks: Vec<Task>,
    pub edges: Vec<Edge>,
    pub budget: BudgetDoc, pub staging: StagingDoc, pub object_store: ObjectStoreDoc,
    pub checkpoint: CheckpointDoc, pub resume: Option<String>, pub error_policy: ErrorPolicyDoc,
    pub ordered: bool, pub sizer: SizerDoc, pub trace: Option<String>,
    pub profiles_dir: Option<String>, pub host_profile: Option<HostProfileDoc>,
    pub allow_gil: bool, pub report: Reporting, pub contracts: Vec<ContractEntry>,
}
pub struct Task {
    pub id: String,                             // stable; events and the report name it
    pub role: Role,                             // source, kernel or sink
    pub kind: String,                           // resolved by the registry for the role
    pub settings: Map<String, Value>,           // FlexBuffers on the wire, checked by the schema
    pub inputs: Vec<String>, pub outputs: Vec<String>,   // named ports, port 0 first
    pub barrier: bool,                          // an enforcement point
    pub members: Vec<String>,                   // the embedder's tasks it carries
    pub at: Option<String>,                     // where a version 1 task was written; not part of the job
}
pub struct Edge { pub from: PortRef, pub to: PortRef, pub flow: Flow, pub barrier: bool }
pub struct PortRef { pub task: String, pub port: String }
pub struct Reporting { pub socket, pub file, pub attempt, pub logs }   // report without `tasks`

impl Job {
    pub fn new(tasks: Vec<Task>, edges: Vec<Edge>) -> Job;
    pub fn from_v1(spec: &JobSpec) -> Job;                  // JB-I10
    pub fn from_flatbuffer(bytes: &[u8]) -> Result<Job, JobError>;   // JB-I11
    pub fn to_flatbuffer(&self) -> Result<Vec<u8>, JobError>;        // deterministic
    pub fn to_json(&self) -> String;                        // a rendering, never read back
    pub fn canonical_json(&self) -> String;
    pub fn identity_json(&self) -> String;                  // JB-I14
    pub fn field(&self, index: usize) -> String;            // `tasks[i]`, or a v1 task's field
    pub fn task_index, task, edges_into, edges_from, topological_order, descendants;
}
pub enum JobError { Identifier, Malformed(String), Version(u32), Field { field, reason } }
pub fn is_job_flatbuffer(bytes: &[u8]) -> bool;
pub fn check(job: &Job, catalogue: &Catalogue) -> Result<(), Vec<Refusal>>;
pub fn check_graph(job: &Job) -> Result<(), Vec<Refusal>>;  // the refusals that need no catalogue
pub mod fb;                                                 // the tables flatc generates
```

`check` takes a `Job`; a version 1 document is checked as `check(&Job::from_v1(&spec), ..)`, and `moruna check-job` reads either version (FlatBuffers when the bytes carry the identifier, JSON otherwise). `RefusalCode` gains `TaskId`, `UnknownPort`, `Unconnected`, `Crowded`, `Cycle` and `Barrier`; `Flow` covers an edge's flow too.

## e. Data model, formats and state machines

### e.1 The embedded files

`catalogue/standard.json` is `moruna kinds` as printed by a build with default features: the listing of SDD 18 e.3, indented. `catalogue/peql.json` is the same format and holds only what the `peql` feature adds, the `datafusion` source and the `peql` sink. Their `moruna` field is the version they were printed at; the catalogue does not carry it, so a release does not make a file stale, and a change to a declaration does (JB-T25). To regenerate after a change: print `moruna kinds` for a build with default features into `standard.json`, and for a build with `--features peql` keep the declarations it lists beyond `standard.json` in `peql.json`.

### e.2 The checks

`check` reads each entry in order, `source`, `kernels[i]`, `sink`:

1. **The kind.** The entry's `kind` is declared for the entry's role, else `UnknownKind` at `<field>.kind`: ``no <role> kind `<name>` is declared; this catalogue <reads|runs|writes> <names, sorted, comma-separated>`` (`this catalogue has no <role> kinds` when there are none). The rest of the entry is not read.
2. **The flows.** The declaration's flows fit the role: a source gives batches, a kernel takes and gives batches, a sink takes batches; else `Flow` at `<field>`. `Catalogue::add` admits only declarations that fit (JB-I4), so this refuses nothing a catalogue built through the API holds; it is read from the declaration so the check does not depend on how the catalogue was filled (JB-T11).
3. **The settings.** The entry's settings, as an object, are validated against the declaration's settings schema (e.3). Each violation is one `Settings` refusal at `<field>.settings.<path>` (`<field>.settings` for the settings as a whole), with the JSON pointer of the value at fault in `path`, and the message ``` `<kind>` settings: <sentence> ``` where the sentence names the value by its dotted path (``` `options.columns[1]` must be a string, not an integer ```, ``` `url` is required ```, ``` `bogus` is not a setting of `parquet` ```, ``` `name` must be one of "cast", "rename", ... ```, ``` `options.file_bytes` must be at least 0 ```).
4. **The contracts.** A kind whose `needs.contracts` is `yes` and a job whose `contracts` is empty: one `Contract` refusal per such entry, at `contracts`, naming the entry (``` `peql` at `sink` reads or writes under a contract, and the job carries none: put it in `contracts` ```). These come last, after every entry.

The reverse of 4, contracts carried by a job none of whose kinds needs them (SDD 18 KD-I8), is not a refusal of `check`: it is the build's.

### e.3 The validator

The validator reads the subset of JSON Schema 2020-12 that `schemars` writes for the settings types of this repository, and nothing else:

| Keyword | What it does |
|---|---|
| `type` | a name or a list of names (`null`, `boolean`, `integer`, `number`, `string`, `array`, `object`); an `integer` is a number written without a fraction or an exponent, as `serde` reads one |
| `properties`, `required` | each declared key checked by its schema; each required key present |
| `additionalProperties` | `false` refuses an undeclared key; `true` allows it; a schema checks it (a map's values) |
| `enum`, `const` | the value is one of the listed, or the one |
| `oneOf`, `anyOf`, `allOf` | exactly one, at least one, every one of the forms |
| `items`, `prefixItems`, `minItems`, `maxItems` | the schema of each item, of the leading items (a tuple), and the length |
| `$ref`, `$defs` | a reference to `#/$defs/<name>` of the schema's root, and the definitions |
| `minimum`, `maximum`, `minLength` | the bounds of a number, the length of a string in characters |
| `format` | an annotation, except the integer widths `schemars` writes (`int8` to `int64`, `uint8` to `uint64`, `int`, `uint`), which bound an integer to that width |
| `$schema`, `title`, `description`, `examples`, `default` | annotations; nothing is checked |

A schema may also be `true` or `false`. The set is what the standard kinds' schemas use (`$defs`, `$ref`, `$schema`, `additionalProperties`, `anyOf`, `default`, `description`, `enum`, `examples`, `format`, `items`, `maxItems`, `minItems`, `minimum`, `prefixItems`, `properties`, `required`, `title`, `type`), plus the keywords `schemars` writes for other shapes the settings may take (`oneOf`, `allOf`, `const`, `maximum`, `minLength`) so a kind that uses them does not leave the subset. `format` is read for integer widths because `schemars` writes `minimum: 0` for an unsigned type and no `maximum`, and `serde` refuses a value past the width; a validator that ignored `format` would accept it.

`Catalogue::add` runs `schema::admit` on the settings schema: any other keyword is `CatalogueError::Schema` naming the keyword and its JSON pointer; so is a `type` that names no type, a `required` that is not a list of strings, a `$ref` that is not `#/$defs/<name>` or names a definition the schema lacks, and any keyword whose value is not the shape it needs. A schema is checked against with `schema::validate(root, schema, value)`, which lists every violation with its path.

### e.4 The FlatBuffers schema (KA5)

`crates/moruna-job/schema/job.fbs`, namespace `moruna.job.fb`, file identifier `MJB2`, extension `mjob`, root `Job`:

| Table | Fields |
|---|---|
| `Job` | `version: uint` (2; written always, a reader refuses any other), `id`, `tasks: [Task]`, `edges: [Edge]`, `budget`, `staging`, `object_store`, `checkpoint`, `resume`, `error_policy`, `ordered`, `sizer`, `trace`, `profiles_dir`, `host_profile`, `allow_gil`, `report`, `contracts: [Contract]` |
| `Task` | `id` (required), `role: Role`, `kind` (required), `settings: [ubyte] (flexbuffer)` (a map), `inputs: [string]`, `outputs: [string]`, `barrier: bool`, `members: [string]` |
| `Edge` | `from: Endpoint` (required), `to: Endpoint` (required), `flow: Flow`, `barrier: bool` |
| `Endpoint` | `task`, `port` (both required) |
| `Budget`, `Elastic`, `Staging` | optional scalars (`= null`) where version 1 has `null` |
| `Checkpoint` | `enabled = true`, `interval_ms = 5000`, `keep` |
| `ErrorPolicy` | `kind: ErrorPolicyKind` (`Terminate`, `Skip`, `Budget`), `budget: uint` |
| `HostProfile` | each guarantee a `Guarantee` (`Unset`, `Present`, `Absent`), `staging_dir` |
| `Report` | `socket`, `file`, `attempt = null`, `logs` |
| `Contract` | `document: [ubyte] (flexbuffer, required)`, `compiled` (required), `functions: [ubyte] (flexbuffer)` (a list), `audiences: [string]` |

Enums: `Role` (`Source`, `Kernel`, `Sink`), `Flow` (`Batches`, `Files`, `Nothing`), `Sizer` (`Rule`, `Learned`). The Rust in `src/generated/job_generated.rs` is flatc 25.12.19's (JB-I12). `tools/flatc/generate.sh` builds flatc from the release's source (pinned by SHA-256) with g++ in `gcc:14-bookworm` (pinned by its index digest), keeps it under `target/flatc`, and runs it in the same image; nothing is installed on the machine. A change to the schema is a run of the script and a commit of its output.

JSON is a rendering of the document for people (`Job::to_json`, with `"moruna_job": 2`), and the canonical form the digest is taken over (JB-I14); nothing reads it back. A version 1 document stays JSON and is read by `JobSpec::from_json`.

### e.5 The graph (KA5)

A task's role decides its ports: a source has output ports and no input port, a sink input ports and no output port, a kernel both. An edge joins an output port to an input port; one output port may feed several input ports (fan-out), and a kernel may take several input ports (fan-in), port 0 first in the order its `inputs` list them, each fed by exactly one edge. Every output port is read by at least one edge and every input port fed by exactly one. The edges form no cycle; `Job::topological_order` orders the tasks so every edge goes forward, ties in document order (Kahn's algorithm).

A **barrier** is an enforcement point (a contract boundary, masking): nothing may be moved across it, and what crosses it passes through it. A barrier task needs something on both sides. For a barrier task `B`, an edge from a task upstream of `B` to a task downstream of `B`, neither being `B`, goes around it and is refused at the edge. For a barrier edge `a -> b`, any other edge from `a` or a task upstream of it to `b` or a task downstream of it goes around it. The planner (KA7) reads the marks; this crate only refuses a document that bypasses one.

## f. Algorithms and policies

**f.1 The verdict.** `validate` walks the value and the schema together. A `$ref` is followed and its siblings kept (2020-12 allows them). `type` is checked first, and a value of another type is one violation and nothing more at that place. `oneOf` and `anyOf` try each form with its own violations, keep none of them when one form matches, and report one violation naming what the forms allow when none does; `oneOf` reports one when more than one matches. Objects check `required`, then each key by `properties` or `additionalProperties`; arrays check the length, then each item by `prefixItems` then `items`. A chain of references deeper than 64 is one violation, not a loop.

**f.2 Why the verdict is the kind's.** A kind reads its settings with the type its schema was derived from (SDD 18 KD-I4, KD1), and `schemars` writes the constraints that type's `serde` reading has, so the validator and the kind refuse the same values; JB-T26 is the check that they do, not an argument that they must.

**f.3 Where a schema and its kind differ.** Three places, each known, none a disagreement about what the type reads.

- *Open values.* What a kind judges of a value after it has read it: a standard kernel's `args` for its `name`, a package file that is not there or has another fingerprint, a contract a governed kind names and the job does not carry, a filter's operator. The schema leaves them open (`args` is any JSON, SDD 18 e.2); `check` accepts and the build refuses, at the field it names.
- *An enum the adapter adds.* The `std` kind lists the standard kernel names as an `enum` on `name` (SDD 18 e.2), a constraint its type, a `String`, cannot say. The validator refuses a name outside the list; the build refuses it too, when the kernel is made, so the verdicts agree on the document and differ only in where the build says it.
- *`serde` reads a struct from an array.* A struct whose settings are written as a JSON array is read field by field in order, which no schema says and no document means; the validator refuses it as the schema says and the kind accepts it. JB-T26 does not make the one change that exposes it, an object made an array.

## g. Concurrency within the component

None. A `Catalogue` is a value; `check` takes it by reference and has no state.

## h. Behaviour

A document with no kernels is a copy from source to sink and passes with two entries checked. A kind spelled `null` in a setting is a setting left out (MH 4.1), before the check sees it. An empty catalogue refuses every entry as an unknown kind. A document that is not valid (not JSON, a version this build does not read, a section that is not what MH 4.1 says) is refused by `JobSpec::from_json`, as a `SpecError`, before there is a document to check.

## i. Configuration

None. The catalogue is two embedded files and what the embedder adds; the refusals' limit on the values an enum lists in a sentence (12, then how many more) is a constant of the message, not a setting.

## j. Observability

`moruna check-job` (d.3). A refusal is a value with its code and its field, so a service that rejects a job can put each on the screen that holds the field.

## k. Tests

Test names are the ids' lowercase with a slug (`jb_t6_every_example_passes`).

**JB-T1 dependencies_are_the_four.** The manifest's dependency tables name `schemars`, `serde`, `serde_json` and `thiserror` and nothing else. JB-I1.

**JB-T2 embedded_catalogues.** The standard catalogue has the sources `parquet`, `tensor`, `vortex`, the kernels `package`, `std` and the sinks `arrow_ipc`, `parquet`, `tensor`, `vortex`, by role in name order; with `peql` it has `datafusion` and `peql` besides; each declaration is one the registry accepts and has examples; a name of another role is not found; the governed kinds need contracts. JB-I3, JB-I4.

**JB-T3 add_refusals.** A kind of the embedder's own is added beside the standard ones; a duplicate of a role and name is refused naming both; a name of another role is not a duplicate; a declaration that fails `Declaration::check` is refused with its reason. JB-I4.

**JB-T4 schema_subset_is_closed.** A schema with `pattern`, `uniqueItems`, `if`, `$dynamicRef` or `patternProperties`, a malformed keyword value, or a `$ref` to anywhere but `#/$defs/<name>` is refused at `add` naming the keyword and its pointer, and the catalogue is unchanged; every keyword of e.3 is accepted. JB-I7.

**JB-T5 from_listing.** A listing of the format this crate reads is a catalogue of what it lists; another format, a duplicate and a declaration listed under another role are refused.

**JB-T6 every_example_passes.** Every example of every declared kind, in a document around it, passes `check`. JB-I5.

**JB-T7 unknown_kinds.** A source, kernel and sink of kinds the catalogue has for another role or not at all are refused at `<field>.kind` with the kinds of the role, sorted, with the code carrying them; an empty catalogue says it has none.

**JB-T8 settings_refusals.** A missing required setting, an undeclared one, one of another type, one in a nested object or list, a tuple of the wrong length, a number below its bound, one of another type than an integer, an enum value not in the list: each is one refusal at its field, with its pointer and its sentence; the widest `u64` is allowed; an open setting (`args`) allows anything. JB-I8.

**JB-T9 every_refusal_is_collected.** A document with a bad source, three bad kernels and a bad sink returns every refusal, in document order. JB-I5.

**JB-T10 contracts_are_needed.** A document that carries no contract and names the `datafusion` source and the `peql` sink gets one `Contract` refusal for each, at `contracts`, naming the entry; carried, it passes; a document whose kinds need none is not refused for carrying none. KD-I8's other half is the build's.

**JB-T11 flows_are_read_from_the_declaration.** A catalogue that holds a declaration whose flows do not fit its role (put in without `add`) gets a `Flow` refusal at each entry of it.

**JB-T12 to JB-T17 the validator** (`tests/schema.rs`). JB-T12 types (a name or a list, an integer is written without a fraction, the names of a value's type); JB-T13 objects (`properties`, `required`, `additionalProperties` as `false`, `true` and a schema, the path of each violation); JB-T14 arrays (`items`, `prefixItems`, `minItems`, `maxItems`); JB-T15 values (`enum`, `const`, `minimum`, `maximum`, `minLength`, every integer width of `format` at and past its edge, other formats not checked); JB-T16 combinators and references (`allOf`, `anyOf`, `oneOf` with none and with several matching, what the forms were in the sentence, `$ref` through `$defs`, nested, with sibling keywords and with an escaped name); JB-T17 admit (a keyword outside the subset at its pointer, a schema that is not a schema, a reference that never ends).

**JB-T18 declaration_check, JB-T19 settings_schema_is_derived, JB-T20 listing_reads_back.** The moved types: a declaration is one the registry accepts for its role and reads back as itself, with each refusal of SDD 18 KD-I2 naming why; a settings schema is derived from the type, titled with the kind, with its examples, and `Urls` is a string or a list; a listing reads back as itself.

**JB-T21 document_round_trip, JB-T22 document_refusals, JB-T23 report_tasks, JB-T24 section_defaults.** The moved document: every section of MH 4.1 parses, serialises and parses back, the canonical form is a fixed point and the identity is blind to `resume` and `report`; a refusal names its field and its text is `spec refused: <field>: <reason>`; `report.tasks` names the chain as it is; what a document leaves out is the default. The tests of the same document that need a build are HO-T1 to HO-T4 and HO-T9 (`moruna-runtime`, `job/tests.rs`).

**JB-T25 catalogue_is_the_registry.** `Registry::standard().listing()` read as a catalogue equals `Catalogue::standard()` (default features) or `Catalogue::standard_with_peql()` (`peql`), and lists the same names by role. JB-I3.

**JB-T26 validator_agrees_with_the_runtime.** For every declared kind and each of its examples, the example is accepted by the validator and by the kind, and each of 200 seeded mutations of it (SplitMix64 from a seed of the kind's name and the example's place; a key dropped, an unknown key added, a value changed to a value of another type, a number put out of range, a string put outside the enum; one to three at a time) is accepted by both or refused by both. The kind's verdict is whether building its part from the entry got past reading its settings: a refusal other than `` `<kind>` settings: `` is the kind's later judgement (f.3). The two verdicts must be: built and no refusal; refused at the settings and a refusal; refused later and no refusal (an open value); or refused later and every refusal an enum (`must be one of`). Any other pair fails the test naming the kind and the settings. Every kind's mutations reach both verdicts. JB-I6.

**JB-T27 the_types_have_one_home.** No source of `moruna-runtime` re-exports `moruna_job` or declares a type `moruna-job` owns. JB-I2.

**JB-T28 check_job_command.** `moruna check-job` exits 0 and writes nothing for a document its binary's kinds accept; for another it writes each refusal as `<field>: <message>` and exits 2; a file that is not there, not JSON, or of another version is refused the same way; it takes one document; a binary with a kind whose schema is outside the subset exits 1 saying so. d.3.

**JB-T29 job_round_trip** (`tests/dag.rs`). A job with every section set survives `to_flatbuffer` and `from_flatbuffer`; the bytes carry the identifier and are the same every time; a job of defaults reads back with `checkpoint.enabled` on; the JSON rendering names every task, port, barrier and edge; the identity is blind to `resume` and `report` and to nothing else. JB-I11, JB-I14.

**JB-T30 verifier_refuses_corrupted_bytes.** 4,000 seeded corruptions of a whole document (one to four bytes set, and truncations) each read as a job or a `JobError`, never a panic, and most are refused; bytes without the identifier, a buffer shorter than one, a document of another version and settings that are a FlexBuffers list are refused by what they are. With `flex::tests::hostile_buffers_never_panic` (20,000 corruptions of a FlexBuffers value through `flex::decode`). JB-I11.

**JB-T31 v1_reads_as_one_path.** A version 1 document reads as its source, kernels and sink with the ids version 1 gave them (or `report.tasks`'s, members included), one port each, an edge of batches from each to the next, every other field as it was; it passes `check`. JB-I10.

**JB-T32 dags_pass.** A fan-out, a fan-in of two sources and a diamond pass `check`. e.5.

**JB-T33 a_cycle_is_refused.** A cycle is refused at the first task on it, naming the tasks on it and not those downstream; a task feeding itself is a cycle. JB-I13.

**JB-T34 port_refusals.** An edge to a port or a task the job does not have, an unfed input, an unread output, two edges into one input, a port named twice and ports a role does not have, each at its field. JB-I13.

**JB-T35 flows_that_do_not_fit.** An edge whose flow is not what its producer gives is refused at the edge. JB-I13.

**JB-T36 barriers.** A barrier task with nothing upstream or downstream is refused; an edge around a barrier task, and one around a barrier edge, is refused at the edge naming the barrier; edges through it pass. JB-I13, e.5.

**JB-T37 task_ids.** A task id that is not one, one that names two tasks and a member that is not a task id are refused at the task. JB-I13.

**JB-T38 the_build_refuses_to_go_around_a_barrier** (`moruna-runtime`, `tests/dag.rs`). The runtime's build refuses a document whose edge goes around a barrier before any kind is built, naming the edge; the document through the barrier builds. JB-I9, JB-I13.

**JB-T39 every_table_at_its_defaults** (`tests/dag.rs`). A version 2 document whose every table is present and holds only its schema defaults, with the strings the schema requires, is read or refused at its field and never panics; the verified document's rendering names every table, for a diagnosis; every enum value this build writes has its schema name, and an unknown one renders as unknown. JB-I11, JB-I12.

## l. Implementation notes for the agent

A new kind adds nothing here: its declaration is in the runtime, and the catalogue files are regenerated from `moruna kinds` (e.1); JB-T25 fails until they are. A kind whose settings type needs a JSON Schema keyword outside the subset adds the keyword to `schema.rs` (`VALIDATING`, `walk`, `admit_at`), to the table in e.3, and to JB-T12 to JB-T17, and JB-T26 proves the validator and the kind still agree. Do not add a JSON Schema crate: the subset is closed on purpose (JB-I7).

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| KA4 (an embedder depends on one small crate) | JB-I1, JB-I2 | JB-T1, JB-T27 |
| SDD 18 e.1 to e.3 (the declaration, the schema, the listing) | JB-I3, JB-I4 | JB-T2, JB-T3, JB-T5, JB-T18 to JB-T20, JB-T25 |
| SDD 18 KD1, KD-I4 (the schema is the type's) | JB-I6, JB-I7 | JB-T4, JB-T12 to JB-T17, JB-T26 |
| SDD 18 KD-I1 (an unknown kind is refused with the kinds known) | JB-I5, JB-I8 | JB-T7, JB-T9 |
| SDD 18 KD-I8 (contracts are carried for a kind that needs them) | JB-I5 | JB-T10 |
| MH 4.1 (the document) | JB-I9 | JB-T21 to JB-T24 |
| SDD 18 d.3 (the command line) | JB-I5, JB-I8 | JB-T28 |
| KA5 (the job is a DAG, in FlatBuffers) | JB-I10 to JB-I14 | JB-T29 to JB-T39 |

## o. Deferred (post-v1)

**JB-O1. Retain, cost hints, partitioning and order (MC2).** MC2 names a per-edge `retain` (MB1 widened), per-task cost hints, a task's partitioning and whether its output order matters. Version 2 carries none of them yet: each is a field added to `Task` or `Edge` when the item that reads it is built (MB1, KD6, KA7).

**JB-O2. Cost and placement.** A check that says what a document would cost, or whether a host could run it, is MC3's; `check` says only that the document names kinds Moruna has with settings they accept.
