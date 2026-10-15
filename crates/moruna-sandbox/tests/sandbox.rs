//! SB-T11 to SB-T20 (17 k): a `.pex` stage's child, for real. Linux with a sandbox host only:
//! `MORUNA_SANDBOX_PYTHON` names a standard-build CPython 3.14 with `moruna` and `pyarrow`, and
//! `MORUNA_SANDBOX_CGROUP` a cgroup v2 directory with `memory` and `cpu` delegated (test
//! configuration, never the runtime's: 17 e.6). Each test is tagged and runs with `--ignored`
//! where those exist (a privileged container).
#![cfg(target_os = "linux")]
// The contract's error is the contract's size (contracts d.14).
#![allow(clippy::result_large_err)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::arrow::array::{Array, Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, InitCtx, Kernel, KernelState, MorunaError, Payload, Sample, Sampler, Tier,
    take_kernel_alloc,
};
use moruna_package::{Inputs, Package};
use moruna_sandbox::{SandboxHost, SandboxKernel, SandboxState};
use moruna_testkit::{FakeAllocator, FakeSampler};
use serde_json::{Value, json};

const TAG: &str = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP";

static SCRATCH: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-sandbox-test-{}-{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn host() -> SandboxHost {
    SandboxHost {
        python: PathBuf::from(std::env::var("MORUNA_SANDBOX_PYTHON").expect(TAG)),
        cgroup: PathBuf::from(std::env::var("MORUNA_SANDBOX_CGROUP").expect(TAG)),
    }
}

fn target() -> String {
    let arch = std::env::consts::ARCH;
    format!("cp314-manylinux_2_28_{arch}")
}

/// A `.pex` of `function.py` (and `deps` files under its target) with `hints`.
fn pex(
    dir: &Path,
    name: &str,
    function: &str,
    hints: Value,
    deps: &[(&str, &str)],
) -> Arc<Package> {
    let manifest = json!({
        "name": name,
        "entry": format!("function:{name}"),
        "declaration": {
            "input_schema": {"n": "int64", "s": "string"},
            "output_schema": {"adds": {}},
            "hints": hints,
        },
        "abi": moruna_kernel::declare::ABI_VERSION,
        "moruna": moruna_package::MORUNA_VERSION,
        "format": "pex",
        "platforms": [target()],
    });
    let path = dir.join(format!("{name}.pex"));
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("moruna.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("src/function.py", opts).unwrap();
    zip.write_all(function.as_bytes()).unwrap();
    for (file, text) in deps {
        zip.start_file(format!("deps/{}/{file}", target()), opts)
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    Arc::new(Package::open(&path).unwrap())
}

fn sampler(anon: u64, ceiling: u64) -> Arc<dyn Sampler> {
    Arc::new(FakeSampler::new().scripted(vec![Sample {
        anon_bytes: anon,
        ceiling_bytes: ceiling,
        cpu_limit: 2.0,
        ..Sample::default()
    }]))
}

const GIB: u64 = 1 << 30;

fn kernel(package: Arc<Package>, ceiling: u64) -> SandboxKernel {
    let k = SandboxKernel::new(package, host(), Inputs::default()).unwrap();
    k.bind_run(sampler(0, ceiling), ceiling);
    k
}

fn alloc() -> Arc<dyn Allocator> {
    Arc::new(FakeAllocator::new())
}

fn ctx() -> InitCtx {
    InitCtx {
        instance: 0,
        device: None,
        alloc: alloc(),
    }
}

fn batch() -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, true),
            Field::new("s", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), None, Some(3)])),
            Arc::new(StringArray::from(vec!["a", "bb", "ccc"])),
        ],
    )
    .unwrap()
}

fn call(k: &SandboxKernel, state: &mut dyn KernelState) -> moruna_kernel::Result<RecordBatch> {
    match k.apply(state, Payload::Table(batch(), Tier::Host))? {
        Payload::Table(out, _) => Ok(out),
        Payload::Tensor(..) => panic!("a table"),
    }
}

fn strings(b: &RecordBatch, column: &str) -> Vec<String> {
    let a = b
        .column_by_name(column)
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .clone();
    (0..a.len()).map(|i| a.value(i).to_string()).collect()
}

