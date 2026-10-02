//! AD-T14 to AD-T23 (E13): the allocator guard's hooks with a real interpreter and NumPy.
//!
//! Every gate here is over a scripted sampler that says the process holds 100 MiB under a
//! 110 MiB ceiling, so a request above 10 MiB is refused whatever this process really holds,
//! and refused requests use `numpy.empty` or are never made, so nothing large is touched. The
//! tests take one lock: Arrow's figures come from a pool the whole process shares (AD-I15).

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::{Arc, Mutex, MutexGuard};

use moruna_adapters::guard::MemoryGate;
use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::{
    Allocator, Kernel, KernelAlloc, MorunaError, NoState, Payload, Result, Sample, Sampler,
    take_kernel_alloc,
};
use moruna_testkit::{FakeAllocator, FakeSampler};
use pyo3::prelude::*;

const MIB: u64 = 1024 * 1024;

const SOURCE: &str = r#"
import threading
import numpy as np
import pyarrow as pa

kept = []

def big(batch):
    np.empty(32 * 1024 * 1024, dtype=np.uint8)
    return batch

def big_bytes(batch):
    bytes(32 * 1024 * 1024)
    return batch

def big_list(batch):
    [None] * (4 * 1024 * 1024)
    return batch

def recovers(batch):
    try:
        bytes(32 * 1024 * 1024)
    except MemoryError:
        pass
    return batch

def reraises(batch):
    try:
        np.empty(32 * 1024 * 1024, dtype=np.uint8)
    except MemoryError:
        raise ValueError("no room")
    return batch

def grows(batch):
    out = []
    for i in range(200_000):
        out.append(i)
    small = [bytes(100) for _ in range(1000)]
    return batch

def keeps(batch):
    a = np.ones(1024 * 1024, dtype=np.uint8)
    b = np.ones(8 * 1024 * 1024, dtype=np.uint8)
    del a
    kept.append(b)
    c = np.ones(1024 * 1024, dtype=np.uint8)
    c.resize(4 * 1024 * 1024, refcheck=False)
    tiny = np.ones(16, dtype=np.uint8)
    return batch

def release():
    # On the free-threaded build a reference dropped by another thread than the owner's is
    # merged later; a collection makes the free happen now.
    import gc
    kept.clear()
    gc.collect()

def fails(batch):
    raise ValueError("x")

def ones_1(batch):
    for _ in range(50):
        np.empty(1024 * 1024, dtype=np.uint8)
    return batch

def ones_2_then_big(batch):
    for _ in range(50):
        np.empty(2 * 1024 * 1024, dtype=np.uint8)
    np.empty(512 * 1024 * 1024, dtype=np.uint8)
    return batch

def arrow_only(batch):
    pa.array(range(1_000_000), pa.int64())
    return batch

def numpy_only(batch):
    np.ones(4 * 1024 * 1024, dtype=np.uint8)
    return batch

def handler_name():
    return np.core.multiarray.get_handler_name() if hasattr(np, "core") else np._core.multiarray.get_handler_name()
"#;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn module() -> Py<PyAny> {
    static MODULE: std::sync::OnceLock<Py<PyAny>> = std::sync::OnceLock::new();
    Python::attach(|py| {
        MODULE
            .get_or_init(|| {
                let code = std::ffi::CString::new(SOURCE).expect("no nul");
                pyo3::types::PyModule::from_code(py, &code, c"ad_guard_mod.py", c"ad_guard_mod")
                    .expect("the fixture imports")
                    .into_any()
                    .unbind()
            })
            .clone_ref(py)
    })
}

fn function(name: &str) -> Py<PyAny> {
    Python::attach(|py| module().bind(py).getattr(name).expect("defined").unbind())
}

fn gate() -> Arc<MemoryGate> {
    let sampler = FakeSampler::new().scripted(vec![Sample {
        anon_bytes: 100 * MIB,
        ..Sample::default()
    }]);
    MemoryGate::new(Arc::new(sampler) as Arc<dyn Sampler>, 110 * MIB)
}

