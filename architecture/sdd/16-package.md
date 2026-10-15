# Moruna SDD 16: The function package (`moruna-package`, `python/moruna/_package.py`)

**Document type:** software design document, component 16
**Status:** DRAFT · 2026-10-15 (becomes HANDOFF-READY when section m is empty and the preamble's E1 and E2 assumptions are accepted; the human flips it)
**Parent:** `architecture/function-packages.md` (FP) sections 2, 3, 5, 6; `architecture/moruna-hosted-engine.md` (MH) section 4.9, 4.1
**Preamble:** `00-preamble.md`; **Contracts:** `01-contracts.md` d.7 (`Kernel`, `Fingerprint`), `moruna_kernel::declare` (MH 4.9)
**Component location:** `crates/moruna-package` (Rust: the manifest, the reader, the fingerprint, the tree, the wasm custom section), `python/moruna/_package.py` (Python: `moruna package` for a project), `crates/moruna-runtime/src/job` (the `package` kind), `crates/moruna-runtime/src/check/package.rs` (a package through its runtime), `crates/moruna-py/src/package.rs` (the module's half of `check`, `aot`, `load`, `package --wasm`)
**Consumes:** contracts (1), the runtimes of SDD 17 (`moruna-wasm`, `moruna-sandbox`), the check harness (`moruna_runtime::check`, MH 4.9). **Consumed by:** the job document (MH 4.1), the Python surface (12), SDD 17.

**Decisions worth your eye:** (1) a package is one file and its fingerprint is `sha256` of that file's bytes, read once: the bytes that are hashed are the bytes that are run, so nothing can change between the check and the load; (2) the manifest is embedded in the file (a zip entry of a `.pex`, a custom section of a `.wasm`) and a `.wasm` also answers `declare()`, and the two must agree; (3) `moruna package` resolves wheels with `pip download` for the named interpreter and platform only, with `--require-hashes` and `--no-deps`, so the lockfile is the whole environment and the packaging machine's own platform never enters it; (4) a package's runtime is chosen by its extension and nothing else, and a hosted run (strict mode) loads no kernel by import path at all.

---

## a. Purpose and boundary

A kernel that is checkable (MH 4.9) is still a Python module whose dependencies are the importing interpreter's, or a Rust crate compiled into the engine. This component makes a kernel one portable file: the code, its dependencies and its declaration, with a fingerprint that is the file. It reads such a file, verifies it, extracts what a runtime needs, and lets a job document, `moruna check`, `moruna aot` and `moruna load` name it.

It owns: the two formats and their byte layouts; `moruna.json`; the fingerprint; the `moruna:kernel` world's text and the verification of a component against it; the extracted tree of a `.pex` and its marker; `moruna package` for a Python project and for a wasm component; `moruna check <package>`, `moruna aot <package>` and `moruna load <package>` as commands; the `package` kind of the job document and its pin; the strict-mode refusal of a kernel named by import path.

It refuses to know: how a package is run (SDD 17), where a host keeps packages or how they reach a disk (FP 5), and any build system: `moruna package --wasm` verifies a component, it compiles nothing.

## b. Vocabulary

**Package.** One file, `<name>.pex` or `<name>.wasm`, holding one kernel. Its kind is its extension.

**Manifest.** `moruna.json` (e.1), embedded in the package.

**Declaration.** The manifest's `declaration`: the kernel's `input_schema`, `output_schema` and `hints`, spelled as the decorator reads them (MH 4.9, e.2).

**Fingerprint.** `sha256` of the package file's bytes, written `sha256:<64 lowercase hex>`. For a packaged kernel it replaces the source-and-lockfile hash of MH 4.9 step 7.

**Project.** A directory with `function.py`, `function.yaml` (e.3), any sibling files, and optionally the lockfile `function.yaml` names.

**Interpreter tag, platform tag, target.** The wheel tags of PEP 425: `cp314` and `manylinux_2_28_x86_64`; a target is the pair, written `cp314-manylinux_2_28_x86_64`.

**Tree.** A `.pex` extracted to a directory, with its bytecode, beside the package or in a private directory (f.6).

**World.** `moruna:kernel` (e.5), the interface a `.wasm` component exports.

## c. Invariants

**PK-I1. The fingerprint is the file.** A package's fingerprint is `sha256` over exactly the bytes the runtime is given; the reader hashes and parses one in-memory copy, read once, and never reads the file again. FP 2.

**PK-I2. A pinned package runs only when its pin holds.** A job document entry of kind `package` without a `fingerprint` is refused naming `kernels[i].fingerprint`; with one, the file is read and hashed before any stage is built, and a mismatch refuses the run naming both values. FP 5.

**PK-I3. The manifest is complete and closed.** Every field of e.1 is present, no other field is, every hint is one e.2 lists, and `format` agrees with the extension; anything else refuses the package naming the field.

**PK-I4. A component imports nothing.** A `.wasm` whose component imports anything is refused naming the first import; one that does not export `declare` and `process` with the world's types, that exports `open` without `close` or the reverse, or that exports anything else, is refused naming the export. A component whose `declare()` disagrees with its embedded manifest on `name`, `declaration`, `abi` or `format` is refused naming the field. FP 3.

**PK-I5. Wheels are resolved for the named targets and never for the packaging machine.** `moruna package` calls `pip download` once per target with `--platform`, `--implementation cp`, `--python-version`, `--abi`, `--only-binary=:all:`, `--no-deps` and `--require-hashes`; a requirement with no wheel for a target refuses the package naming the requirement and the target. FP 6, FP 7.

**PK-I6. A package is reproducible.** The same project, lockfile, wheels and targets give byte-identical `.pex` files: entries sorted by name, every time 1980-01-01 00:00:00, every mode 0644 (0755 for directories), deflate at level 6, bytecode in `unchecked-hash` mode. The same component and `function.yaml` give a byte-identical `.wasm`.

**PK-I7. A tree is used only for the package it was extracted from.** A tree is used when its marker (e.4) names the package's fingerprint and this Moruna's version; otherwise it is extracted anew, and a tree that cannot be written beside the package is written to a private directory and the note says so.

**PK-I8. A hosted run loads no kernel by import path.** Under strict mode a job document entry of kind `python` is refused naming `kernels[i].module`, with the reason that a hosted run loads kernels from packages. FP 4.

**PK-I9. `moruna check <package>` gives the hosted verdict.** It runs the seven steps of MH 4.9 through the package's runtime (SDD 17), never in-process, and the fingerprint it prints is the package's.

## d. Interfaces

### d.1 Exposed (Rust, `moruna-package`)

```rust
pub const MANIFEST_NAME: &str = "moruna.json";          // the zip entry and the wasm custom section
pub const WORLD: &str = "moruna:kernel";                 // a .wasm manifest's entry
pub const WASM_PLATFORM: &str = "wasm32";                // a .wasm manifest's only platform
pub const WIT: &str = /* e.5, byte for byte */;

pub enum Format { Pex, Wasm }
impl Format { pub fn of_path(path: &Path) -> Result<Format, PackageError>; pub fn as_str(&self) -> &'static str; }

pub struct Manifest {
    pub name: String, pub entry: String, pub declaration: DeclarationDoc,
    pub abi: u32, pub moruna: String, pub format: Format, pub platforms: Vec<String>,
}
impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Manifest, PackageError>;   // PK-I3
    pub fn to_json(&self) -> String;                                 // e.1, canonical
    pub fn declared(&self) -> Result<Declared, PackageError>;       // moruna_kernel::declare
    pub fn hints(&self) -> &Hints;
}
pub struct DeclarationDoc { pub input_schema: Option<serde_json::Value>, pub output_schema: Option<serde_json::Value>, pub hints: Hints }
pub struct Hints {
    pub stateful: bool, pub expected_amplification: Option<f64>, pub preferred_rows: Option<u64>,
    pub state_bytes: Option<u64>, pub memory_guard: bool, pub resume: Option<String>,
}

pub struct Package { pub path: PathBuf, pub format: Format, pub fingerprint: [u8; 32], pub manifest: Manifest, /* the bytes */ }
impl Package {
    pub fn open(path: &Path) -> Result<Package, PackageError>;       // PK-I1, PK-I3
    pub fn bytes(&self) -> &[u8];
    pub fn fingerprint_string(&self) -> String;                      // "sha256:<hex>"
    pub fn check_pin(&self, pinned: &str) -> Result<(), PackageError>;  // PK-I2
}

pub fn fingerprint_of(bytes: &[u8]) -> [u8; 32];
pub fn wasm_manifest(component: &[u8]) -> Result<Option<Vec<u8>>, PackageError>;   // the custom section
pub fn embed_manifest(component: &[u8], manifest: &Manifest) -> Result<Vec<u8>, PackageError>;  // f.3

pub struct Tree { pub root: PathBuf, pub deps: PathBuf, pub src: PathBuf, pub notes: Vec<String>, /* private dir guard */ }
pub fn tree(package: &Package, target: &str) -> Result<Tree, PackageError>;        // f.6, PK-I7
pub fn host_target() -> Result<String, PackageError>;            // f.7: this machine's target for a .pex
pub fn compatible(platform: &str, host: &str) -> bool;            // f.7

pub enum PackageError { Io { path, source }, Format { field, reason }, Pin { pinned, actual }, Unsupported(String) }
impl From<PackageError> for MorunaError;   // Config { name: "package", .. }; Pin: Plan
```

### d.2 Exposed (commands)

```
moruna package <project> [--platform <target>]... [--out <dir>] [--index-url <url>] [--find-links <path>]... [--no-index]
moruna package --wasm <module.wasm> <project> [--out <dir>]
moruna check <package> [--json] [--seed N] [--profiles-dir DIR] [--no-profile] [--sandbox-python PATH] [--sandbox-cgroup DIR]
moruna aot <package> [--sandbox-python PATH]
moruna run <spec.json> [--strict] [--sandbox-python PATH] [--sandbox-cgroup DIR]
moruna serve --listen <address> [--no-strict] [--sandbox-python PATH] [--sandbox-cgroup DIR]
```

Exit codes: 0 done (or, for `check`, a checkable and agreeing kernel); 2 refused, with one line naming the field, the file or the requirement. `--platform` defaults to the guest image's targets (MH 4.8): `cp314-manylinux_2_28_x86_64` and `cp314-manylinux_2_28_aarch64`. `--sandbox-python` names a standard-build CPython 3.14 with the `moruna` wheel and `pyarrow` installed, and `--sandbox-cgroup` a cgroup v2 directory the process may create children in with the `memory` and `cpu` controllers enabled (SDD 17 e.6); both are needed for a `.pex` and neither for a `.wasm`.

### d.3 Exposed (Python)

```python
def load(path, *, python=None, cgroup=None) -> PackageKernel
    # python: the standard-build interpreter for a .pex; None is sys.executable when it is a
    # standard build, and refused naming the keyword when it is not. cgroup: as --sandbox-cgroup.

class PackageKernel:                       # frozen; moruna.run accepts it in `kernels`
    path: str; fingerprint: str; format: str; manifest: dict
    def __call__(self, batch: pyarrow.RecordBatch) -> pyarrow.RecordBatch
```

### d.4 Exposed (job document, MH 4.1)

```jsonc
{ "kind": "package", "path": "/disk/0/fn/shout.pex", "fingerprint": "sha256:9f2c…" }
```

`path` and `fingerprint` are required; no other field of a kernel entry may be set (the hints are the manifest's), and each one set is refused by name.

### d.5 Consumed

`moruna_kernel::declare::{Declared, SchemaDecl, ColumnDecl, parse_type, ABI_VERSION}`; `moruna_runtime::check::{check, CheckOptions}`; `moruna_wasm::WasmKernel` and `moruna_sandbox::{SandboxHost, SandboxKernel}` (SDD 17 d.1); `zip`, `sha2`, `serde_json`.

## e. Data model, formats and state machines

### e.1 `moruna.json`

UTF-8 JSON, one object, keys in this order when Moruna writes it, two-space indentation, a final newline:

```json
{
  "name": "shout",
  "entry": "function:shout",
  "declaration": {
    "input_schema": {"name": "string"},
    "output_schema": {"adds": {"loud": "string"}},
    "hints": {"preferred_rows": 8192}
  },
  "abi": 1,
  "moruna": "0.4.3",
  "format": "pex",
  "platforms": ["cp314-manylinux_2_28_aarch64", "cp314-manylinux_2_28_x86_64"]
}
```

- `name`: `[A-Za-z_][A-Za-z0-9_]*`, at most 64 bytes; the package's file is `<name>.<format>`.
- `entry`: for a `.pex`, `<module>:<attribute>` with `module` a dotted name importable from the tree's `src/` and `attribute` a dotted path; for a `.wasm`, exactly `moruna:kernel`.
- `declaration`: e.2. `null` or an absent schema is undeclared; a package whose input is undeclared runs and is not checkable (MH 4.9).
- `abi`: `moruna_kernel::declare::ABI_VERSION` of the Moruna that wrote it; a reader refuses another value.
- `moruna`: the version of the Moruna that wrote it; informational, never compared except for the tree marker and the `.cwasm` header.
- `format`: `pex` or `wasm`, equal to the extension.
- `platforms`: for a `.pex`, the targets it carries wheels for, sorted, at least one, each `cp3NN-manylinux_<major>_<minor>_<x86_64|aarch64>`; for a `.wasm`, exactly `["wasm32"]`.

### e.2 The declaration, spelled as the decorator reads it

- A schema as a JSON object `{column: type}` is a subset (the decorator's mapping).
- A schema as a JSON array of `[column, type]` or `[column, type, nullable]` is exact, in order (the decorator's `pyarrow.Schema`); `nullable` absent is `true`.
- An `output_schema` object whose keys are a non-empty subset of `adds`, `drops`, `changes` is relative: `adds` and `changes` are objects `{column: type}`, `drops` an array of names.
- A type is the type grammar of MH 4.9 (`moruna_kernel::declare::parse_type`).
- `hints` is an object with only these keys: `stateful` (bool, default false), `expected_amplification` (number > 0), `preferred_rows` (integer > 0), `state_bytes` (integer ≥ 0), `memory_guard` (bool, default true: E13, AD-I10), `resume` (`"reinit" | "checkpoint" | "forbid"`, a `.pex` only; a stateful `.wasm` always checkpoints through `close` and `open`). Any other key, or a value of another type, is refused naming `declaration.hints.<key>`.

### e.3 `function.yaml`

```yaml
name: shout                       # required, as e.1
entry: function:shout             # required for a .pex; absent for --wasm (it is moruna:kernel)
input_schema: {name: string}      # optional, as e.2
output_schema: {adds: {loud: string}}
hints: {preferred_rows: 8192}     # optional, as e.2
lockfile: requirements.lock       # optional, a .pex only; relative to the project
```

Parsed with PyYAML (`moruna[package]`); its absence is a refusal naming the extra. Any other key is refused by name. `lockfile` with `--wasm` is refused: a component links its dependencies.

The lockfile is pip's requirements format: one requirement per logical line, each `name==version` followed by at least one `--hash=sha256:<hex>`; comments, blank lines and backslash continuations are allowed; anything else (an unpinned requirement, a URL, an editable, an option line other than `--hash`) is refused naming the line.

### e.4 `.pex` layout

A zip archive (PK-I6) with exactly these top-level entries:

```
__main__.py                         prints moruna.json and exits 0: `python shout.pex`
moruna.json                         e.1
src/function.py, src/<siblings>     the project's files except function.yaml, the lockfile and dot-files
src/**/__pycache__/*.cpython-3NN.pyc
deps/<target>/<installed wheels>    one directory per target in `platforms`
deps/<target>/**/__pycache__/*.pyc
```

A wheel is installed into `deps/<target>/` by unpacking it, moving `<dist>.data/purelib/` and `<dist>.data/platlib/` contents to the root, and leaving out `<dist>.data/{scripts,headers,data}`, each one left out printed on stderr by name. Bytecode is compiled by the packaging interpreter, which must be the minor version of every target (`cp314` needs a 3.14; the standard and the free-threaded build share the bytecode format), in `unchecked-hash` mode.

A tree is the zip extracted under `<package>.tree/` (beside the package) or a private directory, with the marker file `MORUNA-TREE` at its root: one line, `<fingerprint> <moruna version>\n`, written last.

### e.5 The `moruna:kernel` world

```wit
package moruna:kernel@0.1.0;

world kernel {
  /// The package's manifest, moruna.json, as text.
  export declare: func() -> string;
  /// One Arrow record batch as an IPC stream in, one out; an error refuses this batch, named.
  export process: func(batch: list<u8>) -> result<list<u8>, string>;
  /// A stateful kernel's state in: empty at a fresh start, the bytes `close` returned on resume.
  export open: func(state: list<u8>) -> result<_, string>;
  /// A stateful kernel's state out.
  export close: func() -> list<u8>;
}
```

`open` and `close` are exported by a stateful kernel and by no other. The component imports nothing. The manifest is embedded as a custom section named `moruna.json` placed at the end of the component (f.3).

### e.6 `.cwasm` layout

`moruna aot` writes `<package>.cwasm` beside the package: the 16 bytes `MORUNA-CWASM\0\0\0\x01`, a little-endian `u32` header length, a header that is the JSON `{"fingerprint": "sha256:…", "moruna": "0.4.3"}`, then the bytes of wasmtime's `Component::serialize`. SDD 17 f.3 reads it.

## f. Algorithms and policies

**f.1 Open.** Read the file once into memory (`std::fs::read`); the fingerprint is `sha256` of those bytes. Format from the extension (`.pex`, `.wasm`; anything else refused naming the path). A `.pex`'s manifest is its `moruna.json` entry; a `.wasm`'s is its `moruna.json` custom section (exactly one; none or several refused). Parse and validate (PK-I3, e.1, e.2).

**f.2 The pin.** `fingerprint` in a job document is `sha256:<hex>` or `<hex>`, compared lowercase with the file's; another algorithm prefix is refused naming it.

**f.3 Embedding the manifest in a component.** Walk the component's top-level sections (the 8-byte header, then an id byte, an unsigned LEB128 size and the content of each; the header must be the component layer's, `0d 00 01 00`, so a core module is refused), drop every existing `moruna.json` custom section, append one custom section: byte `0x00`, the section size as unsigned LEB128, the name length as LEB128, the name, the manifest bytes of e.1.

**f.4 `moruna package <project>`.**
1. Read `function.yaml` (e.3); build the manifest with `format: pex`, `entry` from the file, `abi` and `moruna` from this build, `platforms` the sorted, de-duplicated `--platform` values (or the defaults of d.2). A target whose interpreter tag is not `cp3NN` with `NN` equal to the packaging interpreter's minor version, or whose platform is not `manylinux_<major>_<minor>_<x86_64|aarch64>`, is refused naming it (a `.pex` runs in the sandbox, which is Linux only; the free-threaded tag `cp314t` is refused because a packaged kernel runs on the standard build, FP 2).
2. Parse the lockfile (e.3). With requirements, for each target, `python -m pip download --dest <tmp>/<target> --no-deps --only-binary=:all: --require-hashes --implementation cp --python-version 3.NN --abi cp3NN --platform <platform> -r <lockfile>` plus `--index-url`, `--find-links`, `--no-index` as given. A non-zero exit refuses the package: the requirement in pip's `No matching distribution found for <req>` (or, failing that, pip's last stderr line) and the target are named (PK-I5). Every requirement must have produced exactly one wheel whose normalised name matches it.
3. Install each wheel into `deps/<target>/` (e.4), copy `src/`, compile bytecode, write the zip (PK-I6) to `<out>/<name>.pex` (default: the project directory), print the fingerprint.

**f.5 `moruna package --wasm <module.wasm> <project>`.** Read `function.yaml`; build the manifest with `format: wasm`, `entry: moruna:kernel`, `platforms: ["wasm32"]`; verify the component (SDD 17 f.2: imports, exports, types); instantiate it with no imports and call `declare()`; compare its manifest with the one built here field by field on `name`, `declaration`, `abi`, `format`, `entry`, `platforms` (PK-I4); embed (f.3); write `<out>/<name>.wasm`; print the fingerprint.

**f.6 The tree.** Given a `.pex` and this machine's target (f.7): if `<package>.tree/MORUNA-TREE` names the fingerprint and this version, use it. Otherwise extract into `<package>.tree.tmp-<pid>-<n>/`, write the marker, remove a stale `<package>.tree/` and rename. When the package's directory is not writable, extract into a private directory under the system temporary directory, removed when the `Tree` is dropped, with the note "`<package>.tree` cannot be written beside the package; extracted to a private directory for this process". The chosen `deps/<target>/` is the first of the package's `platforms` compatible with the host (f.7); none compatible refuses naming the host's target and the package's platforms.

**f.7 Platform compatibility.** The host's target for a `.pex` is `cp3NN-manylinux_<glibc major>_<glibc minor>_<arch>` from the sandbox interpreter's version, its C library's version (`os.confstr("CS_GNU_LIBC_VERSION")`) and the machine's architecture, which the template reports in its `hello` (17 e.3); on a host that is not Linux with glibc, `host_target` is refused ("a .pex runs in the sandbox, which needs Linux with glibc"). `manylinux_<a>_<b>_<arch>` is compatible with a host `manylinux_<c>_<d>_<arch>` when the interpreter tags are equal, the architectures are equal, and `(a, b) <= (c, d)`.

**f.8 The job document.** `KernelKindDoc::Package` entries are built by SDD 17's runtimes: open the package (f.1), check the pin (f.2), then `moruna_wasm::WasmKernel::new` or `moruna_sandbox::SandboxKernel::new` by format. A `.pex` entry without the sandbox host of d.2 (`--sandbox-python`, `--sandbox-cgroup`) is refused naming the missing option. Strict mode refuses `kind: python` before any kernel is loaded (PK-I8).

**f.9 `moruna check <package>`.** Open the package; build its kernel as f.8 does; run `moruna_runtime::check::check` with `kind` `wasm` or `pex` and `fingerprint_scheme` `sha256`; the report's `fingerprint` is the package's (PK-I9). The kernel's `Fingerprint` (contracts e.6) is the package's 32 bytes, so the profile row is keyed by the file and a later run of the same file sizes from it.

**f.10 `moruna aot <package>`.** A `.wasm`: compile the component with the engine of SDD 17 f.1 and write e.6. A `.pex`: write the tree beside the package (f.6) and refuse when that place is not writable (the point of `aot` is the place). Both print the path written.

**f.11 `moruna load <package>`.** Open the package and build its kernel as f.8 does, bound to a private arena of `LOAD_ARENA_BYTES` and the process's sampler, with one instance; `__call__` runs one batch through `Kernel::apply` and returns it as a `pyarrow.RecordBatch`. `moruna.run` given a `PackageKernel` adds the document entry of d.4 with the loaded fingerprint, so the run opens, pins and runs the same file under its own budget.

## g. Concurrency within the component

`Package` is immutable after `open` and `Send + Sync`. Two processes extracting the same tree race only on the final rename; the loser finds a complete tree with the right marker on its next check and uses it, and its own temporary directory is removed. `PackageKernel.__call__` serialises its calls (one instance).

## h. Behaviour

- Normal: open, pin, tree or compile, run.
- A package changed after a document was written: refused by the pin before any stage (PK-I2).
- A `.pex` whose `deps/` holds a library the sandbox's template already imported at another version: refused by the sandbox when the stage starts (SDD 17 f.6), naming both versions.
- A `.wasm` with WASI imports (a `wasm32-wasip2` build): refused naming the first import (PK-I4).
- A `.cwasm` from another version or for another fingerprint: ignored, compiled, noted (SDD 17 f.3).
- `moruna check` on a `.pex` on macOS: refused, "a .pex runs in the sandbox, which needs Linux".

## i. Configuration

| Name | Value | Reason |
|---|---|---|
| `DEFAULT_TARGETS` | `cp314-manylinux_2_28_x86_64`, `cp314-manylinux_2_28_aarch64` | the guest image's interpreter and the wheel platform the release builds for (MH 4.8, PY-I7) |
| `MANIFEST_MAX_BYTES` | 1 MiB | a manifest is a declaration, not data; a larger one is refused |
| `LOAD_ARENA_BYTES` | 256 MiB | the arena `moruna load` lands outputs in; a batch larger than it is refused by the arena, named |
| `ZIP_LEVEL` | deflate 6 | reproducible and small |

## j. Observability

`moruna package` prints one line per target resolved, one per wheel installed, one per wheel part left out, and the fingerprint. A run's notes carry every tree note (f.6) and every compile note (SDD 17 f.3). `moruna check` reports `kind` `wasm` or `pex` and `fingerprint: sha256:<hex>`.

## k. Tests

**PK-T1 manifest_round_trip.** A manifest written by `to_json` parses back equal; each field removed, an unknown field added, a wrong `abi`, a `format` disagreeing with the extension, an unknown hint, a hint of the wrong type: each refused naming the field. PK-I3.

**PK-T2 fingerprint_is_the_file.** A package's fingerprint equals `sha256sum` of the file; one byte changed changes it; a job document pinned to the old value is refused naming both values, and one with no pin is refused naming `kernels[0].fingerprint`. PK-I1, PK-I2.

**PK-T3 declaration_spellings.** An object is a subset, an array exact (nullable defaulting true), `{adds, drops, changes}` relative for an output and refused for an input; every type of the grammar parses; the result equals what the decorator builds from the same declaration. e.2.

**PK-T4 embed_and_read_section.** A component with a `moruna.json` section embedded twice ends with exactly one, the last; reading returns it; a component with none is refused when opened. f.3.

**PK-T5 tree_marker.** A tree is extracted beside the package with its marker; a second open uses it without extracting (its modification time is unchanged); a marker naming another fingerprint is replaced; in a read-only directory the tree goes to a private directory, the note says so, and the directory is gone after the drop. PK-I7, f.6.

**PK-T6 platform_compatibility.** `manylinux_2_17_x86_64` runs on a `2_28` host, `2_31` does not, `aarch64` does not run on `x86_64`, the interpreter tags must be equal. f.7.

**PK-T7 package_a_project.** (`python/tests/test_package.py`) A project with a two-requirement lockfile, resolved from a local `--find-links` directory of wheels built by the test for `manylinux_2_28_x86_64` and `manylinux_2_28_aarch64`, gives a `.pex` with both `deps/` trees, bytecode beside every `.py`, the manifest of e.1; packaging twice gives the same bytes. PK-I5, PK-I6.

**PK-T8 missing_wheel_refused.** The same project with a requirement whose wheel exists for `x86_64` only: refused naming the requirement and `cp314-manylinux_2_28_aarch64`. PK-I5.

**PK-T9 lockfile_rules.** An unpinned line, a URL, a line with no hash, `cp314t`, `macosx_14_0_arm64` and a bytecode version that differs from the interpreter's: each refused, named. e.3, f.4.

**PK-T10 package_a_component.** `moruna package --wasm` on the identity component (SDD 17 SB-T1) writes `<name>.wasm` whose section and `declare()` agree; a `function.yaml` naming another column type is refused naming `declaration`; a component importing a function is refused naming the import. PK-I4.

**PK-T11 strict_refuses_import_paths.** A document with a `python` kernel built with `strict` is refused naming `kernels[0].module`; the same document without `strict` builds. PK-I8.

**PK-T12 package_entry_fields.** A `package` entry with `module`, `callable`, `name`, `args` or any hint set is refused naming that field; `path` absent is refused. d.4.

**PK-T13 check_through_the_runtime.** `moruna check` on the identity `.wasm` agrees and prints `sha256:<file hash>`; on the column-adding `.wasm` that disagrees with its declaration it is refused naming the column; the profile row it wrote is keyed by the package's fingerprint. PK-I9.

**PK-T14 check_a_pex.** (Linux, sandbox host, SB-T rules) `moruna check` on a packaged Polars kernel agrees and its report's allocation figures come from the child. PK-I9.

**PK-T15 aot_and_load.** `moruna aot x.wasm` writes `x.cwasm` with the header of e.6; `moruna load x.wasm` then uses it (no compile note); a `.cwasm` with another fingerprint is ignored and the note says the component was compiled; `load(x)(batch)` returns the kernel's output. f.10, f.11.

**PK-T16 run_a_package.** A job document with a `package` entry runs to a Parquet sink and the rows written equal the kernel's output of the source. f.8.

## l. Implementation notes for the agent

`moruna-package` depends on `sha2`, `serde`, `serde_json`, `zip` (deflate only) and `moruna-kernel`; private directories are `std::env::temp_dir()` plus the process id and a counter. No `unsafe`. `moruna package` is Python because it drives pip; its Rust half (`--wasm`, the manifest) is `_core.package_wasm` and `_core.manifest_json`. A component is verified and compiled by `moruna-wasm` (SDD 17).

Environment facts to verify before starting: `pip download --platform manylinux_2_28_aarch64 --only-binary=:all: --no-deps` runs on the packaging machine.

## m. Open items

None.

## n. Traceability

| Parent | Invariant | Test |
|---|---|---|
| FP 2 (the file is the pin) | PK-I1, PK-I2 | PK-T2 |
| FP 2 (`moruna.json`) | PK-I3, e.1, e.2 | PK-T1, PK-T3 |
| FP 3 (the world) | PK-I4, e.5 | PK-T4, PK-T10 |
| FP 6, FP 7 (wheels for the named platforms) | PK-I5, PK-I6 | PK-T7, PK-T8, PK-T9 |
| FP 6 (`aot`, `load`) | PK-I7, f.10, f.11 | PK-T5, PK-T15 |
| FP 6 (`check` through the runtime) | PK-I9 | PK-T13, PK-T14 |
| FP 4 (strict mode) | PK-I8 | PK-T11 |
| FP 5 (the job document) | PK-I2, d.4, f.8 | PK-T2, PK-T12, PK-T16 |

## o. Deferred (post-v1)

**PK-O1. Python compiled to WebAssembly.** Refused by FP 2 and FP 7 until the data libraries publish for a WASI runtime.

**PK-O2. A build system for other languages.** `--wasm` verifies; it does not compile (FP 7).
