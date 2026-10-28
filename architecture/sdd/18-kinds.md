# Moruna SDD 18: The kinds a run is made of (`moruna-runtime::kinds`)

**Document type:** software design document, component 18
**Status:** DRAFT · 2026-10-28 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/agents/briefs/waves-a-b-c.md` section 1 law 2, section 4 MB3 and MB4, section 5 MC1 and MC2; decisions AP1, FB1, KD1 (`DECISIONS.md`); `architecture/moruna-hosted-engine.md` (MH) section 4.1
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.7 (`Kernel`, `Fingerprint`), `Source`, `Sink`; **Cited:** 12 f.3 (the sink equals source rule), 16 PK-I8 and f.8, 17 (the packaged runtimes), MH 4.5 (the governed ends), MH 4.9 (standard kernels and fusion), MH 4.10 (ports)
**Component location:** `crates/moruna-runtime/src/kinds/` (the ports, the registry, the suites, and one module per technology: `parquet.rs`, `vortex.rs`, `tensor.rs`, `arrow_ipc.rs`, `governed.rs`, `standard.rs`, `package.rs`, `ports.rs`), `crates/moruna-job/src/kind.rs` (the declaration, its role, flows and facts, the settings schema format and the listing, which an embedder reads without the runtime: SDD 22), `crates/moruna-runtime/src/job/build.rs` (the build that resolves a document through the registry), `crates/moruna-py/src/kinds.rs` (the Python kinds), `crates/moruna-runtime/src/host/cli.rs` (`moruna kinds`)
**Consumes:** `moruna-job` (22: the declaration, the listing and the document), contracts (1), sources (7), sinks (8), the standard kernels (`moruna-kernels`), the package and its runtimes (16, 17), the peQL bridge (`moruna-datafusion`, feature `peql`). **Consumed by:** the job document's build (12, MH 4.1), the command line (MH 4.2), the Python surface (12), every embedder that reads `moruna kinds`, MC2 (the job spec v2) and MC3 (the planner).

**Decisions worth your eye:** (1) a document names every part of a run by a string, and a registry the running program assembles resolves it; the closed enums at the boundary are gone, and the document format names no kind; (2) three ports, one per role (source, runnable kind, sink), one declaration shape for all three, one registry; (3) a kind's settings schema is JSON Schema 2020-12 derived from the type its adapter reads its settings with, so the published schema and the check cannot drift (decision KD1); (4) `rust` is not a kind this build registers, so a v1 `rust` entry is refused as any unregistered kind is, and an embedder that compiles a kernel into its binary registers it as a kind of its own.

---

## a. Purpose and boundary

What Moruna can run will keep growing: files of several formats, governed plans, standard kernels, packages, and next containers, VMs, API calls, binaries and Rust (MB3, MB4). Before this component the job document named those through three closed enums, so every new kind was an edit to the document's types, to the builder's `match`, and to every caller that built documents. This component makes "a kind Moruna can run" a port: each kind is one adapter, the only code that knows its technology, registered under its name with a declaration of what it is; a document names a kind by string; the registry resolves it or refuses with the kinds it knows.

It owns: the three ports; the registry, its resolution and its refusals; the conformance suite of each port; the standard adapters (the sources and sinks of 7 and 8, the governed ends of MH 4.5, the standard kernels, packages); the settings schema format; `moruna kinds`; how a document's parts are built through the registry (f.2).

The declaration (`Declaration`, `Role`, `Flow`, `Fact`, `Isolation`, `Needs`), the listing (`Listing`), the settings schema format (`settings_schema`, `SCHEMA_DIALECT`) and the job document (`JobSpec`, `KindDoc`) are `moruna-job`'s, so an embedder that only reads the kinds or checks a document depends on that crate and not on the runtime (SDD 22); this component uses them from there.

It refuses to know: what a kind does with its settings beyond reading them (each adapter's), how a source reads or a sink writes (7, 8), how a package runs (17), and any embedder: a kind is described in Moruna's terms and an embedder draws its screens from the declaration.

## b. Vocabulary

**Kind.** A named thing a run can be made of, in one role: a source kind, a runnable kind (a stage of the chain), a sink kind. One name may be a kind in several roles (`parquet` is a source and a sink; `python` is a source, a kernel and a sink in the library).

**Port.** The trait a kind's adapter implements for its role: `SourceKind`, `RunnableKind`, `SinkKind` (d.2).

**Adapter.** A value implementing a port: `kinds::parquet::Parquet`, `kinds::standard::Std`, `moruna_py::kinds::Imported`.

**Declaration.** What a kind says it is (e.1).

**Entry.** One part of a document: `source`, `kernels[i]` or `sink`, a `KindDoc` of a `kind` and its settings.

**Settings.** The keys of an entry other than `kind`.

**Registry.** The kinds a program can run, by role and name, and the program's hooks (`Embedding`).

**Suite.** A port's conformance suite (f.4).

## c. Invariants

**KD-I1. A document names a kind; the registry resolves it or refuses with the kinds it knows.** Every `kind` of a document is resolved before anything is built. A name no adapter of that role is registered under is refused at `<field>.kind` with the text ``no <role> kind `<name>` is registered; this build <reads|runs|writes> <names, sorted, comma-separated>`` (`this build has no <role> kinds` when there are none). Brief law 2, MC1.

**KD-I2. The registry holds only declarations it accepts.** A kind is registered only when its declaration has a kind name (1 to 64 of lowercase ASCII letters, digits, `_`, `-` and `.`, starting with a letter), the role of the port it is registered through, a summary, flows its role can have (a source gives batches; a kernel takes and gives batches; a sink takes batches), and a settings schema in the dialect of KD1 that describes a closed object (`additionalProperties: false`) with at least one example and no property `kind`; and only when no kind of that role and name is registered already. Otherwise registration is refused, `Config { name: "kinds" }`, naming why.

**KD-I3. Nothing outside the registry and the adapters matches over kinds.** The build reads declarations (KD-I6, KD-I8) and calls ports; no code outside `kinds/` and the adapters branches on a kind's name. Brief law 2.

**KD-I4. A kind's settings are read by the type its schema is derived from.** An adapter reads its settings with `Entry::settings::<T>()`, where `T` is the type its declaration's schema was derived from (e.2), so a setting the schema does not declare, of a type it does not allow, or required and missing, is refused naming the entry. A setting spelled `null` is a setting left out.

**KD-I5. A new kind touches no existing code.** Adding a kind is a new adapter and a call to register it. It is accepted when it passes its port's suite (f.4). KD-T5 proves it with a kind written in a test.

**KD-I6. A hosted run loads no code by import path.** Under strict mode an entry whose kind declares `isolation: imported` is refused at `<field>.kind`, whatever its role, before anything is built. 16 PK-I8.

**KD-I7. `rust` is not registered.** The standard registry registers no `rust` kind, so a v1 entry `{"kind": "rust", "crate", "symbol"}` is refused by KD-I1 as ``kernels[i].kind: no kernel kind `rust` is registered; this build runs package, std``, strict or not. A Rust kernel compiled into the program running a document is a kind that program registers (d.1). MB4 builds `rust` as an adapter of the runnable-kind port, isolated like the other packaged kernels (decision FP1).

**KD-I8. Contracts are carried only for a kind that needs them.** A document that carries `contracts` and none of whose kinds declares `needs.contracts: yes` is refused at `contracts`.

**KD-I9. v1 documents read and run unchanged.** Every `moruna_spec: 1` document 0.5.7 read is read by the same parser into entries of its kinds, unchanged, and its kinds resolve through the registry: `parquet`, `vortex`, `tensor`, `arrow_ipc` (2026-10-28), `datafusion` (with `peql`) sources; `parquet`, `vortex`, `tensor`, `arrow_ipc`, `peql` (with `peql`) sinks; `std`, `package`, and in a program with the Python adapter, `python` kernels; in the library, `iterator` and `python` sources and the `python` sink. What each setting means is what it meant.

**KD-I10. One stage per fused run of stages, by the kinds' own word.** Adjacent stages are joined into one when the earlier stage's `Fuse` returns a stage for the later one's (MH 4.9); the build never inspects a kernel to decide it, and `Built::stage_kernels` says which entries each stage carries.

## d. Interfaces

### d.1 The registry and the program's hooks

```rust
pub struct Registry { /* sources, kernels, sinks by name; the program's Embedding */ }
impl Registry {
    pub fn empty() -> Registry;
    pub fn standard() -> Registry;                          // f.1
    pub fn with_embedding(self, embedding: Arc<dyn Embedding>) -> Registry;
    pub fn embedding(&self) -> Arc<dyn Embedding>;
    pub fn register_source(&mut self, kind: Arc<dyn SourceKind>) -> Result<()>;   // KD-I2
    pub fn register_kernel(&mut self, kind: Arc<dyn RunnableKind>) -> Result<()>;
    pub fn register_sink(&mut self, kind: Arc<dyn SinkKind>) -> Result<()>;
    pub fn source(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn SourceKind)>;   // KD-I1
    pub fn kernel(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn RunnableKind)>;
    pub fn sink(&self, field: &str, name: &str) -> Result<(&Declaration, &dyn SinkKind)>;
    pub fn listing(&self) -> Listing;                       // e.3
}

