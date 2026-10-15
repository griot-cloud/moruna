//! The child's runner (17 f.6): in a fork of the template, confine, load the package under the
//! adapter with the guard bound to the child's own memory and the ceiling Moruna writes, then
//! serve calls whose batches are read in place from the mapping (SB-I7, SB-I8).

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moruna_adapters::guard::MemoryGate;
use moruna_adapters::{PyKernel, PyKernelSpec};
use moruna_kernel::{
    InitCtx, Kernel, KernelAlloc, KernelState, MorunaError, NoState, PayloadKind, PayloadSpec,
    ProcessPeak, ResumePolicy, Sample, Sampler, TierPref,
};
use moruna_package::Manifest;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use crate::confine::{self, Confinement, TREE};
use crate::mapping::{Mapping, field};
use crate::protocol::{
    AllocDoc, FromChild, RefusalDoc, ToChild, ToTemplate, read_frame, write_frame,
};

/// The arena a stateful kernel's `setup` is given a context from: the child lands nothing in
/// it, Moruna does.
const CHILD_ARENA_BYTES: u64 = 1 << 20;

/// The child's sampler (17 f.6): its cgroup's `memory.current`, through the descriptor kept from
/// before the namespace hid the file, and the stage ceiling from the counters page.
struct ChildSampler {
    current: File,
    mapping: Arc<Mapping>,
    peak: AtomicU64,
}

impl ChildSampler {
    fn read_current(&self) -> u64 {
        let mut buf = [0u8; 32];
        let n = self.current.read_at(&mut buf, 0).unwrap_or(0);
        std::str::from_utf8(&buf[..n])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

impl Sampler for ChildSampler {
    fn sample(&self) -> Sample {
        let anon = self.read_current();
        let peak = self.peak.fetch_max(anon, Ordering::AcqRel).max(anon);
        self.mapping
            .counter(field::IN_USE)
            .store(anon, Ordering::Release);
        self.mapping
            .counter(field::PEAK)
            .fetch_max(peak, Ordering::AcqRel);
        Sample {
            anon_bytes: anon,
            peak_anon_bytes: peak,
            ceiling_bytes: self.mapping.counter(field::CEILING).load(Ordering::Acquire),
            ..Sample::default()
        }
    }

    fn reset_peak(&self) {}

    fn process_peak(&self) -> ProcessPeak {
        ProcessPeak {
            bytes: self.peak.load(Ordering::Acquire),
            ..ProcessPeak::default()
        }
    }
}

fn failed(step: &str, e: impl std::fmt::Display) -> String {
    format!("{step}: {e}")
}

/// `_core._sandbox_child(start, control_fd, memfd)`: the forked child's whole life; returns its
/// exit code. Every failure before `ready` is sent as `failed`, naming the step.
#[pyfunction]
#[pyo3(name = "_sandbox_child")]
pub fn sandbox_child(py: Python<'_>, start: &str, control_fd: i32, memfd: i32) -> i32 {
    // SAFETY: the template received these two descriptors for this child with the `start`
    // request and gave its own copies up after the fork; this process now owns them.
    let control = unsafe { UnixStream::from_raw_fd(control_fd) };
    // SAFETY: as above.
    let memfd = unsafe { OwnedFd::from_raw_fd(memfd) };
    crate::coverage::open();
    let code = match run(py, start, &control, memfd) {
        Ok(()) => 0,
        Err(reason) => {
            let mut writer = &control;
            let _ = write_frame(&mut writer, &FromChild::Failed { reason });
            3
        }
    };
    // The template ends the child through `os._exit`.
    crate::coverage::write();
    code
}

/// The interpreter's import roots: every absolute directory on `sys.path`, and the standard
/// library's and site's directories.
fn import_roots(py: Python<'_>) -> PyResult<Vec<PathBuf>> {
    let code = c"import os, sys, sysconfig\nroots = [p for p in list(sys.path) + list(sysconfig.get_paths().values()) if p and os.path.isabs(p) and os.path.isdir(p)]\n";
    let globals = pyo3::types::PyDict::new(py);
    py.run(code, Some(&globals), None)?;
    let roots: Vec<String> = globals
        .get_item("roots")?
        .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("no roots"))?
        .extract()?;
    Ok(roots.into_iter().map(PathBuf::from).collect())
}

/// A distribution name as PEP 503 normalises it.
fn normal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !dash {
                out.push('-');
            }
            dash = true;
        } else {
            out.push(c.to_ascii_lowercase());
            dash = false;
        }
    }
    out
}

