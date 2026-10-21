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
    call_with(k, state, batch())
}

fn call_with(
    k: &SandboxKernel,
    state: &mut dyn KernelState,
    input: RecordBatch,
) -> moruna_kernel::Result<RecordBatch> {
    match k.apply(state, Payload::Table(input, Tier::Host))? {
        Payload::Table(out, _) => Ok(out),
        Payload::Tensor(..) => panic!("a table"),
    }
}

/// A batch of the declared schema whose `s` is `words` and `n` counts.
fn words(words: &[&str]) -> RecordBatch {
    RecordBatch::try_new(
        batch().schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(0..words.len() as i64)),
            Arc::new(StringArray::from(words.to_vec())),
        ],
    )
    .unwrap()
}

/// A batch of `rows` rows, `rows * 13` bytes of buffers.
fn big(rows: usize) -> RecordBatch {
    RecordBatch::try_new(
        batch().schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(0..rows as i64)),
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                "x", rows,
            ))),
        ],
    )
    .unwrap()
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

#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn a_template_that_cannot_start_is_named() {
    use moruna_sandbox::template::Template;
    let e = Template::spawn(Path::new("/nonexistent/python3")).unwrap_err();
    assert!(e.contains("cannot start the sandbox template"), "{e}");
    let dir = scratch();
    // An interpreter that exits before its hello.
    let quits = dir.join("quits");
    std::fs::write(&quits, "#!/bin/sh\nexit 7\n").unwrap();
    std::fs::set_permissions(&quits, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let e = Template::spawn(&quits).unwrap_err();
    assert!(e.contains("did not say hello"), "{e}");
    // One whose moruna has no child runner, or no pyarrow: refused, named.
    let bare = dir.join("bare");
    std::fs::write(
        &bare,
        format!(
            "#!/bin/sh\nexec {} -I -c 'import socket, json, struct\ns = socket.socket(fileno=3)\nb = json.dumps({{\"op\": \"refused\", \"reason\": \"no pyarrow\"}}).encode()\ns.sendall(struct.pack(\"<I\", len(b)) + b)'\n",
            host().python.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bare, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let e = Template::spawn(&bare).unwrap_err();
    assert!(e.contains("refused to start: no pyarrow"), "{e}");
    // The running template answers a status for a pid that is not its child.
    let t = moruna_sandbox::template::template(&host().python).unwrap();
    assert!(t.alive());
    assert_eq!(t.status(1).unwrap(), "running");
    assert!(t.hello.target().unwrap().starts_with("cp314-manylinux_"));
    assert!(format!("{t:?}").contains("Template"));
    let _ = std::fs::remove_dir_all(dir);
}

/// SB-T21 (SB-I13): a kernel that keeps its input and reads it on the next call sees the first
/// batch's bytes, not the second's: the region it reads through is leased until it lets go.
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t21_a_kept_input_keeps_its_bytes() {
    let dir = scratch();
    let function = r#"
class keeper:
    def setup(self, ctx):
        return {"previous": None}

    def __call__(self, state, batch):
        previous = state["previous"]
        state["previous"] = batch
        return batch if previous is None else previous
"#;
    let k = kernel(
        pex(&dir, "keeper", function, json!({"stateful": true}), &[]),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let first = call_with(&k, state.as_mut(), words(&["first", "batch", "bytes"])).unwrap();
    assert_eq!(strings(&first, "s"), ["first", "batch", "bytes"]);
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    assert_eq!(st.leased().0, 1, "the kept input is leased");
    let second = call_with(&k, state.as_mut(), words(&["other", "words", "later"])).unwrap();
    assert_eq!(
        strings(&second, "s"),
        ["first", "batch", "bytes"],
        "the kept batch reads its own bytes"
    );
    let third = call_with(&k, state.as_mut(), words(&["third"])).unwrap();
    assert_eq!(strings(&third, "s"), ["other", "words", "later"]);
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    let (leases, bytes) = st.leased();
    assert_eq!(leases, 1, "the batch let go of was released");
    assert!(bytes > 0);
    assert!(
        state.footprint().unwrap() >= bytes,
        "the profile shows the mapping the kernel holds"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// SB-T22 (SB-I13): backpressure. A kernel that keeps one batch never waits; one whose kept
/// batches are garbage the collector frees waits for the release, counted as `lease_wait_us`;
/// one that keeps every batch is refused after `LEASE_WAIT`, naming the bytes it holds.
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t22_backpressure() {
    let dir = scratch();
    let ceiling = 256 << 20;
    // About 26 MiB of buffers a batch: nine fit the mapping.
    let rows = 2 << 20;
    let last = r#"
class last:
    def setup(self, ctx):
        return {"previous": None}

    def __call__(self, state, batch):
        state["previous"] = batch
        return batch.slice(0, 1)
"#;
    let k = kernel(
        pex(&dir, "last", last, json!({"stateful": true}), &[]),
        ceiling,
    );
    let mut state = k.init(&ctx()).unwrap();
    for _ in 0..30 {
        call_with(&k, state.as_mut(), big(rows)).unwrap();
        assert_eq!(take_kernel_alloc().lease_wait_us, 0);
    }
    drop(state);

    let cyclic = r#"
import gc

class cyclic:
    def setup(self, ctx):
        gc.disable()
        return None

    def __call__(self, state, batch):
        garbage = [batch]
        garbage.append(garbage)
        return batch.slice(0, 1)
"#;
    let k = kernel(
        pex(&dir, "cyclic", cyclic, json!({"stateful": true}), &[]),
        ceiling,
    );
    let mut state = k.init(&ctx()).unwrap();
    let mut waited = 0;
    for _ in 0..30 {
        call_with(&k, state.as_mut(), big(rows)).unwrap();
        waited += take_kernel_alloc().lease_wait_us;
    }
    assert!(waited > 0, "a call waited for the collector to release");
    drop(state);

    let hoarder = r#"
class hoarder:
    def setup(self, ctx):
        return []

    def __call__(self, state, batch):
        state.append(batch)
        return batch.slice(0, 1)
"#;
    let k = kernel(
        pex(&dir, "hoarder", hoarder, json!({"stateful": true}), &[]),
        ceiling,
    );
    let mut state = k.init(&ctx()).unwrap();
    let refused = (0..30).find_map(|_| call_with(&k, state.as_mut(), big(rows)).err());
    match refused {
        Some(MorunaError::Refused {
            kernel,
            in_use,
            ceiling: data,
            ..
        }) => {
            assert_eq!(kernel, "hoarder");
            assert!(in_use > 0 && in_use <= data, "{in_use} of {data}");
        }
        other => panic!("expected the hoarder refused, got {other:?}"),
    }
    assert!(
        take_kernel_alloc().lease_wait_us >= moruna_sandbox::LEASE_WAIT.as_micros() as u64,
        "the refused call's wait is counted"
    );
    let st = state.as_any_mut().downcast_mut::<SandboxState>().unwrap();
    assert!(st.leased().0 >= 2);
    let _ = std::fs::remove_dir_all(dir);
}

/// What a `.pex` stage says of itself, and what it refuses: a stage whose ceiling the rest of
/// the run holds, an input larger than its mapping, a restore without `checkpoint`, a kernel
/// whose `checkpoint` or `restore` raises, a tensor. f.7, f.8, SB-I12.
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn the_stage_says_what_it_is_and_refuses_what_it_cannot_do() {
    use moruna_kernel::{KernelKind, PayloadKind, ResumePolicy, SourceSchema};
    let dir = scratch();
    let identity = "def plain(b):\n    return b\n";
    let package = pex(&dir, "plain", identity, json!({}), &[]);
    let k = SandboxKernel::new(Arc::clone(&package), host(), Inputs::default()).unwrap();
    assert_eq!(k.fingerprint(), package.kernel_fingerprint());
    assert!(matches!(k.kind(), KernelKind::Stateful { .. }));
    assert_eq!(k.hints().resume, ResumePolicy::Reinit);
    assert!(k.declared().input.is_some());
    assert_eq!(k.accepts().kind, PayloadKind::Table);
    assert!(Arc::ptr_eq(k.package(), &package));
    assert!(k.target().starts_with("cp314-"));
    let _ = k.notes();
    let schema = SourceSchema::Table(batch().schema());
    assert!(matches!(
        k.output_schema(&schema).unwrap(),
        SourceSchema::Table(s) if s == batch().schema()
    ));
    // Unbound: no ceiling to compute from.
    let e = k.init(&ctx()).err().expect("unbound").to_string();
    assert!(e.contains("no ceiling"), "{e}");
    // The rest of the run holds the whole ceiling.
    k.bind_run(sampler(GIB, GIB), GIB);
    let e = k.init(&ctx()).err().expect("no room").to_string();
    assert!(e.contains("ceiling is 0"), "{e}");
    // A sample without a ceiling: the bound one is the run's.
    k.bind_run(sampler(0, 0), 64 << 20);
    let mut state = k.init(&ctx()).unwrap();
    call(&k, state.as_mut()).unwrap();
    // An input larger than the whole data area, with nothing leased.
    let e = call_with(&k, state.as_mut(), big(6 << 20))
        .unwrap_err()
        .to_string();
    assert!(e.contains("does not fit the mapping's data area"), "{e}");
    assert_eq!(
        state.checkpoint().unwrap(),
        None,
        "a stateless stage has none"
    );
    let e = k
        .restore(&ctx(), b"x")
        .err()
        .expect("no checkpoint")
        .to_string();
    assert!(e.contains("does not checkpoint"), "{e}");
    drop(state);

    let forbid = pex(
        &dir,
        "forbidden",
        "class forbidden:\n    def setup(self, ctx):\n        return None\n\n    def __call__(self, state, b):\n        return b\n",
        json!({"stateful": true, "resume": "forbid"}),
        &[],
    );
    let k = kernel(forbid, GIB);
    assert_eq!(k.hints().resume, ResumePolicy::Forbid);

    let fragile = r#"
class fragile:
    def setup(self, ctx):
        return {}

    def __call__(self, state, batch):
        return batch

    def checkpoint(self, state):
        raise RuntimeError("no state today")

    def restore(self, ctx, data):
        raise ValueError("bad bytes")
"#;
    let k = kernel(
        pex(
            &dir,
            "fragile",
            fragile,
            json!({"stateful": true, "resume": "checkpoint"}),
            &[],
        ),
        GIB,
    );
    let mut state = k.init(&ctx()).unwrap();
    let e = state.checkpoint().unwrap_err().to_string();
    assert!(e.contains("no state today"), "{e}");
    let e = k
        .restore(&ctx(), b"saved")
        .err()
        .expect("refused")
        .to_string();
    assert!(e.contains("open") && e.contains("bad bytes"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

/// What a test's run keeps of a child's output: each piece with its stage, and records with
/// their level.
#[derive(Default)]
struct Kept {
    got: std::sync::Mutex<Vec<(Option<u16>, String)>>,
}

impl moruna_kernel::OutputSink for Kept {
    fn bytes(&self, stage: Option<u16>, stream: moruna_kernel::Stream, bytes: &[u8]) {
        self.got.lock().unwrap().push((
            stage,
            format!("{stream:?}:{}", String::from_utf8_lossy(bytes)),
        ));
    }
    fn record(&self, stage: Option<u16>, level: Option<moruna_kernel::Level>, text: &str) {
        self.got
            .lock()
            .unwrap()
            .push((stage, format!("{level:?}:{text}")));
    }
    fn hold(&self) -> Box<dyn Send> {
        Box::new(())
    }
}

/// SB-T23 a child's output (MH 4.3 `log`): what a `.pex` prints, at import and in its calls, on
/// standard output with no newline and no flush, on standard error, and through `logging` with
/// an exception, is the stage's whose worker started the child, as it is written, never on
/// Moruna's own streams; a logging record keeps its level and its traceback whole.
#[test]
#[ignore = "Linux, sandbox host: MORUNA_SANDBOX_PYTHON and MORUNA_SANDBOX_CGROUP"]
fn sb_t23_a_childs_output_is_its_stages() {
    let dir = scratch();
    let k = kernel(
        pex(
            &dir,
            "loud",
            "import logging, sys\n\
             print('imported')\n\
             def loud(batch):\n\
             \x20   print('partial', end='')\n\
             \x20   print('on stderr', file=sys.stderr)\n\
             \x20   try:\n\
             \x20       raise ValueError('bad row')\n\
             \x20   except ValueError:\n\
             \x20       logging.getLogger('loud').exception('the call failed')\n\
             \x20   return batch\n",
            json!({}),
            &[],
        ),
        2 * GIB,
    );
    let kept = Arc::new(Kept::default());
    let sink: Arc<dyn moruna_kernel::OutputSink> = kept.clone();
    let mut state = {
        let _working = moruna_kernel::output::enter(&sink, 2);
        k.init(&ctx()).unwrap()
    };
    {
        let _working = moruna_kernel::output::enter(&sink, 2);
        assert_eq!(call(&k, state.as_mut()).unwrap(), batch());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let joined = || {
        kept.got
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect::<String>()
    };
    while !joined().contains("the call failed") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let got = kept.got.lock().unwrap().clone();
    assert!(got.iter().all(|(stage, _)| *stage == Some(2)), "{got:?}");
    let all = joined();
    assert!(all.contains("Stdout:imported\n"), "{all}");
    // Written through: the partial line arrives without its newline or a flush.
    assert!(all.contains("Stdout:partial"), "{all}");
    assert!(all.contains("Stderr:on stderr\n"), "{all}");
    let record = got
        .iter()
        .find(|(_, t)| t.starts_with("Some(Error):"))
        .expect("the logging record, at its level");
    assert!(record.1.contains("the call failed"), "{record:?}");
    assert!(
        record.1.contains("Traceback (most recent call last):"),
        "{record:?}"
    );
    assert!(record.1.contains("ValueError: bad row"), "{record:?}");
    drop(state);
    let _ = std::fs::remove_dir_all(dir);
}