struct Rig {
    fake: FakeAllocator,
    kernel: Arc<PyKernel>,
}

fn rig(name: &str, guarded: bool) -> Rig {
    let fake = FakeAllocator::new();
    let mut spec = PyKernelSpec::new(function(name));
    spec.memory_guard = guarded;
    let kernel = PyKernel::new(spec).expect("a kernel");
    kernel.bind_allocator(Arc::new(fake.clone()) as Arc<dyn Allocator>);
    kernel.bind_gate(gate());
    Rig {
        fake,
        kernel: Arc::new(kernel),
    }
}

impl Rig {
    /// One call on this thread: its result and the counts it left.
    fn call(&self) -> (Result<Payload>, KernelAlloc) {
        let batch = common::arena_i64_batch(&self.fake, &[1, 2, 3]);
        let input = Payload::table(batch).expect("arena owned");
        let result = self.kernel.apply(&mut NoState, input);
        (result, take_kernel_alloc())
    }
}

#[test]
fn ad_t14_numpy_blocks_counted() {
    let _serial = serial();
    let r = rig("keeps", true);
    let (result, counts) = r.call();
    result.expect("the call completes");
    let numpy = counts.numpy;
    assert!(numpy.requests >= 4, "{numpy:?}");
    assert!(
        numpy.bytes >= 13 * MIB,
        "1 + 8 + 1 + 3 of growth: {numpy:?}"
    );
    assert!(numpy.largest >= 8 * MIB, "{numpy:?}");
    assert!(
        numpy.peak >= 12 * MIB,
        "8 kept and 4 grown at once: {numpy:?}"
    );
    let live = r.kernel.memory();
    assert_eq!(
        live.live_bytes,
        8 * MIB,
        "only the kept array is still held: {live:?}"
    );
    assert!(live.peak_bytes >= 12 * MIB, "{live:?}");
    // The kept array is freed on another thread, after the call: credited all the same.
    std::thread::spawn(|| {
        Python::attach(|py| {
            module().bind(py).call_method0("release").expect("released");
        })
    })
    .join()
    .expect("freed");
    assert_eq!(r.kernel.memory().live_bytes, 0);
}

#[test]
fn ad_t15_numpy_request_refused() {
    let _serial = serial();
    let r = rig("big", true);
    let (result, counts) = Python::attach(|py| {
        // An outer attachment, so the worker's context is this one and can be read after.
        let before = module()
            .bind(py)
            .call_method0("handler_name")
            .expect("name")
            .to_string();
        let out = r.call();
        let after = module()
            .bind(py)
            .call_method0("handler_name")
            .expect("name")
            .to_string();
        assert_eq!(before, after, "the handler before the call is back");
        out
    });
    let error = result.expect_err("refused");
    let MorunaError::Refused {
        kernel,
        requested,
        in_use,
        ceiling,
        ..
    } = &error
    else {
        panic!("expected Refused, got {error}");
    };
    assert_eq!(kernel, "ad_guard_mod.big");
    assert_eq!(*requested, 32 * MIB);
    assert_eq!((*in_use, *ceiling), (100 * MIB, 110 * MIB));
    assert_eq!(counts.numpy.refused, 1);
    assert!(counts.refusal_on && counts.measured);
    assert_eq!(r.kernel.memory().refusals, 1);
}

#[test]
fn ad_t16_python_domain_request_refused() {
    let _serial = serial();
    for name in ["big_bytes", "big_list"] {
        let r = rig(name, true);
        let (result, counts) = r.call();
        let error = result.expect_err("refused");
        assert!(
            matches!(error, MorunaError::Refused { .. }),
            "{name}: {error}"
        );
        assert_eq!(counts.python.refused, 1, "{name}");
        assert!(counts.python.largest >= 32 * MIB, "{name}: {counts:?}");
    }
    let r = rig("recovers", true);
    let (result, counts) = r.call();
    result.expect("a call that catches MemoryError completes");
    assert_eq!(counts.python.refused, 1);
    // Growth by realloc and small requests are never refused.
    let r = rig("grows", true);
    let (result, counts) = r.call();
    result.expect("growth and small requests pass");
    assert!(counts.python.requests >= 1000, "{counts:?}");
    assert_eq!(counts.python.refused, 0);
}

