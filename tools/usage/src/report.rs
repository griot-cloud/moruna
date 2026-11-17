//! The report: from each job's evidence to what it used, and from all of them to what nothing
//! used, sorted by how much user space can reach.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use moruna_usage::{
    dump,
    elf::{Elf, FUNC, LOAD},
    profraw,
};
use serde::Serialize;

use crate::{
    graph::{Graph, USER_ENTRIES},
    llvm::{Export, Function, Tools, demangle},
    serial, source,
};

/// The kernel's profile, beside the programs'.
pub const KERNEL: &str = "kernel";

/// Kept for the accelerator path whatever their use (Brackly, 2026-11-17): the device and DMA
/// calls, the driver processes and the device manager, and the kernel-mode code they need.
pub const KEEP_CALLS: [&str; 4] = [
    "MEMORY_PIN",
    "DEVICE_MEMORY_CREATE",
    "INTERRUPT_CREATE_MSI",
    "DMA_RELEASE",
];
pub const KEEP_PROGRAMS: [&str; 6] = [
    "devices",
    "virtio-blk",
    "virtio-vsock",
    "virtio-net",
    "nvme",
    "gve",
];
/// Kernel-mode code whose file path holds one of these.
const KEEP_PATHS: [&str; 2] = [
    "/ostd-0.19.0/src/arch/x86/iommu/",
    "/ostd-0.19.0/src/mm/dma/",
];
/// Kernel-mode code whose name holds one of these, ignoring case.
const KEEP_NAMES: [&str; 6] = [
    "msi",
    "dma",
    "memory_pin",
    "device_memory",
    "devicememory",
    "moruna_microkernel::memory::pin",
];

/// A dispatcher whose arms are counted: where it is, and where its numbers are.
#[derive(Clone, Copy, Debug)]
pub struct Dispatch {
    pub what: &'static str,
    /// `KERNEL` or a program's binary name.
    pub binary: &'static str,
    pub file: &'static str,
    pub function: &'static str,
    pub prefix: &'static str,
    pub numbers_file: &'static str,
    pub numbers_module: &'static str,
}

/// The dispatchers. When B2's personality replaces `kernel/linux/server`, its call dispatcher
/// goes here beside (or in place of) `linux`'s, with its program's name.
pub const DISPATCHES: [Dispatch; 3] = [
    Dispatch {
        what: "native",
        binary: KERNEL,
        file: "kernel/core/src/syscall.rs",
        function: "dispatch",
        prefix: "nr",
        numbers_file: "kernel/abi/src/lib.rs",
        numbers_module: "nr",
    },
    Dispatch {
        what: "linux-kernel",
        binary: KERNEL,
        file: "kernel/core/src/foreign.rs",
        function: "fast_call",
        prefix: "fast",
        numbers_file: "kernel/linux/core/src/table.rs",
        numbers_module: "nr",
    },
    Dispatch {
        what: "linux-server",
        binary: "linux",
        file: "kernel/linux/server/src/task.rs",
        function: "call",
        prefix: "nr",
        numbers_file: "kernel/linux/core/src/table.rs",
        numbers_module: "nr",
    },
];

/// The moruna-sys sources whose functions name the native calls a program makes.
const SYS_SOURCES: [&str; 7] = [
    "kernel/sys/src/raw.rs",
    "kernel/sys/src/lib.rs",
    "kernel/sys/src/datapath.rs",
    "kernel/sys/src/personality.rs",
    "kernel/sys/src/plane.rs",
    "kernel/sys/src/private_memory.rs",
    "kernel/sys/src/usage.rs",
];