/// The distributions installed in `deps`: `(name, version)` from each `*.dist-info/METADATA`.
pub fn distributions(deps: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(deps) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dist-info") {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(path.join("METADATA")) else {
            continue;
        };
        let field = |key: &str| {
            meta.lines()
                .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
        };
        if let (Some(name), Some(version)) = (field("Name:"), field("Version:")) {
            out.push((name, version));
        }
    }
    out.sort();
    out
}

/// 17 f.6: a package may not carry, at another version, a library the template imported.
pub fn conflicts(deps: &Path, libraries: &BTreeMap<String, String>) -> Result<(), String> {
    for (name, version) in distributions(deps) {
        if let Some((_, imported)) = libraries.iter().find(|(l, _)| normal(l) == normal(&name))
            && imported != &version
        {
            return Err(format!(
                "the package carries {name} {version} and the sandbox's interpreter imported {name} \
                 {imported} before the fork; pin {name}=={imported} or leave it out of the lockfile"
            ));
        }
    }
    Ok(())
}

/// Import `entry` from the tree and make the adapter's kernel of it with the manifest's
/// declaration and hints.
fn load(
    py: Python<'_>,
    deps: &Path,
    entry: &str,
    manifest: &Manifest,
    args: Option<&serde_json::Value>,
) -> Result<PyKernel, String> {
    let sys = py.import("sys").map_err(|e| failed("load", e))?;
    let path = sys.getattr("path").map_err(|e| failed("load", e))?;
    for dir in [deps.to_path_buf(), Path::new(TREE).join("src")] {
        path.call_method1("insert", (0, dir.display().to_string()))
            .map_err(|e| failed("load", e))?;
    }
    let (module, attribute) = entry
        .split_once(':')
        .ok_or_else(|| format!("load: `{entry}` is not <module>:<attribute>"))?;
    let mut target = py
        .import(module)
        .map_err(|e| failed(&format!("import {module}"), e))?
        .into_any();
    for part in attribute.split('.') {
        target = target.getattr(part).map_err(|e| failed(entry, e))?;
    }
    let type_name = target
        .get_type()
        .qualname()
        .map(|q| q.to_string())
        .unwrap_or_default();
    if matches!(type_name.as_str(), "KernelSpec" | "StdKernel") {
        return Err(format!(
            "load: `{entry}` is a {type_name}; a package declares its kernel in function.yaml, \
             so its entry is the plain function or class"
        ));
    }
    let hints = manifest.hints();
    // 17 f.11: a kernel built by arguments is made by its entry, called once with them.
    if hints.args {
        let json = py.import("json").map_err(|e| failed("load", e))?;
        let text = args.map_or_else(|| "{}".to_string(), serde_json::Value::to_string);
        let given = json
            .call_method1("loads", (text,))
            .map_err(|e| failed("args", e))?;
        target = target
            .call1((given,))
            .map_err(|e| failed(&format!("{entry}(args)"), e))?;
    }
    let declare = py
        .import("moruna._declare")
        .map_err(|e| failed("load", e))?;
    let mut origin = None;
    if hints.stateful {
        if target.is_instance_of::<pyo3::types::PyType>() {
            target = target
                .call0()
                .map_err(|e| failed(&format!("{entry}()"), e))?;
        }
    } else if let Some(kind) = declare
        .call_method1("polars_signature", (&target,))
        .map_err(|e| failed("load", e))?
        .extract::<Option<String>>()
        .map_err(|e| failed("load", e))?
    {
        origin = Some(target.clone().unbind());
        target = declare
            .call_method1("polars_callable", (&target, kind))
            .map_err(|e| failed("load", e))?;
    }
    let mut spec = PyKernelSpec::new(target.unbind());
    spec.stateful = hints.stateful;
    spec.accepts = PayloadSpec {
        kind: PayloadKind::Table,
        tier: TierPref::Host,
    };
    spec.expected_amplification = hints.expected_amplification;
    spec.preferred_rows = hints.preferred_rows;
    spec.state_bytes = hints.state_bytes;
    spec.resume = match hints.resume.as_deref() {
        Some("checkpoint") => ResumePolicy::Checkpoint,
        Some("forbid") => ResumePolicy::Forbid,
        _ => ResumePolicy::Reinit,
    };
    spec.declared = manifest.declared().map_err(|e| e.to_string())?;
    spec.origin = origin;
    spec.memory_guard = hints.memory_guard;
    PyKernel::new(spec).map_err(|e| failed("load", e))
}