#[test]
fn ad_t17_attribution_across_threads() {
    let _serial = serial();
    let a = rig("ones_1", true);
    let b = rig("ones_2_then_big", true);
    let run = |r: Rig| std::thread::spawn(move || r.call());
    let (ha, hb) = (run(a), run(b));
    let (ra, ca) = ha.join().expect("a");
    let (rb, cb) = hb.join().expect("b");
    ra.expect("a completes while b is refused");
    assert!(matches!(rb, Err(MorunaError::Refused { .. })));
    assert_eq!(ca.numpy.bytes, 50 * MIB, "{ca:?}");
    assert_eq!(ca.numpy.largest, MIB);
    assert_eq!(ca.numpy.refused, 0);
    assert_eq!(cb.numpy.largest, 512 * MIB);
    assert_eq!(cb.numpy.bytes, 100 * MIB + 512 * MIB);
    assert_eq!(cb.numpy.refused, 1);
}

#[test]
fn ad_t18_inert_outside_a_frame() {
    let _serial = serial();
    let r = rig("big", true);
    let _ = r.call(); // the hooks are installed now, and a gate is bound
    Python::attach(|py| {
        // Outside any frame: not refused (the gate would refuse it) and not counted anywhere.
        let made = py
            .eval(c"len(bytes(32 * 1024 * 1024))", None, None)
            .expect("not refused outside a frame");
        assert_eq!(made.extract::<u64>().expect("int"), 32 * MIB);
    });
    let other = std::thread::spawn(|| {
        Python::attach(|py| {
            py.eval(c"len(bytearray(32 * 1024 * 1024))", None, None)
                .map(|v| v.extract::<u64>().unwrap_or(0))
                .unwrap_or(0)
        })
    })
    .join()
    .expect("joined");
    assert_eq!(other, 32 * MIB);
    assert_eq!(take_kernel_alloc(), KernelAlloc::default());
}

#[test]
fn ad_t19_switch_off() {
    let _serial = serial();
    let r = rig("big", false);
    assert!(!r.kernel.memory_guard());
    let (result, counts) = r.call();
    result.expect("refusal is off: the request goes through");
    assert!(counts.measured, "and it is counted all the same");
    assert!(!counts.refusal_on);
    assert!(counts.numpy.bytes >= 32 * MIB, "{counts:?}");
    assert_eq!(counts.numpy.refused, 0);
    assert_eq!(r.kernel.memory().refusals, 0);
}

#[test]
fn ad_t20_refusal_diagnostic() {
    let _serial = serial();
    let r = rig("big", true);
    let error = r.call().0.expect_err("refused");
    assert_eq!(
        error.to_string(),
        "budget: kernel ad_guard_mod.big stage 65535 morsel 18446744073709551615 requested \
         33554432 bytes with 104857600 in use, which would pass the ceiling of 115343360 bytes; \
         refused before the memory existed"
    );
    let r = rig("reraises", true);
    let error = r.call().0.expect_err("refused");
    assert!(
        matches!(error, MorunaError::Refused { ref kernel, .. } if kernel == "ad_guard_mod.reraises"),
        "a kernel that turns MemoryError into ValueError is still refused: {error}"
    );
}