fn pid_of(state: &mut Box<dyn KernelState>) -> u32 {
    state
        .as_any_mut()
        .downcast_mut::<SandboxState>()
        .unwrap()
        .pid()
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t11_pex_identity() {
    let dir = scratch();
    let k = kernel(
        pex(
            &dir,
            "same",
            "def same(batch):\n    return batch\n",
            json!({}),
            &[],
        ),
        2 * GIB,
    );
    assert!(matches!(
        k.kind(),
        moruna_kernel::KernelKind::Stateful { max_instances } if max_instances.get() == 1
    ));
    let mut state = k.init(&ctx()).unwrap();
    let out = call(&k, state.as_mut()).unwrap();
    assert_eq!(out, batch());
    let counts = take_kernel_alloc();
    assert!(counts.measured && counts.refusal_on, "{counts:?}");
    let pid = pid_of(&mut state);
    assert_ne!(pid, std::process::id());
    // Many calls, one child.
    for _ in 0..20 {
        assert_eq!(call(&k, state.as_mut()).unwrap(), batch());
    }
    assert_eq!(pid_of(&mut state), pid);
    assert!(state.footprint().unwrap() > 0);
    assert_eq!(state.checkpoint().unwrap(), None);
    assert!(format!("{k:?}").contains("SandboxKernel"));
    assert_eq!(k.target(), target());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t12_confined_reads() {
    let dir = scratch();
    let secret = dir.join("job.json");
    std::fs::write(&secret, "{}").unwrap();
    let function = format!(
        r#"
import pyarrow as pa

PATHS = ["/etc/hostname", "/proc/self/status", {secret:?}, "/work/Cargo.toml", "/sys/fs/cgroup/cgroup.procs", "/pkg/src/function.py"]

def probe(batch):
    out = []
    for p in PATHS:
        try:
            with open(p, "rb") as f:
                f.read(1)
            out.append(p + ":read")
        except Exception as e:
            out.append(p + ":" + type(e).__name__)
    try:
        open("/pkg/src/new.py", "w")
        out.append("/pkg:written")
    except Exception as e:
        out.append("/pkg:" + type(e).__name__)
    with open("/tmp/scratch", "w") as f:
        f.write("ok")
    out.append("/tmp:written")
    return pa.record_batch({{"r": out}})
"#,
        secret = secret.display().to_string()
    );
    let k = kernel(pex(&dir, "probe", &function, json!({}), &[]), 2 * GIB);
    let mut state = k.init(&ctx()).unwrap();
    let out = strings(&call(&k, state.as_mut()).unwrap(), "r");
    let said = |prefix: &str| out.iter().find(|r| r.starts_with(prefix)).unwrap().clone();
    for hidden in [
        "/etc/hostname",
        "/proc/self/status",
        secret.display().to_string().as_str(),
        "/work/Cargo.toml",
        "/sys/fs/cgroup/cgroup.procs",
    ] {
        let r = said(hidden);
        assert!(
            r.ends_with(":FileNotFoundError") || r.ends_with(":PermissionError"),
            "{r}"
        );
    }
    assert!(said("/pkg/src/function.py").ends_with(":read"));
    assert!(
        !said("/pkg:").ends_with(":written"),
        "the tree is read only"
    );
    assert_eq!(said("/tmp:"), "/tmp:written");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t13_no_socket() {
    let dir = scratch();
    let function = r#"
import os, socket
import pyarrow as pa

def net(batch):
    out = []
    for name, act in [
        ("socket", lambda: socket.socket()),
        ("socketpair", lambda: socket.socketpair()),
        ("exec", lambda: os.execv("/bin/sh", ["sh", "-c", "true"])),
        ("kill", lambda: os.kill(1, 0)),
        ("unshare", lambda: os.unshare(os.CLONE_NEWNS)),
    ]:
        try:
            act()
            out.append(name + ":done")
        except Exception as e:
            out.append(name + ":" + type(e).__name__)
    return pa.record_batch({"r": out})
"#;
    let k = kernel(pex(&dir, "net", function, json!({}), &[]), 2 * GIB);
    let mut state = k.init(&ctx()).unwrap();
    let out = strings(&call(&k, state.as_mut()).unwrap(), "r");
    for r in &out {
        assert!(
            r.ends_with(":PermissionError") || r.ends_with(":FileNotFoundError"),
            "every one is refused: {out:?}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t14_guard_in_the_child() {
    let dir = scratch();
    let function = r#"
import numpy as np

def greedy(batch):
    np.empty(512 * 1024 * 1024, dtype=np.uint8)
    return batch
"#;
    // A 256 MiB stage ceiling: a 512 MiB request is refused by the child's own gate.
    let k = kernel(pex(&dir, "greedy", function, json!({}), &[]), 256 << 20);
    let mut state = k.init(&ctx()).unwrap();
    let e = call(&k, state.as_mut()).unwrap_err();
    let counts = take_kernel_alloc();
    match e {
        MorunaError::Refused {
            kernel,
            requested,
            ceiling,
            ..
        } => {
            assert_eq!(kernel, "greedy");
            assert_eq!(requested, 512 << 20);
            assert_eq!(ceiling, 256 << 20);
        }
        other => panic!("expected the child's refusal, got {other}"),
    }
    assert!(counts.numpy.refused >= 1, "{counts:?}");
    assert!(counts.refusal_on);
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    assert!(st.peak() > 0, "the counters page's peak rose");
    // memory_guard false: counted, not refused by the gate; the cgroup is the line.
    let k = kernel(
        pex(
            &dir,
            "unguarded",
            &function.replace("greedy", "unguarded"),
            json!({"memory_guard": false}),
            &[],
        ),
        1 << 30,
    );
    let mut state = k.init(&ctx()).unwrap();
    call(&k, state.as_mut()).unwrap();
    let counts = take_kernel_alloc();
    assert!(!counts.refusal_on);
    assert!(counts.numpy.bytes >= 512 << 20, "{counts:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t15_cgroup_is_the_hard_line() {
    let dir = scratch();
    let k = kernel(
        pex(
            &dir,
            "same",
            "def same(batch):\n    return batch\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    let (memory, cpu) = st.cgroup_limits().unwrap();
    assert_eq!(memory, GIB.to_string());
    assert_eq!(cpu, "200000 100000");
    let cgroup = st.cgroup();
    assert!(cgroup.is_dir());
    // The limits moved: the ceiling is sent again.
    k.bind_run(sampler(0, GIB / 2), GIB / 2);
    k.limits_changed();
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    assert_eq!(st.cgroup_limits().unwrap().0, (GIB / 2).to_string());
    drop(state);
    assert!(
        !cgroup.exists(),
        "the stage's cgroup is gone with its child"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t16_dead_child() {
    let dir = scratch();
    let k = kernel(
        pex(
            &dir,
            "quits",
            "import os\n\ndef quits(batch):\n    os._exit(3)\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let pid = pid_of(&mut state);
    let e = call(&k, state.as_mut()).unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains(&pid.to_string()) && text.contains("exit status 3"),
        "{text}"
    );
    let again = call(&k, state.as_mut()).unwrap_err().to_string();
    assert!(again.contains("exit status 3"), "{again}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t17_sampler_sees_the_child() {
    let dir = scratch();
    let function = r#"
KEEP = []

def holds(batch):
    if not KEEP:
        KEEP.append(bytearray(64 * 1024 * 1024))
    return batch
"#;
    let k = kernel(pex(&dir, "holds", function, json!({}), &[]), 2 * GIB);
    let outside = k.outside();
    assert_eq!(outside.in_use(), 0, "no child yet");
    let mut state = k.init(&ctx()).unwrap();
    let before = outside.in_use();
    call(&k, state.as_mut()).unwrap();
    let after = outside.in_use();
    assert!(after >= before + (60 << 20), "{before} then {after}");
    drop(state);
    assert_eq!(outside.in_use(), 0, "the child is gone");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t18_library_conflict() {
    let dir = scratch();
    let package = pex(
        &dir,
        "old",
        "def old(batch):\n    return batch\n",
        json!({}),
        &[(
            "pyarrow-0.0.1.dist-info/METADATA",
            "Name: pyarrow\nVersion: 0.0.1\n",
        )],
    );
    let k = kernel(package, GIB);
    let e = k.init(&ctx()).err().expect("refused");
    let text = e.to_string();
    assert!(
        text.contains("pyarrow 0.0.1") && text.contains("imported pyarrow"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t19_template_threads() {
    let dir = scratch();
    let wrapper = dir.join("threaded-python");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec {} -I -c 'import threading, time, runpy\nthreading.Thread(target=time.sleep, args=(60,), daemon=True).start()\nrunpy.run_module(\"moruna._sandbox\", run_name=\"__main__\")'\n",
            host().python.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let package = pex(
        &dir,
        "same",
        "def same(batch):\n    return batch\n",
        json!({}),
        &[],
    );
    let e = SandboxKernel::new(
        package,
        SandboxHost {
            python: wrapper,
            cgroup: host().cgroup,
        },
        Inputs::default(),
    )
    .expect_err("refused");
    let text = e.to_string();
    assert!(
        text.contains("threads") && text.contains("SB-I10"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t20_pex_state_crosses_a_checkpoint() {
    let dir = scratch();
    let function = r#"
class counter:
    def setup(self, ctx):
        return {"n": 0}

    def __call__(self, state, batch):
        state["n"] += 1
        return batch

    def checkpoint(self, state):
        return str(state["n"]).encode()

    def restore(self, ctx, data):
        return {"n": int(data.decode())}
"#;
    let k = kernel(
        pex(
            &dir,
            "counter",
            function,
            json!({"stateful": true, "resume": "checkpoint"}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    for _ in 0..3 {
        call(&k, state.as_mut()).unwrap();
    }
    let saved = state.checkpoint().unwrap().unwrap();
    assert_eq!(saved, b"3");
    let mut restored = k.restore(&ctx(), &saved).unwrap();
    call(&k, restored.as_mut()).unwrap();
    assert_eq!(restored.checkpoint().unwrap().unwrap(), b"4");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn the_child_refuses_what_it_cannot_load() {
    let dir = scratch();
    // An entry that does not import, a decorated entry, a kernel that raises.
    let k = kernel(
        pex(
            &dir,
            "missing",
            "import no_such_module\n\ndef missing(b):\n    return b\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let e = k.init(&ctx()).err().expect("refused").to_string();
    assert!(
        e.contains("import function") && e.contains("no_such_module"),
        "{e}"
    );
    let k = kernel(
        pex(
            &dir,
            "decorated",
            "import moruna\n\n@moruna.kernel\ndef decorated(b):\n    return b\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let e = k.init(&ctx()).err().expect("refused").to_string();
    assert!(e.contains("KernelSpec"), "{e}");
    let k = kernel(
        pex(
            &dir,
            "raises",
            "def raises(b):\n    raise ValueError('nope')\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let e = call(&k, state.as_mut()).unwrap_err().to_string();
    assert!(e.contains("ValueError: nope"), "{e}");
    let k = kernel(
        pex(
            &dir,
            "wrong",
            "def wrong(b):\n    return 7\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let e = call(&k, state.as_mut()).unwrap_err().to_string();
    assert!(e.contains("RecordBatch"), "{e}");
    // A table of one batch is a batch.
    let k = kernel(
        pex(
            &dir,
            "table",
            "import pyarrow as pa\n\ndef table(b):\n    return pa.Table.from_batches([b])\n",
            json!({}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    assert_eq!(call(&k, state.as_mut()).unwrap(), batch());
    // An unbound stage is refused before a child starts.
    let unbound = SandboxKernel::new(
        pex(
            &dir,
            "unbound",
            "def unbound(b):\n    return b\n",
            json!({}),
            &[],
        ),
        host(),
        Inputs::default(),
    )
    .unwrap();
    assert!(
        unbound
            .init(&ctx())
            .err()
            .unwrap()
            .to_string()
            .contains("no ceiling")
    );
    assert!(
        unbound.restore(&ctx(), b"").is_err(),
        "not a checkpointing package"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn pk_t17_args_and_assets_in_the_child() {
    let dir = scratch();
    let assets = dir.join("model");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("weights.txt"), "42").unwrap();
    let function = r#"
import pyarrow as pa

def predict(args):
    with open("/assets/weights.txt") as f:
        weight = f.read()
    try:
        open("/assets/new.txt", "w")
        wrote = "written"
    except Exception as e:
        wrote = type(e).__name__
    def kernel(batch):
        return pa.record_batch({"r": [args["label"], weight, wrote]})
    return kernel
"#;
    let package = pex(&dir, "predict", function, json!({"args": true}), &[]);
    let inputs = Inputs {
        args: Some(json!({"label": "churn"})),
        assets: Some(assets.clone()),
    };
    let k = SandboxKernel::new(package.clone(), host(), inputs).unwrap();
    k.bind_run(sampler(0, GIB), GIB);
    let mut state = k.init(&ctx()).unwrap();
    let out = strings(&call(&k, state.as_mut()).unwrap(), "r");
    assert_eq!(out[0], "churn");
    assert_eq!(out[1], "42");
    assert!(
        out[2] == "PermissionError" || out[2] == "OSError",
        "{out:?}"
    );
    assert!(!assets.join("new.txt").exists());
    // Arguments for a package not built by them are refused before a child starts.
    let plain = pex(
        &dir,
        "same",
        "def same(batch):\n    return batch\n",
        json!({}),
        &[],
    );
    let e = SandboxKernel::new(
        plain,
        host(),
        Inputs {
            args: Some(json!({})),
            assets: None,
        },
    )
    .expect_err("refused");
    assert!(e.to_string().contains("args"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}