fn run(py: Python<'_>, start: &str, control: &UnixStream, memfd: OwnedFd) -> Result<(), String> {
    let start: ToTemplate = serde_json::from_str(start).map_err(|e| failed("start", e))?;
    let ToTemplate::Start {
        tree,
        target,
        entry,
        manifest,
        cgroup,
        root,
        mapping_bytes,
        libraries,
        args,
        assets,
    } = start
    else {
        return Err("start: not a start request".into());
    };
    let current = confine::join_cgroup(Path::new(&cgroup)).map_err(|e| e.to_string())?;
    let roots = import_roots(py).map_err(|e| failed("roots", e))?;
    confine::confine(&Confinement {
        cgroup: PathBuf::from(&cgroup),
        root: PathBuf::from(&root),
        tree: PathBuf::from(&tree),
        roots,
        assets: assets.map(PathBuf::from),
    })
    .map_err(|e| e.to_string())?;
    let mapping =
        Arc::new(Mapping::open(memfd, mapping_bytes as usize).map_err(|e| failed("mapping", e))?);
    let manifest = Manifest::from_value(&manifest).map_err(|e| failed("manifest", e))?;
    let deps = Path::new(TREE).join("deps").join(&target);
    conflicts(&deps, &libraries)?;
    let kernel = load(py, &deps, &entry, &manifest, args.as_ref())?;
    let sampler = Arc::new(ChildSampler {
        current,
        mapping: Arc::clone(&mapping),
        peak: AtomicU64::new(0),
    });
    let ceiling = mapping.counter(field::CEILING).load(Ordering::Acquire);
    kernel.bind_gate(MemoryGate::new(
        Arc::clone(&sampler) as Arc<dyn Sampler>,
        ceiling,
    ));
    let arena = moruna_arena::Arena::new(moruna_arena::ArenaConfig {
        host_bytes: CHILD_ARENA_BYTES,
        ..moruna_arena::ArenaConfig::default()
    })
    .map_err(|e| failed("arena", e))?;
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: arena,
    };
    let mut state: Box<dyn KernelState> = if manifest.hints().stateful {
        kernel.new_state(&ctx).map_err(|e| failed("setup", e))?
    } else {
        Box::new(NoState)
    };
    let mut writer = control;
    write_frame(&mut writer, &FromChild::Ready).map_err(|e| failed("ready", e))?;
    let mut child = Served {
        mapping,
        sampler,
        kernel,
        ctx,
        released: Arc::new(Mutex::new(Vec::new())),
    };
    loop {
        let message = py.detach(|| {
            let mut reader = control;
            read_frame::<ToChild>(&mut reader)
        });
        let reply = match message {
            Err(_) | Ok(ToChild::Stop) => return Ok(()),
            Ok(ToChild::Call {
                lease,
                offset,
                len,
                out_offset,
                out_len,
            }) => child.call(
                py,
                state.as_mut(),
                lease,
                (offset as usize, len as usize),
                (out_offset as usize, out_len as usize),
            ),
            Ok(ToChild::Checkpoint { offset, len }) => {
                child.checkpoint(state.as_mut(), offset as usize, len as usize)
            }
            // A kernel's reference cycles hold leases until the collector runs.
            Ok(ToChild::Release) => match py.import("gc").and_then(|gc| gc.call_method0("collect"))
            {
                Ok(_) => FromChild::Released {
                    released: Vec::new(),
                },
                Err(e) => FromChild::Error {
                    message: format!("release: the collector failed: {}", e.value(py)),
                    refusal: None,
                    alloc: AllocDoc::default(),
                    released: Vec::new(),
                },
            },
            Ok(ToChild::Open { offset, len }) => match child.open(offset, len) {
                Ok(opened) => {
                    state = opened;
                    FromChild::Opened
                }
                Err(message) => FromChild::Error {
                    message,
                    refusal: None,
                    alloc: AllocDoc::default(),
                    released: Vec::new(),
                },
            },
        };
        // Every object of the call is gone by now, so its lease is in the list if the kernel
        // did not keep the batch (SB-I13).
        let reply = child.with_released(reply);
        let mut writer = control;
        write_frame(&mut writer, &reply).map_err(|e| failed("reply", e))?;
    }
}

/// What the child serves calls with.
struct Served {
    mapping: Arc<Mapping>,
    sampler: Arc<ChildSampler>,
    kernel: PyKernel,
    ctx: InitCtx,
    /// Leases whose last buffer was released since the last reply (SB-I13).
    released: Arc<Mutex<Vec<u64>>>,
}