/// What the report is given.
pub struct Config {
    /// The source tree the usage build was built from.
    pub src: PathBuf,
    /// The usage build's kit (`kernel/tools/build.sh`'s OUT with MORUNA_USAGE=1).
    pub kit: PathBuf,
    /// A production kit of the same tree, for sizes; else the usage kit's (instrumented) sizes.
    pub prod_kit: Option<PathBuf>,
    pub out: PathBuf,
    /// Each job's label and its MORUNA_USAGE_DIR.
    pub runs: Vec<(String, PathBuf)>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CallUse {
    pub names: Vec<String>,
    pub numbers: Vec<u64>,
    pub line: usize,
    /// None when the binary had no profile in that job.
    pub count: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ProgramUse {
    pub processes: BTreeMap<String, u64>,
    pub wakes: u64,
    pub lines: (u64, u64),
    /// Native calls it made, from moruna-sys's wrappers.
    pub calls: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Scope {
    pub label: String,
    /// Dispatcher (`what`) to its arms.
    pub dispatch: BTreeMap<String, Vec<CallUse>>,
    /// Linux calls refused, by number, from the Linux server's lines.
    pub refused: BTreeMap<u64, u64>,
    /// Linux processes that ended, with the calls each was served.
    pub linux_processes: Vec<(String, u64)>,
    pub programs: BTreeMap<String, ProgramUse>,
    /// Kernel-mode coverage by area: lines run, lines mapped (generic code never instantiated
    /// included), functions run and functions in the image.
    pub kernel_areas: BTreeMap<String, [u64; 4]>,
    pub dumps: usize,
    pub broken_dumps: usize,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Item {
    pub group: String,
    pub name: String,
    pub detail: String,
    pub lines: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Report {
    pub jobs: Vec<Scope>,
    pub all: Scope,
    /// Never used, by group: A to E, in the order they are listed.
    pub unused: Vec<Item>,
    /// Kept for the accelerator path, used or not.
    pub keep: Vec<Item>,
    pub notes: Vec<String>,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// A source file's short name: from `kernel/`, `crates/`, the toolchain's `library/` or a
/// registry crate's directory on.
pub fn short(path: &str) -> String {
    for marker in ["/vendor/", "/library/", "/registry/src/"] {
        if let Some(at) = path.find(marker) {
            let rest = &path[at + marker.len()..];
            let rest = if marker == "/registry/src/" {
                rest.split_once('/').map_or(rest, |(_, r)| r)
            } else {
                rest
            };
            return rest.to_string();
        }
    }
    for marker in ["/kernel/", "/crates/", "/images/"] {
        if let Some(at) = path.find(marker) {
            return path[at + 1..].to_string();
        }
    }
    path.to_string()
}

/// The kernel-mode area a source file belongs to.
pub fn area(path: &str) -> &'static str {
    if path.contains("/vendor/ostd-") {
        "OSTD 0.19"
    } else if path.contains("/vendor/osdk-frame-allocator") {
        "frame allocator (OSDK)"
    } else if path.contains("/kernel/core/src/") {
        "Moruna's core (kernel/core)"
    } else if path.contains("/kernel/boot/") {
        "boot (kernel/boot)"
    } else if path.contains("/kernel/") {
        "kernel crates (abi, loader, handles, quota, datapath)"
    } else if path.contains("/rust/library/") {
        "core and alloc"
    } else {
        "dependencies"
    }
}

/// A demangled name without its generic arguments (`::<...>`), which may name any type.
fn without_arguments(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if depth == 0 && name[i..].starts_with("::<") {
            depth = 1;
            i += 3;
            continue;
        }
        match (depth, bytes[i]) {
            (0, _) => out.push(bytes[i] as char),
            (_, b'<') => depth += 1,
            (_, b'>') => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    out
}

/// Kept for the accelerator path: OSTD's IOMMU and DMA code by its files, and Moruna's core's
/// device, DMA and MSI code by its names.
pub fn keep(name: &str, file: &str) -> bool {
    if KEEP_PATHS.iter().any(|p| file.contains(p)) {
        return true;
    }
    let lower = without_arguments(name).to_lowercase();
    file.contains("/kernel/core/src/") && KEEP_NAMES.iter().any(|n| lower.contains(n))
}

/// Function name (demangled) to its size in an ELF's symbol table, summed over copies.
pub fn sizes(elf: &[u8]) -> Result<BTreeMap<String, u64>, String> {
    let elf = Elf::parse(elf).map_err(|e| e.to_string())?;
    let mut out = BTreeMap::new();
    for s in elf.symbols().map_err(|e| e.to_string())? {
        if s.kind == FUNC && s.size > 0 {
            *out.entry(demangle(&s.name)).or_insert(0) += s.size;
        }
    }
    Ok(out)
}

/// A program's loaded bytes: its loadable segments' file sizes.
pub fn loaded_bytes(elf: &[u8]) -> u64 {
    Elf::parse(elf).map_or(0, |e| {
        e.segments
            .iter()
            .filter(|s| s.kind == LOAD)
            .map(|s| s.filesz)
            .sum()
    })
}

/// The evidence of one job, turned into profiles.
struct Evidence {
    label: String,
    /// Binary name to its raw profile.
    profiles: BTreeMap<String, PathBuf>,
    programs: BTreeMap<String, dump::Program>,
    linux: Vec<serial::LinuxEnd>,
    dumps: usize,
    broken: usize,
}

fn evidence(cfg: &Config, label: &str, dir: &Path) -> Result<Evidence, String> {
    let out = cfg.out.join("jobs").join(label);
    let mut profiles = BTreeMap::new();
    let kernel = read(&dir.join("kernel.elf"))?;
    let counters = read(&dir.join("kernel.cnts"))?;
    let elf = Elf::parse(&kernel).map_err(|e| format!("{label}: kernel ELF: {e}"))?;
    let raw = profraw::build(&elf, &counters).map_err(|e| format!("{label}: kernel: {e}"))?;
    let path = out.join("kernel.profraw");
    write(&path, &raw)?;
    profiles.insert(KERNEL.to_string(), path);
    let log = String::from_utf8_lossy(&read(&dir.join("serial.log"))?).into_owned();
    let decoded = dump::decode(&log);
    let programs = dump::programs(&decoded.dumps).map_err(|e| format!("{label}: {e}"))?;
    let programs = named(programs, &kit_index(&cfg.kit)?).map_err(|e| format!("{label}: {e}"))?;
    for (name, program) in &programs {
        let binary = read(&cfg.kit.join("node-user-bins").join(name))?;
        let elf = Elf::parse(&binary).map_err(|e| format!("{label}: {name}: {e}"))?;
        let layout = profraw::counters(&elf).map_err(|e| format!("{label}: {name}: {e}"))?;
        if layout.addr != program.addr || layout.count() != program.counters.len() as u64 {
            return Err(format!(
                "{label}: {name}: its dumps are not of the kit's build ({:#x}, {} counters, against {:#x}, {})",
                program.addr,
                program.counters.len(),
                layout.addr,
                layout.count()
            ));
        }
        let raw = profraw::build(&elf, &profraw::to_bytes(&program.counters))
            .map_err(|e| format!("{label}: {name}: {e}"))?;
        let path = out.join(format!("{name}.profraw"));
        write(&path, &raw)?;
        profiles.insert(name.clone(), path);
    }
    Ok(Evidence {
        label: label.to_string(),
        profiles,
        programs,
        linux: serial::linux_ends(&log),
        dumps: decoded.dumps.len(),
        broken: decoded.broken,
    })
}

/// The kit's programs by their fingerprint (`dump::fingerprint`): a dump names its build that
/// way, not by a name. Two programs of one fingerprint are both named, joined by `|`.
pub fn kit_index(kit: &Path) -> Result<BTreeMap<String, String>, String> {
    let bins = kit.join("node-user-bins");
    let mut index: BTreeMap<String, String> = BTreeMap::new();
    let mut names: Vec<String> = fs::read_dir(&bins)
        .map_err(|e| format!("{}: {e}", bins.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    for name in names {
        let Ok(bytes) = fs::read(bins.join(&name)) else {
            continue;
        };
        let Some(print) = Elf::parse(&bytes)
            .ok()
            .and_then(|e| profraw::fingerprint(&e))
        else {
            continue;
        };
        index
            .entry(print)
            .and_modify(|n| {
                n.push('|');
                n.push_str(&name);
            })
            .or_insert(name);
    }
    Ok(index)
}

/// Programs keyed by fingerprint, keyed and named by the kit's program instead.
pub fn named(
    programs: BTreeMap<String, dump::Program>,
    index: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, dump::Program>, String> {
    let mut out = BTreeMap::new();
    for (print, mut program) in programs {
        let name = index
            .get(&print)
            .ok_or_else(|| format!("dumps of a program not in the kit (build {print})"))?;
        if name.contains('|') {
            return Err(format!("dumps of build {print}, which is each of {name}"));
        }
        program.program.clone_from(name);
        out.insert(name.clone(), program);
    }
    Ok(out)
}

/// The binary a profile belongs to.
fn binary(cfg: &Config, name: &str, job: Option<&Path>) -> PathBuf {
    match (name, job) {
        (KERNEL, Some(dir)) => dir.join("kernel.elf"),
        (KERNEL, None) => cfg.kit.join("moruna-microkernel.elf"),
        (program, _) => cfg.kit.join("node-user-bins").join(program),
    }
}

/// The source files the report reads, by their path under `src`.
struct Sources {
    texts: BTreeMap<String, String>,
}

impl Sources {
    fn load(src: &Path) -> Sources {
        let mut texts = BTreeMap::new();
        let files = DISPATCHES
            .iter()
            .flat_map(|d| [d.file, d.numbers_file])
            .chain(SYS_SOURCES)
            .chain(["kernel/linux/server/src/task.rs"]);
        for file in files {
            if let Ok(text) = fs::read_to_string(src.join(file)) {
                texts.insert(file.to_string(), text);
            }
        }
        Sources { texts }
    }

    fn get(&self, file: &str) -> &str {
        self.texts.get(file).map_or("", String::as_str)
    }
}

/// The arms of `d` counted in `export` (none when the binary had no profile).
fn arm_uses(d: &Dispatch, sources: &Sources, export: Option<&Export>) -> Vec<CallUse> {
    let numbers = source::numbers(sources.get(d.numbers_file), d.numbers_module);
    let file = export.and_then(|e| e.file(d.file));
    source::arms(sources.get(d.file), d.function, d.prefix)
        .into_iter()
        .map(|arm| CallUse {
            numbers: arm
                .names
                .iter()
                .filter_map(|n| numbers.get(n).copied())
                .collect(),
            names: arm.names,
            line: arm.line,
            count: export.map(|_| {
                file.and_then(|f| f.count_after(arm.line as u64, arm.arrow as u64 + 1))
                    .unwrap_or(0)
            }),
        })
        .collect()
}

/// The native calls a program made, from the coverage of moruna-sys's functions that each name
/// one call.
fn program_calls(export: &Export, sources: &Sources) -> BTreeMap<String, u64> {
    let mut spans: Vec<(String, source::Span, String)> = Vec::new();
    for file in SYS_SOURCES {
        for (span, names) in source::functions_naming(sources.get(file), "nr") {
            if let [name] = names.as_slice() {
                spans.push((file.to_string(), span, name.clone()));
            }
        }
    }
    let mut calls = BTreeMap::new();
    for f in &export.functions {
        let Some((first, _)) = f.span() else { continue };
        let innermost = spans
            .iter()
            .filter(|(file, span, _)| {
                f.file().ends_with(file.as_str())
                    && span.first as u64 <= first
                    && first <= span.last as u64
            })
            .min_by_key(|(_, span, _)| span.lines());
        if let Some((_, _, name)) = innermost
            && f.count > 0
        {
            *calls.entry(name.clone()).or_insert(0) += f.count;
        }
    }
    calls
}

/// Covered and total lines of the files under `kernel/` (the program's own and Moruna's).
fn own_lines(export: &Export) -> (u64, u64) {
    export
        .files
        .iter()
        .filter(|f| f.filename.contains("/kernel/"))
        .fold((0, 0), |(c, t), f| {
            (c + f.summary.lines.covered, t + f.summary.lines.count)
        })
}

/// The functions of `export` that are in the image (`image`, from the profile), all of them
/// when the image is not known.
fn in_image<'e>(export: &'e Export, image: Option<&BTreeSet<String>>) -> Vec<&'e Function> {
    export
        .functions
        .iter()
        .filter(|f| image.is_none_or(|i| i.contains(&f.name)))
        .collect()
}

fn scope(
    label: &str,
    exports: &BTreeMap<String, Export>,
    image: &BTreeMap<String, BTreeSet<String>>,
    programs: &BTreeMap<String, dump::Program>,
    linux: &[serial::LinuxEnd],
    sources: &Sources,
) -> Scope {
    let mut s = Scope {
        label: label.to_string(),
        ..Scope::default()
    };
    for d in &DISPATCHES {
        s.dispatch.insert(
            d.what.to_string(),
            arm_uses(d, sources, exports.get(d.binary)),
        );
    }
    for end in linux {
        for (n, c) in &end.refused {
            *s.refused.entry(*n).or_insert(0) += c;
        }
        s.linux_processes.push((end.name.clone(), end.served));
    }
    for (name, p) in programs {
        let mut u = ProgramUse {
            processes: p.processes.clone(),
            wakes: p.wakes,
            ..ProgramUse::default()
        };
        if let Some(e) = exports.get(name) {
            u.lines = own_lines(e);
            u.calls = program_calls(e, sources);
        }
        s.programs.insert(name.clone(), u);
    }
    if let Some(k) = exports.get(KERNEL) {
        for f in &k.files {
            let a = s
                .kernel_areas
                .entry(area(&f.filename).to_string())
                .or_default();
            a[0] += f.summary.lines.covered;
            a[1] += f.summary.lines.count;
        }
        let functions: Vec<Function> = in_image(k, image.get(KERNEL))
            .into_iter()
            .cloned()
            .collect();
        for ((file, _), f) in source_functions(&functions, &BTreeMap::new(), &BTreeSet::new()) {
            let a = s.kernel_areas.entry(area(&file).to_string()).or_default();
            a[2] += u64::from(f.count > 0);
            a[3] += 1;
        }
    }
    s
}

/// Lines and bytes of the functions named by `names` (demangled), counted once each.
fn size_of(functions: &[&Function], sizes: &BTreeMap<String, u64>) -> (u64, u64) {
    let mut seen = BTreeSet::new();
    let mut lines = 0;
    let mut bytes = 0;
    for f in functions {
        let name = f.demangled();
        if seen.insert(name.clone()) {
            lines += f.lines();
            bytes += sizes.get(&name).copied().unwrap_or(0);
        }
    }
    (lines, bytes)
}

/// The functions of `export` an arm's body calls: by the called path's last segments, preferring
/// those in the arm's own file.
fn handlers<'e>(export: &'e Export, body: &str, file: &str) -> Vec<&'e Function> {
    let Some(path) = source::called(body) else {
        return Vec::new();
    };
    let path = path.trim_start_matches("self.").replace('.', "::");
    let suffix = format!("::{path}");
    let all: Vec<&Function> = export
        .functions
        .iter()
        .filter(|f| f.demangled().ends_with(&suffix))
        .collect();
    let own: Vec<&Function> = all
        .iter()
        .copied()
        .filter(|f| f.file().ends_with(file))
        .collect();
    if own.is_empty() { all } else { own }
}

/// One function in the source, over all its instantiations in the binary.
#[derive(Clone, Debug, Default)]
struct SourceFn {
    name: String,
    lines: u64,
    count: u64,
    bytes: u64,
    reached: bool,
    kept: bool,
}

/// The binary's functions by their place in the source (file, first line), so a generic
/// function is unused only when none of its instantiations ran.
fn source_functions(
    functions: &[Function],
    sizes: &BTreeMap<String, u64>,
    reached: &BTreeSet<String>,
) -> BTreeMap<(String, u64), SourceFn> {
    let mut out: BTreeMap<(String, u64), SourceFn> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for f in functions {
        let Some((first, _)) = f.span() else { continue };
        let name = f.demangled();
        let s = out.entry((f.file().to_string(), first)).or_default();
        if s.name.is_empty() || name.len() < s.name.len() {
            s.name.clone_from(&name);
        }
        s.lines = s.lines.max(f.lines());
        s.count = s.count.saturating_add(f.count);
        s.reached |= reached.contains(&name);
        s.kept |= keep(&name, f.file());
        if seen.insert(name.clone()) {
            s.bytes += sizes.get(&name).copied().unwrap_or(0);
        }
    }
    out
}

/// Unused source functions, one item per module (file), in `group`.
fn kernel_modules(
    functions: &[(&(String, u64), &SourceFn)],
    group: &str,
    detail: &str,
) -> Vec<Item> {
    let mut modules: BTreeMap<String, Vec<&SourceFn>> = BTreeMap::new();
    for ((file, _), f) in functions {
        modules.entry(short(file)).or_default().push(f);
    }
    modules
        .into_iter()
        .map(|(module, mut fs)| {
            fs.sort_by_key(|f| std::cmp::Reverse((f.bytes, f.lines)));
            let names: Vec<&str> = fs.iter().take(3).map(|f| f.name.as_str()).collect();
            Item {
                group: group.to_string(),
                name: module,
                detail: format!(
                    "{} functions never run ({detail}); largest: {}",
                    fs.len(),
                    names.join(", ")
                ),
                lines: fs.iter().map(|f| f.lines).sum(),
                bytes: fs.iter().map(|f| f.bytes).sum(),
            }
        })
        .collect()
}

fn sort(items: &mut [Item]) {
    items.sort_by(|a, b| (b.bytes, b.lines, &a.name).cmp(&(a.bytes, a.lines, &b.name)));
}

/// The group letters, in the order the coordinator set (2026-11-17): what user space can reach
/// first.
pub const GROUPS: [(&str, &str); 5] = [
    ("A", "Native calls never made (any process can make them)"),
    ("B", "Kernel-mode code reachable from user space, never run"),
    (
        "C",
        "Linux calls never answered (any Linux program can make them)",
    ),
    (
        "D",
        "Services and programs never started, or started and idle",
    ),
    (
        "E",
        "Kernel-mode code reachable only from the kernel, never run",
    ),
];

/// The candidates for removal and the accelerator path's parts, from the merged profiles.
#[allow(clippy::too_many_arguments)]
fn candidates(
    cfg: &Config,
    exports: &BTreeMap<String, Export>,
    image: &BTreeMap<String, BTreeSet<String>>,
    all: &Scope,
    sources: &Sources,
    tools: &dyn Tools,
    started: &BTreeSet<String>,
    notes: &mut Vec<String>,
) -> Result<(Vec<Item>, Vec<Item>), String> {
    let mut unused = Vec::new();
    let mut keep_items = Vec::new();
    let prod_kernel = cfg
        .prod_kit
        .as_ref()
        .map(|k| k.join("moruna-microkernel.elf"));
    let sized_kernel = prod_kernel
        .clone()
        .unwrap_or_else(|| binary(cfg, KERNEL, None));
    if prod_kernel.is_none() {
        notes.push(
            "Bytes are the usage build's (instrumented) sizes: no --prod-kit was given.".into(),
        );
    }
    let kernel_sizes = sizes(&read(&sized_kernel)?)?;
    let graph = Graph::parse(&tools.disassemble(&sized_kernel)?);
    let reached = graph.reachable(&USER_ENTRIES);
    let empty = Export::default();
    let kernel = exports.get(KERNEL).unwrap_or(&empty);
    let mut listed: BTreeSet<String> = BTreeSet::new();

    // A, and the kept calls: the native dispatcher's arms.
    let native = DISPATCHES[0];
    let arms = source::arms(sources.get(native.file), native.function, native.prefix);
    for (arm, used) in arms.iter().zip(&all.dispatch[native.what]) {
        let fs = handlers(kernel, &arm.body, native.file);
        let (lines, bytes) = size_of(&fs, &kernel_sizes);
        let name = arm.names.join(" | ");
        let kept = arm.names.iter().any(|n| KEEP_CALLS.contains(&n.as_str()));
        let count = used.count.unwrap_or(0);
        if kept {
            keep_items.push(Item {
                group: "native call".into(),
                name,
                detail: format!("made {count} times"),
                lines,
                bytes,
            });
        } else if count == 0 {
            listed.extend(fs.iter().map(|f| f.demangled()));
            unused.push(Item {
                group: "A".into(),
                name,
                detail: format!(
                    "number {:?}; handler {}",
                    used.numbers,
                    source::called(&arm.body).unwrap_or_default()
                ),
                lines: lines.max(1),
                bytes,
            });
        }
    }

    // B and E, and the kept kernel-mode code: source functions in the image never run, by
    // module.
    let compiled: Vec<Function> = in_image(kernel, image.get(KERNEL))
        .into_iter()
        .cloned()
        .collect();
    let functions = source_functions(&compiled, &kernel_sizes, &reached);
    let mut never: (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
    let mut kept_modules: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for (key, f) in &functions {
        if f.kept {
            let m = kept_modules.entry(short(&key.0)).or_default();
            *m = (
                m.0 + f.lines,
                m.1 + f.bytes,
                m.2 + u64::from(f.count > 0),
                m.3 + 1,
            );
        } else if f.count == 0 && !listed.contains(&f.name) {
            if f.reached {
                never.0.push((key, f));
            } else {
                never.1.push((key, f));
            }
        }
    }
    unused.extend(kernel_modules(
        &never.0,
        "B",
        "on a direct call path from the user-mode loop",
    ));
    unused.extend(kernel_modules(
        &never.1,
        "E",
        "on no direct call path from user space",
    ));
    for (module, (lines, bytes, used, all)) in kept_modules {
        keep_items.push(Item {
            group: "kernel-mode code".into(),
            name: module,
            detail: format!("{used} of {all} functions run"),
            lines,
            bytes,
        });
    }

    // C: Linux calls never answered, in the server and the kernel's fast path.
    for d in DISPATCHES.iter().filter(|d| d.what.starts_with("linux")) {
        let Some(export) = exports.get(d.binary) else {
            notes.push(format!(
                "{}: no profile of {}, so its calls are not judged",
                d.what, d.binary
            ));
            continue;
        };
        let arm_sizes = if d.binary == KERNEL {
            kernel_sizes.clone()
        } else {
            let elf = cfg
                .prod_kit
                .as_ref()
                .map(|k| k.join("node-user-bins").join(d.binary))
                .unwrap_or_else(|| binary(cfg, d.binary, None));
            read(&elf).and_then(|b| sizes(&b)).unwrap_or_default()
        };
        let arms = source::arms(sources.get(d.file), d.function, d.prefix);
        for (arm, used) in arms.iter().zip(&all.dispatch[d.what]) {
            if used.count != Some(0) {
                continue;
            }
            let fs = handlers(export, &arm.body, d.file);
            let (lines, bytes) = size_of(&fs, &arm_sizes);
            let refused: u64 = used.numbers.iter().filter_map(|n| all.refused.get(n)).sum();
            unused.push(Item {
                group: "C".into(),
                name: arm.names.join(" | "),
                detail: format!(
                    "{} {:?}; handler {}{}",
                    if d.binary == KERNEL {
                        "kernel mode, numbers"
                    } else {
                        "Linux server, numbers"
                    },
                    used.numbers,
                    source::called(&arm.body).unwrap_or_default(),
                    if refused > 0 {
                        format!("; refused {refused} times")
                    } else {
                        String::new()
                    }
                ),
                lines: lines.max(1),
                bytes,
            });
        }
    }

    // D, and the kept programs: every program in the kit.
    let bins = cfg.kit.join("node-user-bins");
    let mut names: Vec<String> = fs::read_dir(&bins)
        .map_err(|e| format!("{}: {e}", bins.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    for name in names {
        let sized = cfg
            .prod_kit
            .as_ref()
            .map(|k| k.join("node-user-bins").join(&name))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| bins.join(&name));
        let bytes = read(&sized).map(|b| loaded_bytes(&b)).unwrap_or(0);
        let lines = match exports.get(&name) {
            Some(e) => own_lines(e).1,
            None => zero_lines(cfg, &name, tools).unwrap_or(0),
        };
        let use_ = all.programs.get(&name);
        let status = match use_ {
            None if !started.contains(&name) => "never started".to_string(),
            None => "started, no dump".to_string(),
            Some(u) if u.wakes == 0 => format!(
                "started ({} processes), handled nothing",
                u.processes.values().sum::<u64>()
            ),
            Some(u) => format!(
                "started ({} processes), handled {} events, ran {} of {} lines",
                u.processes.values().sum::<u64>(),
                u.wakes,
                u.lines.0,
                u.lines.1
            ),
        };
        let item = Item {
            group: if KEEP_PROGRAMS.contains(&name.as_str()) {
                "program".into()
            } else {
                "D".into()
            },
            name: name.clone(),
            detail: status,
            lines,
            bytes,
        };
        if KEEP_PROGRAMS.contains(&name.as_str()) {
            keep_items.push(item);
        } else if use_.is_none_or(|u| u.wakes == 0) {
            unused.push(item);
        }
    }

    let order = |g: &str| {
        GROUPS
            .iter()
            .position(|(l, _)| *l == g)
            .unwrap_or(GROUPS.len())
    };
    unused.sort_by_key(|i| order(&i.group));
    for (letter, _) in GROUPS {
        let start = unused
            .iter()
            .position(|i| i.group == letter)
            .unwrap_or(unused.len());
        let end = unused
            .iter()
            .rposition(|i| i.group == letter)
            .map_or(start, |e| e + 1);
        sort(&mut unused[start..end]);
    }
    sort(&mut keep_items);
    Ok((unused, keep_items))
}

/// The lines of a program no job ran, from an export under a profile of zero counters.
fn zero_lines(cfg: &Config, name: &str, tools: &dyn Tools) -> Result<u64, String> {
    let elf_path = binary(cfg, name, None);
    let bytes = read(&elf_path)?;
    let elf = Elf::parse(&bytes).map_err(|e| e.to_string())?;
    let layout = profraw::counters(&elf).map_err(|e| e.to_string())?;
    let raw = profraw::build(&elf, &vec![0u8; layout.size as usize]).map_err(|e| e.to_string())?;
    let dir = cfg.out.join("zero");
    let raw_path = dir.join(format!("{name}.profraw"));
    write(&raw_path, &raw)?;
    let indexed = dir.join(format!("{name}.profdata"));
    tools.merge(&[raw_path], &indexed)?;
    Ok(own_lines(&tools.export(&elf_path, &indexed)?).1)
}

/// Builds the whole report: each job, then all of them, then the candidates.
pub fn build(cfg: &Config, tools: &dyn Tools) -> Result<Report, String> {
    let sources = Sources::load(&cfg.src);
    if sources.texts.is_empty() {
        return Err(format!(
            "{}: not the usage build's source tree",
            cfg.src.display()
        ));
    }
    let mut report = Report::default();
    let mut all_profiles: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut all_dumps: Vec<dump::Dump> = Vec::new();
    let mut all_linux = Vec::new();
    let mut started = BTreeSet::new();
    let mut dumps = (0, 0);
    let mut kernel: Option<Vec<u8>> = None;
    // Binary name to the functions compiled into it.
    let mut image: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (label, dir) in &cfg.runs {
        // One kernel for every job, or the merged profile would mix two builds.
        let this = read(&dir.join("kernel.elf"))?;
        match &kernel {
            Some(first) if *first != this => {
                return Err(format!(
                    "{label}: its kernel is not the first job's: one usage build per report"
                ));
            }
            Some(_) => {}
            None => kernel = Some(this),
        }
        let ev = evidence(cfg, label, dir)?;
        let mut exports = BTreeMap::new();
        for (name, raw) in &ev.profiles {
            let indexed = raw.with_extension("profdata");
            tools.merge(std::slice::from_ref(raw), &indexed)?;
            exports.insert(
                name.clone(),
                tools.export(&binary(cfg, name, Some(dir)), &indexed)?,
            );
            all_profiles
                .entry(name.clone())
                .or_default()
                .push(raw.clone());
            if !image.contains_key(name) {
                image.insert(name.clone(), tools.functions(&indexed)?);
            }
        }
        let mut s = scope(
            &ev.label,
            &exports,
            &image,
            &ev.programs,
            &ev.linux,
            &sources,
        );
        s.dumps = ev.dumps;
        s.broken_dumps = ev.broken;
        dumps = (dumps.0 + ev.dumps, dumps.1 + ev.broken);
        started.extend(ev.programs.keys().cloned());
        // Each process's dumps are its own; the sum over jobs is a sum over processes.
        let log = String::from_utf8_lossy(&read(&dir.join("serial.log"))?).into_owned();
        all_dumps.extend(dump::latest(&dump::decode(&log).dumps).into_iter().cloned());
        all_linux.extend(ev.linux);
        report.jobs.push(s);
    }
    let mut exports = BTreeMap::new();
    for (name, raws) in &all_profiles {
        let indexed = cfg.out.join("all").join(format!("{name}.profdata"));
        if let Some(dir) = indexed.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        tools.merge(raws, &indexed)?;
        let job_dir = cfg.runs.first().map(|(_, d)| d.as_path());
        exports.insert(
            name.clone(),
            tools.export(&binary(cfg, name, job_dir), &indexed)?,
        );
    }
    let programs = named(dump::programs(&all_dumps)?, &kit_index(&cfg.kit)?)?;
    report.all = scope(
        "all jobs", &exports, &image, &programs, &all_linux, &sources,
    );
    (report.all.dumps, report.all.broken_dumps) = dumps;
    let (unused, keep) = candidates(
        cfg,
        &exports,
        &image,
        &report.all,
        &sources,
        tools,
        &started,
        &mut report.notes,
    )?;
    report.unused = unused;
    report.keep = keep;
    report.notes.push(
        "Reachability follows direct calls from the user-mode loop; code reached only through a \
         pointer is listed under E though user space may reach it."
            .into(),
    );
    Ok(report)
}