#[test]
#[ignore = "(reference host, E1) reports the guard's overhead; run with --ignored --nocapture"]
fn ad_t21_guard_overhead() {
    let _serial = serial();
    const LOOP: &str = r#"
import time, numpy as np
def make(size, n):
    if size == 0:
        def k(batch):
            # Python objects: a string and a list slot per iteration.
            out = []
            for i in range(n):
                out.append(str(i))
            return batch
        return k
    def k(batch):
        for _ in range(n):
            np.empty(size, dtype=np.uint8)
        return batch
    return k
"#;
    let maker = Python::attach(|py| {
        let code = std::ffi::CString::new(LOOP).expect("no nul");
        pyo3::types::PyModule::from_code(py, &code, c"ad_t21.py", c"ad_t21")
            .expect("imports")
            .getattr("make")
            .expect("make")
            .unbind()
    });
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "this host".into());
    for (size, n) in [
        (64 * 1024usize, 200_000usize),
        (1024, 1_000_000),
        (0, 1_000_000),
    ] {
        let callable = Python::attach(|py| maker.bind(py).call1((size, n)).expect("k").unbind());
        // Three ways, best of five each, interleaved: the function called plainly (no frame, no
        // handler: what it costs without the guard), counted with refusal off, guarded.
        let mut best = [f64::MAX; 3];
        let kernels: Vec<(bool, PyKernel)> = [false, true]
            .into_iter()
            .map(|guarded| {
                let mut spec = PyKernelSpec::new(Python::attach(|py| callable.clone_ref(py)));
                spec.memory_guard = guarded;
                let kernel = PyKernel::new(spec).expect("kernel");
                kernel.bind_allocator(Arc::new(FakeAllocator::new()) as Arc<dyn Allocator>);
                kernel.bind_gate(gate());
                (guarded, kernel)
            })
            .collect();
        for _round in 0..5 {
            let t = std::time::Instant::now();
            Python::attach(|py| {
                callable.bind(py).call1((py.None(),)).expect("plain");
            });
            best[0] = best[0].min(t.elapsed().as_nanos() as f64 / n as f64);
            for (at, (_, kernel)) in kernels.iter().enumerate() {
                let fake = FakeAllocator::new();
                kernel.bind_allocator(Arc::new(fake.clone()) as Arc<dyn Allocator>);
                let batch = common::arena_i64_batch(&fake, &[1]);
                let input = Payload::table(batch).expect("arena");
                let t = std::time::Instant::now();
                let _ = kernel.apply(&mut NoState, input);
                best[at + 1] = best[at + 1].min(t.elapsed().as_nanos() as f64 / n as f64);
            }
        }
        println!(
            "AD-T21 ({host}, provisional): {size} B arrays, {n} per call, best of 5: plain {:.1} ns, counted {:.1} ns, guarded {:.1} ns per array; guarded/plain {:.3}",
            best[0],
            best[1],
            best[2],
            best[2] / best[0]
        );
    }
}

#[test]
fn ad_t22_arrow_from_the_pool() {
    let _serial = serial();
    let r = rig("arrow_only", true);
    let (result, counts) = r.call();
    result.expect("completes");
    assert!(counts.arrow.bytes >= 8_000_000, "{counts:?}");
    assert!(counts.arrow.requests >= 1, "{counts:?}");
    assert_eq!((counts.arrow.largest, counts.arrow.refused), (0, 0));
    let r = rig("numpy_only", true);
    let (result, counts) = r.call();
    result.expect("completes");
    assert_eq!(counts.arrow.bytes, 0, "{counts:?}");
    assert!(counts.numpy.bytes >= 4 * MIB);
}

#[test]
fn ad_t23_counts_reach_the_record() {
    let _serial = serial();
    let r = rig("numpy_only", true);
    let (result, counts) = r.call();
    result.expect("completes");
    assert!(counts.measured && counts.refusal_on);
    assert_eq!(take_kernel_alloc(), KernelAlloc::default(), "taken once");
    let failed = rig("fails", false);
    let (result, counts) = failed.call();
    let error = result.expect_err("raises ValueError");
    assert!(
        matches!(error, MorunaError::Kernel { .. }),
        "unguarded: {error}"
    );
    assert!(counts.measured && !counts.refusal_on);
}