/// An input region leased to the child (SB-I13): the base object of the foreign Arrow buffer
/// the input is read through. When the last buffer over the region is gone, Python drops it and
/// its id goes into the released list, which the next reply carries to Moruna.
#[pyclass(frozen, module = "moruna._core", name = "Lease")]
struct Lease {
    id: u64,
    released: Arc<Mutex<Vec<u64>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.id);
    }
}

/// A memoryview over `len` bytes of the mapping at `ptr`.
fn memoryview<'py>(
    py: Python<'py>,
    ptr: *mut u8,
    len: usize,
    write: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let flags = if write {
        pyo3::ffi::PyBUF_WRITE
    } else {
        pyo3::ffi::PyBUF_READ
    };
    // SAFETY: `ptr` is valid for `len` bytes of the mapping, which stays mapped for the child's
    // life (the `Arc<Mapping>` in `Served` outlives every call); the memoryview is a borrowed
    // view whose owner is the mapping, and the protocol gives these bytes to the child for the
    // length of the call (17 e.3).
    unsafe {
        let view =
            pyo3::ffi::PyMemoryView_FromMemory(ptr.cast(), len as pyo3::ffi::Py_ssize_t, flags);
        Bound::from_owned_ptr_or_err(py, view)
    }
}

fn message_of(e: &MorunaError) -> String {
    match e {
        MorunaError::Kernel { msg, .. } => msg.clone(),
        other => other.to_string(),
    }
}

impl Served {
    fn counted(&self, counts: KernelAlloc) -> AllocDoc {
        let _ = self.sampler.sample();
        self.mapping
            .counter(field::CALLS)
            .fetch_add(1, Ordering::AcqRel);
        AllocDoc::from(&counts)
    }

    /// `reply` with the leases released since the last reply.
    fn with_released(&self, reply: FromChild) -> FromChild {
        let ids: Vec<u64> = std::mem::take(
            &mut *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        match reply {
            FromChild::Done {
                offset, len, alloc, ..
            } => FromChild::Done {
                offset,
                len,
                alloc,
                released: ids,
            },
            FromChild::Error {
                message,
                refusal,
                alloc,
                ..
            } => FromChild::Error {
                message,
                refusal,
                alloc,
                released: ids,
            },
            FromChild::State { offset, len, .. } => FromChild::State {
                offset,
                len,
                released: ids,
            },
            FromChild::Released { .. } => FromChild::Released { released: ids },
            other => {
                // A reply that carries none keeps them for the next.
                self.released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(ids);
                other
            }
        }
    }

    fn call(
        &mut self,
        py: Python<'_>,
        state: &mut dyn KernelState,
        lease: u64,
        (offset, len): (usize, usize),
        (out_at, out_len): (usize, usize),
    ) -> FromChild {
        let error = |message: String, alloc: AllocDoc| FromChild::Error {
            message,
            refusal: None,
            alloc,
            released: Vec::new(),
        };
        let batch = match self.read(py, lease, offset, len) {
            Ok(batch) => batch,
            Err(e) => return error(format!("the input: {e}"), AllocDoc::default()),
        };
        let (result, counts) = self.kernel.call_arrow(py, state, batch);
        let alloc = self.counted(counts);
        match result {
            Ok(out) => match self.write(py, out, out_at, out_len) {
                Ok(written) => FromChild::Done {
                    offset: out_at as u64,
                    len: written,
                    alloc,
                    released: Vec::new(),
                },
                Err(e) => error(e, alloc),
            },
            Err(MorunaError::Refused {
                requested,
                in_use,
                ceiling,
                ..
            }) => FromChild::Error {
                message: format!(
                    "a request of {requested} bytes was refused: the child held {in_use} of its \
                     ceiling of {ceiling}"
                ),
                refusal: Some(RefusalDoc {
                    requested,
                    in_use,
                    ceiling,
                }),
                alloc,
                released: Vec::new(),
            },
            Err(e) => error(message_of(&e), alloc),
        }
    }

    /// The input stream, read in place: a pyarrow batch over a foreign buffer of the mapping's
    /// bytes whose base is the lease, so the region stays the input's for as long as anything
    /// in the child refers to it (SB-I13).
    fn read<'py>(
        &self,
        py: Python<'py>,
        lease: u64,
        offset: usize,
        len: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ptr = self
            .mapping
            .address(offset, len)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        let base = Bound::new(
            py,
            Lease {
                id: lease,
                released: Arc::clone(&self.released),
            },
        )?;
        let pa = py.import("pyarrow")?;
        let buffer = pa.call_method1("foreign_buffer", (ptr as usize, len, base))?;
        let reader = pa.getattr("ipc")?.call_method1("open_stream", (buffer,))?;
        reader.call_method0("read_next_batch")
    }