/// What the program running a document supplies besides its kinds; every method defaults to a
/// program that supplies nothing (`Standalone`).
pub trait Embedding: Send + Sync {
    #[cfg(feature = "peql")]
    fn bindings(&self, root: &Path) -> Result<Option<Arc<dyn BindingResolver>>>;   // MH 4.5
    fn admit(&self, index: usize, contract: &ContractEntry) -> Result<()>;          // MH 4.5
    fn guest_port(&self, name: &str) -> Option<Arc<dyn GuestPort>>;                 // MH 4.10
}

pub fn build(job: &JobSpec, kinds: &Registry, opts: BuildOptions<'_>) -> Result<Built>;   // f.2
```

`moruna_runtime::host::cli::main(args, &Registry, cancel)`, `session::run_file` and `session::serve` take the registry the binary was built with. The `KernelLoader` trait, `NoKernels`, `LoadedKernel`, `CompiledKernel`, `spec::KernelEntry` and `spec::build_kernels` are gone: what a loader loaded is a registered kind, and what it answered is `Embedding`.

### d.2 The ports

```rust
pub trait SourceKind: Send + Sync {
    fn declaration(&self) -> Declaration;
    fn source(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSource>;
}
pub trait RunnableKind: Send + Sync {
    fn declaration(&self) -> Declaration;
    fn stage(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<Stage>;
}
pub trait SinkKind: Send + Sync {
    fn declaration(&self) -> Declaration;
    fn sink(&self, entry: &Entry<'_>, cx: &mut Context) -> Result<BuiltSink>;
}

pub struct Entry<'d>;      // field(), at(name), index(), doc(), settings::<T>(), refuse(name, reason)
pub struct Context;        // notes(), carried_contracts(), package_host(), host_port_hub(),
                           // embedding(), source_targets(), shared::<T>(make)
pub struct BuiltSource { pub spec: SourceSpec, pub targets: Vec<String>, pub memory: Option<Arc<dyn EngineMemory>> }
pub struct BuiltSink   { pub spec: SinkSpec, pub target: String, pub memory: Option<Arc<dyn EngineMemory>> }
pub struct Stage {
    pub kernel: Arc<dyn Kernel>,
    pub fuse: Option<Arc<dyn Fuse>>,                       // KD-I10
    pub packaged: Option<Packaged>,                        // bound to the run's gate (16, 17)
    pub placed: Option<Box<dyn FnOnce(StageId) + Send>>,   // the stage's id once the chain is settled
    #[cfg(feature = "python")] pub python: Option<Arc<PyKernel>>,
}
pub trait Fuse: Send + Sync { fn fuse(&self, next: &dyn Fuse) -> Option<Stage>; fn as_any(&self) -> &dyn Any; }
pub fn check_pin(entry: &Entry<'_>, pinned: Option<&str>, kernel: &dyn Kernel) -> Result<()>;
```

`Context::shared::<T>` is the build's one value of type `T`: the `datafusion` source and the `peql` sink share one engine per root through it (MH 4.5). `Context::source_targets` is where the built source reads, for a sink that keeps something apart from it (the `peql` sink's `excluded_url`).

### d.3 The command

```
moruna kinds
```

prints the registry's listing (e.3) as indented JSON and exits 0; any argument is refused (exit 2). The `moruna` binary with the Python adapter lists the Python kinds beside the runtime's.

### d.4 The document (MH 4.1)

`JobSpec.source` and `JobSpec.sink` are `KindDoc`s and `JobSpec.kernels` a list of them (`moruna_job`): `{ "kind": "<name>", <settings> }`. The parser checks only that an entry is an object with a non-empty string `kind`; what its settings may be is the kind's, checked when the document is built (KD-I4). `KindDoc::of(kind, &settings)` writes an entry from a kind's typed settings.

### d.5 Consumed

`schemars` 1.2.2 (derive), for the settings schema (e.2): an addition to the preamble 6.2 table, already in the graph through `parcel-core` at that version.

## e. Data model, formats and state machines

### e.1 The declaration

```jsonc
{
  "name": "parquet",                 // KD-I2
  "role": "source",                  // source | kernel | sink
  "summary": "Parquet files or prefixes, local or in an object store",
  "settings": { /* e.2 */ },
  "inputs": "files",                 // batches | files | nothing
  "outputs": "batches",
  "needs": {
    "network": "by_settings",        // yes | no | by_settings: connections out, always through the reactor's clients
    "egress": [],                    // destinations needed whatever the settings
    "gpu": "no",
    "model_port": "no",              // a named port it calls (MH 4.10)
    "contracts": "no"                // the job's carried contracts (KD-I8)
  },
  "deterministic": "yes",            // the same input and settings give the same output rows
  "preserves_columns": "by_settings",// for a source: the stored columns as stored; for a sink: every column written
  "isolation": "builtin"             // builtin | imported | caller | confined
}
```

Isolation: `builtin`, code compiled into the program, in its process; `imported`, code imported by path into the process (what KD-I6 refuses); `caller`, an object the library caller handed over, never named by a document from a file or a socket; `confined`, code from a package run apart from the process (17, decision FP1).

| Kind | Role | Inputs, outputs | Network | Contracts | Deterministic | Preserves columns | Isolation |
|---|---|---|---|---|---|---|---|
| `arrow_ipc` | source | files, batches | no | no | yes | by settings | builtin |
| `parquet` | source | files, batches | by settings | no | yes | by settings | builtin |
| `vortex` | source | files, batches | by settings | no | yes | by settings | builtin |
| `tensor` | source | files, batches | no | no | yes | by settings | builtin |
| `datafusion` | source | files, batches | no | yes | yes | by settings | builtin |
| `std` | kernel | batches, batches | no | no | yes | by settings | builtin |
| `package` | kernel | batches, batches | no (model port by settings) | no | by settings | by settings | confined |
| `parquet`, `vortex` | sink | batches, files | by settings | no | yes | yes | builtin |
| `tensor`, `arrow_ipc` | sink | batches, files | no | no | yes | yes | builtin |
| `peql` | sink | batches, files | no | yes | yes | yes | builtin |
| `python` (command) | kernel | batches, batches | no (GPU by settings) | no | no | no | imported |
| `python` (library) | kernel | batches, batches | no (GPU by settings) | no | no | no | caller |
| `iterator`, `python` (library) | source | nothing, batches | no | no | no | no | caller |
| `python` (library) | sink | batches, nothing | no | no | no | no | caller |

### e.2 The settings schema (decision KD1)

JSON Schema, dialect 2020-12 (`"$schema": "https://json-schema.org/draft/2020-12/schema"`), of the entry's keys other than `kind`: a closed object (`additionalProperties: false`) with `title` the kind's name, `properties` with each setting's type and description, `required`, nested types under `$defs`, and `examples`, each an object of settings that builds. It is derived (`moruna_job::settings_schema::<T>`) from the type the adapter reads its settings with (KD-I4), so a schema cannot describe settings the adapter does not read; an adapter may add constraints the type cannot say (the `std` kind lists the standard kernel names as `enum`). Values that are open by nature are described as such: a standard kernel's or a package's `args` (any JSON), a peQL `caller` (an object), a filter's value (a number, a string or a boolean), a `url` that is one string or several.

In the job spec v2 (MC2, FlatBuffers, decision FB1), a node is a kind name and its settings, and the settings are a value of this same data model carried in the spec (FlexBuffers, the FlatBuffers project's schemaless encoding of it, is the natural carrier), checked against this schema; the JSON entry of d.4 is its rendering for people. A FlatBuffers union of per-kind tables is not used: it is a closed enum, and adding a kind would change the spec's schema (KD-I5).

### e.3 The listing

```jsonc
{ "moruna_kinds": 1, "moruna": "0.5.7",
  "sources": [ /* e.1, by name */ ], "kernels": [ ... ], "sinks": [ ... ] }
```

`Listing` (`moruna_job`) serialises and deserialises as itself, so an embedder in Rust reads it back, and `moruna_job::Catalogue` is the declarations of one, which the embedder checks a document against (SDD 22). The standard kinds' listing is embedded in `moruna-job` and a runtime test proves it equal to this registry's (SDD 22 JB-I3).

## f. Algorithms and policies

**f.1 The standard registry.** `Registry::standard()` registers: sources `arrow_ipc`, `parquet`, `tensor`, `vortex`, and with `peql` `datafusion`; kernels `package`, `std`; sinks `arrow_ipc`, `parquet`, `tensor`, `vortex`, and with `peql` `peql`. A build without `peql` has no governed kinds, so a document naming them is refused by KD-I1 (the stub that refused them by text is gone). The Python surface adds its kinds (`crates/moruna-py/src/kinds.rs`): the command registers `python` (`Imported`, which imports the module an entry names); `moruna.run` registers `python` (`Given`, which hands back the kernels it was given by the entry's place), `iterator`, the `python` source and the `python` sink (the objects it holds). A program that registers none of them refuses documents naming them by KD-I1.

**f.2 The build.** In order, each step's refusal before the next is tried: the environment (MH 4.1); the run id; every kind resolved (KD-I1: source, sink, then each kernel); strict mode over the declarations (KD-I6); the contracts over the declarations (KD-I8); a `Context` made with the program's hooks, the package host, the host ports and the carried contracts; the source built; its targets recorded on the context; the sink built; the sink equals source rule (12 f.3); each kernel's stage built and joined with the previous one when their fuses say so (KD-I10); the kinds' notes appended to the build's; then every stage given its id (`placed`), its packaged kernel and its Python kernel collected; then the ranges, resume and the rest of MH 4.1, unchanged.

**f.3 A pin.** A runnable kind that reads a `fingerprint` calls `check_pin` with the kernel it built: hex, optionally prefixed `algo:`, compared lower-cased with `kernel.fingerprint().to_hex()`, refused at `<field>.fingerprint` naming both. The package kind also passes the pin to the package's reader, which hashes the file before anything is built (16 PK-I2).

**f.4 The conformance suites.** `kinds::conformance::{source, kernel, sink}(kind, &Harness)` run one port's suite over the declaration's examples, or over the examples a `Harness` gives for a kind whose examples need a fixture (a package file, carried contracts), each case in a fresh context the harness makes. They return the properties checked and the cases built, or the first `Violation` by property id:

- **KD-C1** the declaration is one the registry accepts for the port's role (KD-I2);
- **KD-C2** it is equal every time it is asked, and its JSON reads back as itself;
- **KD-C3** every example builds, and its entry reads back as itself;
- **KD-C4** an example with a setting the schema does not declare is refused at the entry or a field under it;
- **KD-C5** for each declared setting whose schema allows only some JSON types (resolving `$ref`, `anyOf` and `oneOf`; `null` aside), an example with a value of another type is refused at the entry or under it;
- **KD-C6** an example without each setting the schema requires is refused at the entry or under it;
- **KD-C7** (runnable kinds) when `fingerprint` is a setting, each example with a pin that is not its kernel's is refused at `<entry>.fingerprint`.

A suite reads nothing but the port and the declaration, so it holds for a kind that does not exist yet (MB3's), and it was written before the ports' second adapters were moved onto it.

**f.5 How `rust` is refused.** By KD-I1, through the registry, as any kind no adapter is registered for (KD-I7). There is no reserved word, no special case and no match arm for it.

## g. Concurrency within the component

Ports are `Send + Sync`; a registry is built once by the program and read by every build. A build's `Context` is its own; `shared` values are per build. The library's handed-object kinds take their object once, under a mutex.

## h. Behaviour

A document with no kernels builds a copy from source to sink. A kind whose stage is refused refuses the build at that entry; nothing later is built. A kind's notes are reported after the environment's and before the ranges' (f.2).

## i. Configuration

None. The registry is code the program assembles; the settings are the document's.

## j. Observability

`moruna kinds` (d.3). A refusal names the field and, for an unknown kind, the kinds the build has.

## k. Tests

**KD-T1 resolve_or_refuse.** Each standard name resolves to its role's adapter; a name of another role, an unknown name and a name in an empty registry are refused at `<field>.kind` with KD-I1's text. KD-I1.

**KD-T2 rust_is_not_registered.** `kernels[0]` of kind `rust` is refused by the standard registry with KD-I7's exact text. KD-I7. The same, through `build`, strict and not, is in KD-T9.

**KD-T3 registration_refusals.** A duplicate, a name that is not a kind name (uppercase, a leading digit, 65 characters), a role other than the port's, an empty summary, flows the role cannot have, another dialect, a schema that is not an object, an open schema, a property `kind`, no examples or a non-object example: each refused at registration naming why; a second `parquet` source or sink, or `std`, is refused. KD-I2.

**KD-T4 standard_conformance.** Every standard source and sink and `std` passes its port's suite over its declared examples (`kinds/tests.rs`); `package` passes over a real `.wasm` (`tests/package.rs`), `datafusion` and `peql` over carried contracts (`kinds/governed.rs`, feature `peql`); the Python kinds' declarations are ones the registry accepts (`crates/moruna-py/src/kinds.rs`). KD-I5.

**KD-T5 add_a_kind.** A kind written in the test, `test.scale`, passes the runnable-kind suite, is refused by the standard registry, registers beside the standard kinds, is listed, and a strict document naming it runs end to end and writes every value scaled (`tests/kinds.rs`). KD-I5, KD-I3.

**KD-T6 listing.** The standard listing names every kind by role in name order, reads back as itself, each declaration is one the registry accepts and has examples, the `std` schema lists the standard kernel names; `moruna kinds` exits 0 and refuses an argument. e.3, d.3.

**KD-T7 strict_refuses_imported.** The MH 4.1 example, whose kernel is of an imported kind, is refused in strict mode at `kernels[0].kind`, and builds without it; a library caller's kinds (`caller`) build in strict mode. KD-I6.

**KD-T8 contracts_need_a_kind.** Contracts carried with `parquet` ends are refused at `contracts`. KD-I8.

**KD-T9 v1_documents_resolve.** v1 documents over every source and sink kind, `std` and `python` kernels, read back from their canonical form and build; `rust` is refused, strict and not; a `package` entry resolves and is refused only because its file is not there. KD-I9, KD-I7.

**KD-T10 suite_catches_violations.** Adapters that break KD-C1, KD-C2, KD-C3, KD-C4 (accepting an undeclared setting, or refusing it at another field), KD-C6 and KD-C7 each fail their suite on that property; a harness's examples replace the declared ones. f.4.

**KD-T11 fusion_through_the_port.** Two stages of a kind whose fuse joins its own become one, as two fusable standard kernels do, and no stage joins a neighbour of another kind; `stage_kernels` says which entries each stage carries. KD-I10.

The tests that were the builder's keep their ids and now run through the registry: HO-T1 to HO-T4, HO-T9 (`job/tests.rs`), PK-T11 (as KD-T7), PK-T12, PO-T6, PY-T23 to PY-T27, HO-T19.

## l. Implementation notes for the agent

A new kind: write its adapter in a module of its own (or its own crate), derive its settings type with `serde` and `schemars` (`deny_unknown_fields`), build its declaration with `moruna_job::settings_schema::<T>(name, examples)`, register it, and run its port's suite in its tests. Change nothing else. A kind that calls ports reads `ports: Option<BTreeMap<String, PortDoc>>` and builds its routes with `kinds::ports::routes`, binding them in `Stage::placed`.

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| Brief law 2 (a document names a kind; refusal lists the known) | KD-I1, KD-I3 | KD-T1, KD-T2 |
| Brief law 2 (one suite per port, written once) | KD-I2, KD-I5, f.4 | KD-T3, KD-T4, KD-T10 |
| Brief law 2 (adding a kind touches no existing code) | KD-I5 | KD-T5 |
| MC1 (the declaration, `moruna kinds`) | e.1, e.2, e.3 | KD-T6 |
| MC1 (every existing kind moved onto the registry) | f.1, KD-I9 | KD-T4, KD-T9 |
| MB4 (`rust` reserved and refused) | KD-I7, f.5 | KD-T2, KD-T9 |
| 16 PK-I8 (no import path in a hosted run) | KD-I6 | KD-T7 |
| MH 4.5 (contracts carried for the governed ends) | KD-I8 | KD-T8 |
| MH 4.9 (fusion) | KD-I10 | KD-T11 |

## o. Deferred (post-v1)

**KD-O1. Settings as FlexBuffers.** MC2 carries a node's settings in the FlatBuffers spec and checks them against e.2's schema; the encoding (FlexBuffers) and its verifier are MC2's.

**KD-O2. Costs in the declaration.** The empirical cost model (decision CM1) profiles a kind when it is checked; the declaration may then carry the profile's key. MC3.
