//! SB-T1 to SB-T7 (17 k), over components written in the text format.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_adapters::guard::MemoryGate;
use moruna_kernel::arrow::array::{Int64Array, StringArray};
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Allocator, InitCtx, Kernel, KernelKind, KernelState, MorunaError, Payload, Sample, Sampler,
    SourceSchema, take_kernel_alloc,
};
use moruna_package::{Manifest, Package};
use moruna_testkit::{FakeAllocator, FakeSampler};
use serde_json::json;

use super::*;
use crate::testing::*;
use moruna_package::Inputs;

static SCRATCH: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moruna-wasm-test-{}-{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn kernel_of(body: &str, m: &Manifest) -> (WasmKernel, PathBuf) {
    let dir = scratch();
    let bytes = if m.hints().stateful {
        component(&m.to_json(), body, &[], STATE_FUNCS, STATE_LIFTS)
    } else {
        component(&m.to_json(), body, &[], "", "")
    };
    let kernel = WasmKernel::new(package_at(&dir, &bytes, m), Inputs::default()).unwrap();
    (kernel, dir)
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

fn alloc() -> Arc<dyn Allocator> {
    Arc::new(FakeAllocator::new())
}

fn state_of(kernel: &WasmKernel) -> Box<dyn KernelState> {
    kernel
        .init(&InitCtx {
            instance: 0,
            device: None,
            alloc: alloc(),
        })
        .unwrap()
}

fn gate(anon: u64, ceiling: u64) -> Arc<MemoryGate> {
    let sampler = FakeSampler::new().scripted(vec![Sample {
        anon_bytes: anon,
        ceiling_bytes: ceiling,
        ..Sample::default()
    }]);
    MemoryGate::new(Arc::new(sampler) as Arc<dyn Sampler>, ceiling)
}

#[test]
fn sb_t1_identity_component() {
    let (kernel, dir) = kernel_of(IDENTITY, &manifest("echo", false, true));
    assert!(matches!(
        kernel.kind(),
        KernelKind::Stateful { max_instances } if max_instances.get() == 1
    ));
    assert_eq!(kernel.fingerprint(), kernel.package().kernel_fingerprint());
    assert!(kernel.declared().is_checkable());
    assert_eq!(kernel.hints().resume, moruna_kernel::ResumePolicy::Reinit);
    assert_eq!(kernel.notes().len(), 1, "no .cwasm: compiled, and said so");
    assert!(kernel.notes()[0].contains("compiled (no "));
    let schema = SourceSchema::Table(batch().schema());
    assert_eq!(kernel.output_schema(&schema).unwrap().hash(), schema.hash());
    let mut state = state_of(&kernel);
    // Large enough that its IPC stream does not fit the component's free page: the instance
    // grows its memory to take it, and the limiter counts that.
    let rows = 50_000i64;
    let input = RecordBatch::try_new(
        batch().schema(),
        vec![
            Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())),
            Arc::new(StringArray::from(
                (0..rows).map(|i| format!("row {i}")).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let out = kernel
        .apply(
            state.as_mut(),
            Payload::Table(input.clone(), moruna_kernel::Tier::Host),
        )
        .unwrap();
    let Payload::Table(out, _) = out else {
        panic!("a table");
    };
    assert_eq!(out, input);
    let counts = take_kernel_alloc();
    assert!(counts.measured);
    assert!(!counts.refusal_on, "no gate bound");
    assert!(
        counts.linear.requests >= 1,
        "the input was placed in grown memory"
    );
    assert!(counts.linear.peak >= counts.linear.bytes);
    assert!(counts.linear.largest > 0);
    assert_eq!(state.footprint(), Some(counts.linear.peak));
    assert_eq!(
        state.checkpoint().unwrap(),
        None,
        "a stateless instance has nothing to save"
    );
    // A stateless package has no state to restore; a tensor is refused.
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: alloc(),
    };
    assert!(matches!(
        kernel.restore(&ctx, &[]),
        Err(MorunaError::Resume(_))
    ));
    assert!(format!("{kernel:?}").contains("WasmKernel"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t1_the_kernels_error_names_it() {
    let (kernel, dir) = kernel_of(REFUSES, &manifest("refuses", false, true));
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        matches!(&e, MorunaError::Kernel { msg, .. } if msg == "no"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t1_output_must_be_one_batch() {
    let m = manifest("constant", false, true);
    let dir = scratch();
    // Not an IPC stream.
    let bytes = component(&m.to_json(), &constant(4), b"junk", "", "");
    let kernel = WasmKernel::new(package_at(&dir, &bytes, &m), Inputs::default()).unwrap();
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("not an Arrow IPC stream") || e.to_string().contains("ends inside"),
        "{e}"
    );
    // A stream with no batch.
    let empty = {
        let mut out = Vec::new();
        let mut w =
            moruna_kernel::arrow::ipc::writer::StreamWriter::try_new(&mut out, &batch().schema())
                .unwrap();
        w.finish().unwrap();
        drop(w);
        out
    };
    let m2 = manifest("empty", false, true);
    let bytes = component(&m2.to_json(), &constant(empty.len()), &empty, "", "");
    let kernel = WasmKernel::new(package_at(&dir, &bytes, &m2), Inputs::default()).unwrap();
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(e.to_string().contains("holds 0 record batches"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t2_import_refused() {
    let engine = engine().unwrap();
    let importing = wat::parse_str(r#"(component (import "host-fn" (func)))"#).unwrap();
    let c = Component::new(engine, &importing).unwrap();
    let e = verify(&c, false, false).unwrap_err();
    assert!(e.to_string().contains("imports `host-fn`"), "{e}");

    let m = manifest("echo", false, true);
    let ok = component(&m.to_json(), IDENTITY, &[], "", "");
    let c = Component::new(engine, &ok).unwrap();
    verify(&c, false, false).unwrap();
    // Stateful in the manifest and no open or close.
    assert!(
        verify(&c, true, false)
            .unwrap_err()
            .to_string()
            .contains("neither `open` nor `close`")
    );
    // open and close, and the manifest does not say stateful.
    let both = component(&m.to_json(), IDENTITY, &[], STATE_FUNCS, STATE_LIFTS);
    let c = Component::new(engine, &both).unwrap();
    assert!(
        verify(&c, false, false)
            .unwrap_err()
            .to_string()
            .contains("does not say stateful")
    );
    verify(&c, true, false).unwrap();
    // open without close, close without open.
    let open_only = component(
        &m.to_json(),
        IDENTITY,
        &[],
        STATE_FUNCS,
        r#"(func (export "open") (param "state" (list u8)) (result (result (error string)))
             (canon lift (core func $i "open") (memory $i "memory") (realloc (func $i "cabi_realloc"))))"#,
    );
    let c = Component::new(engine, &open_only).unwrap();
    assert!(
        verify(&c, true, false)
            .unwrap_err()
            .to_string()
            .contains("`open` without `close`")
    );
    let close_only = component(
        &m.to_json(),
        IDENTITY,
        &[],
        STATE_FUNCS,
        r#"(func (export "close") (result (list u8))
             (canon lift (core func $i "close") (memory $i "memory") (realloc (func $i "cabi_realloc"))))"#,
    );
    let c = Component::new(engine, &close_only).unwrap();
    assert!(
        verify(&c, true, false)
            .unwrap_err()
            .to_string()
            .contains("`close` without `open`")
    );
    // An extra export, and a wrong type.
    let extra = component(
        &m.to_json(),
        IDENTITY,
        &[],
        "",
        r#"(func (export "extra") (result string)
             (canon lift (core func $i "declare") (memory $i "memory") (realloc (func $i "cabi_realloc"))))"#,
    );
    let c = Component::new(engine, &extra).unwrap();
    assert!(
        verify(&c, false, false)
            .unwrap_err()
            .to_string()
            .contains("exports `extra`")
    );
    let wrong = wat::parse_str(
        r#"(component
  (core module $m (func (export "f") (result i32) (i32.const 0)))
  (core instance $i (instantiate $m))
  (func (export "process") (result u32) (canon lift (core func $i "f")))
  (func (export "declare") (result u32) (canon lift (core func $i "f"))))"#,
    )
    .unwrap();
    let c = Component::new(engine, &wrong).unwrap();
    let said = verify(&c, false, false).unwrap_err().to_string();
    assert!(said.contains("the world's is"), "{said}");
    let missing = wat::parse_str("(component)").unwrap();
    let c = Component::new(engine, &missing).unwrap();
    assert!(
        verify(&c, false, false)
            .unwrap_err()
            .to_string()
            .contains("does not export `declare`")
    );
    let not_func =
        wat::parse_str(r#"(component (core module $m) (export "process" (core module $m)))"#);
    if let Ok(bytes) = not_func {
        let c = Component::new(engine, &bytes).unwrap();
        assert!(
            verify(&c, false, false)
                .unwrap_err()
                .to_string()
                .contains("not a function")
        );
    }
}

#[test]
fn sb_t2_declare_must_agree() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let other = manifest("other", false, true);
    let bytes = component(&other.to_json(), IDENTITY, &[], "", "");
    let embedded = moruna_package::embed_manifest(&bytes, &m).unwrap();
    std::fs::write(dir.join("echo.wasm"), embedded).unwrap();
    let opened = Arc::new(Package::open(&dir.join("echo.wasm")).unwrap());
    let e = WasmKernel::new(opened, Inputs::default()).unwrap_err();
    assert!(e.to_string().contains("package: name:"), "{e}");
    // package(): the same check, and the embedded result.
    assert!(
        super::package(&bytes, &m)
            .unwrap_err()
            .to_string()
            .contains("name")
    );
    let out = super::package(&bytes, &other).unwrap();
    assert!(moruna_package::wasm_manifest(&out).unwrap().is_some());
    assert!(super::package(b"\0asm\x0d\0\x01\0junk", &m).is_err());
    // declare() returning something that is not a manifest.
    let junk = component("{}", IDENTITY, &[], "", "");
    let c = Component::new(engine().unwrap(), &junk).unwrap();
    assert!(
        agrees(&c, &m)
            .unwrap_err()
            .to_string()
            .contains("declare()")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t3_growth_past_the_ceiling_refused() {
    let (kernel, dir) = kernel_of(GROWS, &manifest("grows", false, true));
    // A ceiling of 4 MiB in force: the instance's memory may not pass it.
    kernel.bind_gate(gate(0, 4 << 20));
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    let counts = take_kernel_alloc();
    match e {
        MorunaError::Refused {
            kernel: name,
            requested,
            ceiling,
            ..
        } => {
            assert_eq!(name, "grows");
            assert_eq!(requested, 16 * 65536);
            assert_eq!(ceiling, 4 << 20);
        }
        other => panic!("expected a refusal, got {other}"),
    }
    assert!(counts.refusal_on);
    assert!(counts.linear.refused >= 1);
    assert!(counts.linear.peak <= 4 << 20, "{counts:?}");
    assert!(counts.linear.bytes > 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t4_gate_refuses_growth() {
    // The process already holds 100 MiB of a 101 MiB ceiling: one 1 MiB growth does not fit.
    let (kernel, dir) = kernel_of(GROWS, &manifest("grows", false, true));
    kernel.bind_gate(gate(100 << 20, 101 << 20));
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        matches!(e, MorunaError::Refused { in_use, .. } if in_use >= 100 << 20),
        "{e}"
    );
    assert!(take_kernel_alloc().linear.refused >= 1);
    let _ = std::fs::remove_dir_all(dir);

    // memory_guard false: the gate is not asked; the ceiling still holds.
    let (kernel, dir) = kernel_of(GROWS, &manifest("unguarded", false, false));
    kernel.bind_gate(gate(100 << 20, 101 << 20));
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    let counts = take_kernel_alloc();
    assert!(!counts.refusal_on);
    // The memory grew past what the gate would have allowed, up to the ceiling.
    assert!(counts.linear.peak > 2 << 20, "{counts:?}");
    assert!(
        matches!(e, MorunaError::Refused { ceiling, .. } if ceiling == 101 << 20),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t5_fuel_exhausted() {
    let (kernel, dir) = kernel_of(LOOPS, &manifest("loops", false, true));
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        matches!(&e, MorunaError::Kernel { msg, .. } if msg.starts_with("wasm: fuel exhausted")),
        "{e}"
    );
    assert_eq!(WasmState::fuel(10), FUEL_FLOOR + 10 * FUEL_PER_INPUT_BYTE);
    assert_eq!(WasmState::fuel(u64::MAX), u64::MAX);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t6_stateful_component() {
    let (kernel, dir) = kernel_of(COUNTS, &manifest("counter", true, true));
    assert_eq!(
        kernel.hints().resume,
        moruna_kernel::ResumePolicy::Checkpoint
    );
    let mut state = state_of(&kernel);
    for _ in 0..3 {
        kernel
            .apply(
                state.as_mut(),
                Payload::Table(batch(), moruna_kernel::Tier::Host),
            )
            .unwrap();
    }
    let saved = state.checkpoint().unwrap().unwrap();
    assert_eq!(saved, 3u64.to_le_bytes().to_vec());
    kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap();
    assert_eq!(
        state.checkpoint().unwrap().unwrap(),
        4u64.to_le_bytes().to_vec()
    );
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: alloc(),
    };
    let mut restored = kernel.restore(&ctx, &saved).unwrap();
    kernel
        .apply(
            restored.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap();
    assert_eq!(
        restored.checkpoint().unwrap().unwrap(),
        4u64.to_le_bytes().to_vec(),
        "a restored instance continues the count"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sb_t7_cwasm_reused() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let bytes = component(&m.to_json(), IDENTITY, &[], "", "");
    let package = package_at(&dir, &bytes, &m);
    let written = aot(&package).unwrap();
    assert_eq!(written, dir.join("echo.cwasm"));
    let head = std::fs::read(&written).unwrap();
    assert_eq!(&head[..16], &cwasm::MAGIC);
    let kernel = WasmKernel::new(package.clone(), Inputs::default()).unwrap();
    assert!(
        kernel.notes().is_empty(),
        "the .cwasm was used: {:?}",
        kernel.notes()
    );
    let mut state = state_of(&kernel);
    kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap();

    // Another package's .cwasm: ignored, compiled, and the note says why.
    let other = manifest("echo", false, false);
    let other_bytes = component(&other.to_json(), IDENTITY, &[], "", "");
    let other_dir = scratch();
    let other_package = package_at(&other_dir, &other_bytes, &other);
    std::fs::copy(&written, other_dir.join("echo.cwasm")).unwrap();
    let kernel = WasmKernel::new(other_package.clone(), Inputs::default()).unwrap();
    assert!(
        kernel.notes()[0].contains("it was compiled from"),
        "{:?}",
        kernel.notes()
    );
    // Not a .cwasm, a short header, another Moruna.
    std::fs::write(other_dir.join("echo.cwasm"), b"junk").unwrap();
    assert!(
        WasmKernel::new(other_package.clone(), Inputs::default())
            .unwrap()
            .notes()[0]
            .contains("not a Moruna .cwasm")
    );
    let mut cut = cwasm::MAGIC.to_vec();
    cut.extend_from_slice(&1000u32.to_le_bytes());
    std::fs::write(other_dir.join("echo.cwasm"), &cut).unwrap();
    assert!(
        WasmKernel::new(other_package.clone(), Inputs::default())
            .unwrap()
            .notes()[0]
            .contains("cut short")
    );
    let mut aged = cwasm::MAGIC.to_vec();
    let h =
        json!({"fingerprint": other_package.fingerprint_string(), "moruna": "0.0.0"}).to_string();
    aged.extend_from_slice(&(h.len() as u32).to_le_bytes());
    aged.extend_from_slice(h.as_bytes());
    std::fs::write(other_dir.join("echo.cwasm"), &aged).unwrap();
    assert!(
        WasmKernel::new(other_package.clone(), Inputs::default())
            .unwrap()
            .notes()[0]
            .contains("compiled by Moruna")
    );
    let mut broken = cwasm::MAGIC.to_vec();
    let h = json!({"fingerprint": other_package.fingerprint_string(), "moruna": moruna_package::MORUNA_VERSION}).to_string();
    broken.extend_from_slice(&(h.len() as u32).to_le_bytes());
    broken.extend_from_slice(h.as_bytes());
    broken.extend_from_slice(b"not wasmtime's");
    std::fs::write(other_dir.join("echo.cwasm"), &broken).unwrap();
    assert!(
        WasmKernel::new(other_package.clone(), Inputs::default())
            .unwrap()
            .notes()[0]
            .contains("wasmtime refused it")
    );
    let mut bad_json = cwasm::MAGIC.to_vec();
    bad_json.extend_from_slice(&2u32.to_le_bytes());
    bad_json.extend_from_slice(b"{]");
    std::fs::write(other_dir.join("echo.cwasm"), &bad_json).unwrap();
    assert!(
        WasmKernel::new(other_package, Inputs::default())
            .unwrap()
            .notes()[0]
            .contains("not JSON")
    );
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(other_dir);
}

#[test]
fn a_pex_is_not_a_component() {
    use std::io::Write;
    let dir = scratch();
    let path = dir.join("x.pex");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    zip.start_file("moruna.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    let manifest = json!({
        "name": "x", "entry": "function:f", "declaration": {"hints": {}},
        "abi": moruna_kernel::declare::ABI_VERSION, "moruna": "0", "format": "pex",
        "platforms": ["cp314-manylinux_2_28_x86_64"]
    });
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();
    let package = Package::open(&path).unwrap();
    assert!(compile(&package).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

/// A growth past the memory's own maximum: the limiter admitted it, wasmtime failed it, and the
/// admission goes back to the gate (e.2: `memory_grow_failed` credits).
#[test]
fn sb_t3_a_failed_growth_gives_its_admission_back() {
    let dir = scratch();
    let m = manifest("capped", false, true);
    let text = component_text(&m.to_json(), GROWS, &[], "", "").replace(
        "(memory (export \"memory\") 2)",
        "(memory (export \"memory\") 2 20)",
    );
    let bytes = wat::parse_str(text).unwrap();
    let kernel = WasmKernel::new(package_at(&dir, &bytes, &m), Inputs::default()).unwrap();
    let g = gate(0, 1 << 30);
    kernel.bind_gate(g.clone());
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    // Not a refusal: the component trapped after its own memory said no.
    assert!(
        matches!(&e, MorunaError::Kernel { msg, .. } if msg.starts_with("wasm: ")),
        "{e}"
    );
    let counts = take_kernel_alloc();
    assert_eq!(counts.linear.refused, 0);
    assert!(counts.linear.peak <= 20 * 65536, "{counts:?}");
    // What was admitted and failed is not held against the next request.
    assert_eq!(g.admit((1 << 30) - (20 * 65536)), Ok(()));
    let _ = std::fs::remove_dir_all(dir);
}

/// `aot` into a place that cannot be written is an IO error naming the file.
#[test]
fn sb_t7_aot_needs_the_place() {
    let dir = scratch();
    let m = manifest("echo", false, true);
    let bytes = component(&m.to_json(), IDENTITY, &[], "", "");
    let package = package_at(&dir, &bytes, &m);
    std::fs::create_dir_all(dir.join("echo.cwasm")).unwrap();
    let e = aot(&package).unwrap_err();
    assert!(
        matches!(&e, MorunaError::Io { target, .. } if target.ends_with("echo.cwasm")),
        "{e}"
    );
    // A .cwasm that is a directory cannot be read either, and the kernel compiles.
    let kernel = WasmKernel::new(package, Inputs::default()).unwrap();
    assert!(kernel.notes()[0].contains("compiled"));
    let _ = std::fs::remove_dir_all(dir);
}

/// PK-T17 (a .wasm): the document's arguments reach `configure`, once, before any batch; a kernel
/// not built by arguments refuses them; one that is gets `{}` when none are given.
#[test]
fn pk_t17_configure_gets_the_arguments() {
    let dir = scratch();
    let m = args_manifest("configured");
    let package = package_at(&dir, &configure_component(&m.to_json()), &m);
    let inputs = Inputs {
        args: Some(json!({"model": "churn", "threshold": 0.5})),
        assets: None,
    };
    let kernel = WasmKernel::new(package.clone(), inputs).unwrap();
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        matches!(&e, MorunaError::Kernel { msg, .. } if msg == r#"{"model":"churn","threshold":0.5}"#),
        "{e}"
    );
    let kernel = WasmKernel::new(package, Inputs::default()).unwrap();
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(
        matches!(&e, MorunaError::Kernel { msg, .. } if msg == "{}"),
        "{e}"
    );
    // Arguments for a kernel that is not built by them.
    let plain = manifest("plain", false, true);
    let package = package_at(
        &dir,
        &component(&plain.to_json(), IDENTITY, &[], "", ""),
        &plain,
    );
    let e = WasmKernel::new(
        package,
        Inputs {
            args: Some(json!({})),
            assets: None,
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("args"), "{e}");
    // `configure` without the hint, and the hint without `configure`.
    let engine = engine().unwrap();
    let c = Component::new(engine, configure_component(&plain.to_json())).unwrap();
    assert!(
        verify(&c, false, false)
            .unwrap_err()
            .to_string()
            .contains("does not say args")
    );
    let c = Component::new(engine, component(&plain.to_json(), IDENTITY, &[], "", "")).unwrap();
    assert!(
        verify(&c, false, true)
            .unwrap_err()
            .to_string()
            .contains("does not export `configure`")
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// PK-T17 (a .wasm): `read-asset` reads a file of the assets directory, and nothing outside it.
#[test]
fn pk_t17_read_asset() {
    let dir = scratch();
    let assets = dir.join("assets");
    std::fs::create_dir_all(assets.join("weights")).unwrap();
    // The asset is the IPC stream of a batch, so the kernel's output is the asset itself.
    let mut ipc = Vec::new();
    let mut w =
        moruna_kernel::arrow::ipc::writer::StreamWriter::try_new(&mut ipc, &batch().schema())
            .unwrap();
    w.write(&batch()).unwrap();
    w.finish().unwrap();
    drop(w);
    std::fs::write(assets.join("weights/w.arrow"), &ipc).unwrap();
    std::fs::write(dir.join("secret"), b"no").unwrap();
    let m = manifest("reads", false, true);
    let package = package_at(&dir, &asset_component(&m.to_json(), "weights/w.arrow"), &m);
    let inputs = Inputs {
        args: None,
        assets: Some(assets.clone()),
    };
    let kernel = WasmKernel::new(package.clone(), inputs.clone()).unwrap();
    let mut state = state_of(&kernel);
    let out = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap();
    let Payload::Table(out, _) = out else {
        panic!("a table");
    };
    assert_eq!(out, batch());
    // No assets directory named: the import's error is the kernel's.
    let kernel = WasmKernel::new(package, Inputs::default()).unwrap();
    let mut state = state_of(&kernel);
    let e = kernel
        .apply(
            state.as_mut(),
            Payload::Table(batch(), moruna_kernel::Tier::Host),
        )
        .unwrap_err();
    assert!(e.to_string().contains("no assets directory"), "{e}");
    // Names that leave the directory.
    let limits = limiter::Limits::unbound().with_assets(Some(assets.clone()));
    assert!(
        limits
            .read_asset("../secret")
            .unwrap_err()
            .contains("not a path inside")
    );
    assert!(
        limits
            .read_asset("/etc/hosts")
            .unwrap_err()
            .contains("not a path inside")
    );
    assert!(limits.read_asset("").is_err());
    assert!(limits.read_asset("missing").is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.join("secret"), assets.join("link")).unwrap();
        assert!(
            limits
                .read_asset("link")
                .unwrap_err()
                .contains("leaves the assets directory")
        );
    }
    assert_eq!(limits.read_asset("weights/w.arrow").unwrap(), ipc);
    let missing = Inputs {
        args: None,
        assets: Some(dir.join("none")),
    };
    let m2 = manifest("reads2", false, true);
    let package = package_at(&dir, &asset_component(&m2.to_json(), "x"), &m2);
    assert!(
        WasmKernel::new(package, missing)
            .unwrap_err()
            .to_string()
            .contains("assets")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_import_other_than_read_asset_is_refused() {
    let engine = engine().unwrap();
    let c = Component::new(
        engine,
        wat::parse_str(r#"(component (import "read-asset" (func (param "name" string))))"#)
            .unwrap(),
    )
    .unwrap();
    let said = verify(&c, false, false).unwrap_err().to_string();
    assert!(said.contains("imported as"), "{said}");
    let c = Component::new(
        engine,
        wat::parse_str(r#"(component (import "env" (instance)))"#).unwrap(),
    )
    .unwrap();
    assert!(
        verify(&c, false, false)
            .unwrap_err()
            .to_string()
            .contains("imports `env`")
    );
}

/// MH 4.3 `log`, audited for `.wasm`: a component has no stream to print to. WASI's standard
/// output and error are imports, and every import but `read-asset` is refused, so a Wasm
/// kernel's only output is its batches and its traps, which the lifecycle tells.
#[test]
fn a_wasm_kernel_has_no_output_stream() {
    let engine = engine().unwrap();
    for interface in ["wasi:cli/stdout@0.2.0", "wasi:cli/stderr@0.2.0"] {
        let c = Component::new(
            engine,
            wat::parse_str(format!(r#"(component (import "{interface}" (instance)))"#)).unwrap(),
        )
        .unwrap();
        let said = verify(&c, false, false).unwrap_err().to_string();
        assert!(said.contains(&format!("imports `{interface}`")), "{said}");
    }
}