    /// The kernel's result as one IPC stream into the mapping at `at`, at most `cap` bytes; its
    /// length.
    fn write(
        &self,
        py: Python<'_>,
        out: Bound<'_, PyAny>,
        at: usize,
        cap: usize,
    ) -> Result<u64, String> {
        let inner = || -> PyResult<u64> {
            let pa = py.import("pyarrow")?;
            let mut out = out;
            if out.is_instance(&pa.getattr("Table")?)? {
                out = out.call_method0("combine_chunks")?;
                let batches = out.call_method0("to_batches")?;
                let n: usize = batches.len()?;
                if n != 1 {
                    return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                        "the kernel returned a table of {n} batches; a call returns one"
                    )));
                }
                out = batches.get_item(0)?;
            }
            if !out.is_instance(&pa.getattr("RecordBatch")?)? {
                let name = out.get_type().qualname()?.to_string();
                return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                    "a packaged kernel returns a pyarrow.RecordBatch, not {name}"
                )));
            }
            if cap == 0 {
                return Err(pyo3::exceptions::PyMemoryError::new_err(
                    "no free region of the mapping is left: the kernel holds its inputs",
                ));
            }
            let ptr = self
                .mapping
                .address(at, cap)
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
            let view = memoryview(py, ptr, cap, true)?;
            let sink = pa.call_method1(
                "FixedSizeBufferWriter",
                (pa.call_method1("py_buffer", (view,))?,),
            )?;
            let writer = pa
                .getattr("ipc")?
                .call_method1("new_stream", (&sink, out.getattr("schema")?))?;
            writer.call_method1("write_batch", (&out,))?;
            writer.call_method0("close")?;
            sink.call_method0("tell")?.extract()
        };
        inner().map_err(|e| {
            format!(
                "the output does not fit the mapping or is not a batch: {}",
                e.value(py)
            )
        })
    }

    fn checkpoint(&mut self, state: &mut dyn KernelState, at: usize, cap: usize) -> FromChild {
        let failed = |message: String| FromChild::Error {
            message,
            refusal: None,
            alloc: AllocDoc::default(),
            released: Vec::new(),
        };
        let saved = match state.checkpoint() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => Vec::new(),
            Err(e) => return failed(message_of(&e)),
        };
        if saved.len() > cap {
            return failed(format!(
                "the state of {} bytes does not fit the free region of {cap}",
                saved.len()
            ));
        }
        match self.mapping.bytes_mut(at, saved.len()) {
            Ok(area) => {
                area.copy_from_slice(&saved);
                FromChild::State {
                    offset: at as u64,
                    len: saved.len() as u64,
                    released: Vec::new(),
                }
            }
            Err(e) => failed(format!("the state does not fit the mapping: {e}")),
        }
    }

    fn open(&mut self, offset: u64, len: u64) -> Result<Box<dyn KernelState>, String> {
        let bytes = self
            .mapping
            .bytes(offset as usize, len as usize)
            .map_err(|e| e.to_string())?
            .to_vec();
        self.kernel
            .restore(&self.ctx, &bytes)
            .map_err(|e| message_of(&e))
    }
}

/// Registers the child's entry on the module (the template calls it after the fork).
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sandbox_child, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_conflicts_are_named() {
        let dir = std::env::temp_dir().join(format!("moruna-sb-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let info = dir.join("pyarrow-24.0.0.dist-info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("METADATA"), "Name: pyarrow\nVersion: 24.0.0\n").unwrap();
        let other = dir.join("Some_Lib-1.0.dist-info");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("METADATA"), "Name: Some_Lib\nVersion: 1.0\n").unwrap();
        assert_eq!(distributions(&dir).len(), 2);
        let mut libraries = BTreeMap::new();
        libraries.insert("pyarrow".to_string(), "25.0.1".to_string());
        let e = conflicts(&dir, &libraries).unwrap_err();
        assert!(
            e.contains("pyarrow 24.0.0") && e.contains("pyarrow 25.0.1"),
            "{e}"
        );
        libraries.insert("pyarrow".to_string(), "24.0.0".to_string());
        libraries.insert("some-lib".to_string(), "1.0".to_string());
        conflicts(&dir, &libraries).unwrap();
        assert!(distributions(&dir.join("missing")).is_empty());
        assert_eq!(normal("Some_Lib.x"), "some-lib-x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
