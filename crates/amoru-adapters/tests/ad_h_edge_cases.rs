//! The edge cases and failures section h names, and the parts of f.4, f.7 and e.1 no section k
//! test reaches.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use core::num::NonZeroUsize;
use std::sync::Arc;

use amoru_adapters::python::cross::export;
use amoru_adapters::python::{PyKernel, PyKernelSpec};
use amoru_adapters::{python_build_info, python_gil_enabled};
use amoru_kernel::{
    Allocator, AmoruError, DeviceId, InitCtx, Kernel, KernelKind, NoState, Payload, PayloadKind,
    PayloadSpec, ResumePolicy, SourceSchema, Tier, TierPref,
};
use amoru_testkit::FakeAllocator;
use pyo3::prelude::*;

const IDENTITY: &str = r#"
def identity(batch):
    return batch
"#;

/// h: `apply` before `bind_allocator` is a `Config` error naming the adapter, never a copy to the
/// global allocator.
#[test]
fn apply_before_bind_allocator_is_a_config_error() {
    let fake = FakeAllocator::new();
    let kernel = PyKernel::new(PyKernelSpec::new(common::py_object(
        IDENTITY,
        "identity",
        "ad_h_unbound",
    )))
    .expect("a well formed kernel");
    let batch = common::arena_i64_batch(&fake, &[1]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut state = NoState;
    let error = kernel
        .apply(&mut state, input)
        .expect_err("an unbound adapter refuses");
    let AmoruError::Config { name, .. } = &error else {
        panic!("not a Config error: {error:?}");
    };
    assert_eq!(*name, "adapter");
}

/// d.1: a stateless kernel that is not callable is refused when the kernel is built.
#[test]
fn a_stateless_kernel_must_be_callable() {
    let not_callable = Python::attach(|py| py.eval(c"42", None, None).expect("an int").unbind());
    let error = PyKernel::new(PyKernelSpec::new(not_callable)).expect_err("42 is not a kernel");
    assert!(matches!(error, AmoruError::Plan(_)));
}

/// d.7: the hints and the kind a spec asks for are the ones the kernel reports.
#[test]
fn the_spec_becomes_the_kernels_hints() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake);
    let mut spec = PyKernelSpec::new(common::py_object(IDENTITY, "identity", "ad_h_hints"));
    spec.device_memory = true;
    spec.releases_gil = Some(true);
    spec.expected_amplification = Some(1.5);
    spec.preferred_rows = Some(65_536);
    spec.state_bytes = Some(1 << 20);
    spec.accepts = PayloadSpec {
        kind: PayloadKind::Either,
        tier: TierPref::Any,
    };
    let kernel = PyKernel::new(spec).expect("a well formed kernel");
    kernel.bind_allocator(alloc);
    let hints = kernel.hints();
    assert!(hints.uses_device_memory);
    assert_eq!(hints.releases_gil, Some(true));
    assert_eq!(hints.expected_amplification, Some(1.5));
    assert_eq!(hints.preferred_rows, Some(65_536));
    assert_eq!(hints.state_bytes, Some(1 << 20));
    assert_eq!(hints.resume, ResumePolicy::Reinit);
    assert!(matches!(kernel.kind(), KernelKind::Stateless));
    assert_eq!(kernel.accepts().kind, PayloadKind::Either);
    // The adapter cannot know a Python kernel's output schema, so it answers with the input's.
    let schema = SourceSchema::Tensor {
        dtype: amoru_kernel::DType::F32,
        shape: vec![-1, 8],
    };
    let out = kernel
        .output_schema(&schema)
        .expect("Either accepts a tensor");
    assert!(matches!(out, SourceSchema::Tensor { .. }));
    // A stateless kernel's `init` is never called by the scheduler, and returns the unit state.
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc: Arc::new(FakeAllocator::new()),
    };
    assert!(kernel.init(&ctx).is_ok());
    assert!(format!("{kernel:?}").contains("PyKernel"));
}

/// e.2: a batch with no resident bytes cannot cross, and the reserved remote tier is refused
/// rather than wildcarded (CT-I11).
#[test]
fn a_batch_that_is_not_resident_cannot_cross() {
    let fake = FakeAllocator::new();
    let batch = common::arena_i64_batch(&fake, &[1, 2]);
    Python::attach(|py| {
        // SAFETY: test code building a state a test needs (E9); nothing reads the bytes,
        // because `export` refuses the tier before looking at them.
        let staged = unsafe {
            Payload::table_in(
                batch.clone(),
                Tier::Disk(amoru_kernel::SegmentRef {
                    segment: 3,
                    offset: 0,
                    len: 16,
                }),
            )
        };
        assert!(matches!(export(py, staged), Err(AmoruError::Staging(_))));

        // SAFETY: as above, for the reserved remote tier.
        let remote = unsafe {
            Payload::table_in(
                batch.clone(),
                Tier::Remote(
                    amoru_kernel::LOCAL_NODE,
                    amoru_kernel::RemoteRef {
                        addr: 0,
                        rkey: 0,
                        len: 16,
                    },
                ),
            )
        };
        assert!(matches!(
            export(py, remote),
            Err(AmoruError::Unsupported("rdma"))
        ));

        // SAFETY: as above, for a device batch, which needs the Arrow C Device Interface.
        let device = unsafe { Payload::table_in(batch, Tier::Device(DeviceId(0))) };
        assert!(matches!(
            export(py, device),
            Err(AmoruError::Unsupported("arrow-c-device"))
        ));
    });
}

const AWKWARD_STATE: &str = r#"
class Awkward:
    def setup(self, ctx):
        return {"instance": ctx.instance, "device": ctx.device}

    def __call__(self, state, batch):
        return batch

    def footprint(self, state):
        return "not an int"

awkward = Awkward()

class Raises:
    def setup(self, ctx):
        return 0

    def __call__(self, state, batch):
        return batch

    def footprint(self, state):
        raise RuntimeError("no idea")

raises = Raises()

class BadSetup:
    def setup(self, ctx):
        raise ValueError("setup failed")

    def __call__(self, state, batch):
        return batch

bad_setup = BadSetup()
"#;

fn stateful(attribute: &str, module: &str) -> PyKernelSpec {
    let mut spec = PyKernelSpec::new(common::py_object(AWKWARD_STATE, attribute, module));
    spec.stateful = true;
    spec.instances = NonZeroUsize::new(2).expect("two is not zero");
    spec
}

/// e.1: a `footprint` that answers with something other than an int or `None` leaves the state
/// size unknown rather than failing the run, and so does one that raises.
#[test]
fn a_footprint_that_is_not_an_int_is_unknown() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    for (attribute, module) in [("awkward", "ad_h_awkward"), ("raises", "ad_h_raises")] {
        let kernel = PyKernel::new(stateful(attribute, module)).expect("a well formed kernel");
        kernel.bind_allocator(Arc::clone(&alloc));
        let ctx = InitCtx {
            instance: 1,
            device: Some(DeviceId(2)),
            alloc: Arc::clone(&alloc),
        };
        let state = kernel.init(&ctx).expect("setup runs");
        assert_eq!(state.footprint(), None);
        // Complained about once, then quiet.
        assert_eq!(state.footprint(), None);
    }
}

/// e.1: `ctx` carries the instance index and the device as `"cuda:N"`.
#[test]
fn the_ctx_object_carries_the_instance_and_the_device() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let kernel = PyKernel::new(stateful("awkward", "ad_h_ctx")).expect("a well formed kernel");
    kernel.bind_allocator(Arc::clone(&alloc));
    let ctx = InitCtx {
        instance: 1,
        device: Some(DeviceId(3)),
        alloc: Arc::clone(&alloc),
    };
    let mut state = kernel.init(&ctx).expect("setup runs");
    let seen = state.as_any_mut();
    let py_state = seen
        .downcast_mut::<amoru_adapters::python::PyState>()
        .expect("a Python state");
    assert_eq!(py_state.instance(), 1);
    assert_eq!(py_state.device(), Some(DeviceId(3)));
    let device: Option<String> = Python::attach(|py| {
        py_state
            .state()
            .bind(py)
            .get_item("device")
            .expect("the state records the device")
            .extract()
            .expect("a string or None")
    });
    assert_eq!(device.as_deref(), Some("cuda:3"));
}

/// h: `setup` raising is a `Kernel` error at `init`, before any morsel is read.
#[test]
fn a_setup_that_raises_fails_at_init() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let kernel =
        PyKernel::new(stateful("bad_setup", "ad_h_bad_setup")).expect("a well formed kernel");
    kernel.bind_allocator(Arc::clone(&alloc));
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc,
    };
    let Err(error) = kernel.init(&ctx) else {
        panic!("setup raised, so init must fail");
    };
    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("not a Kernel error: {error:?}");
    };
    assert!(msg.starts_with("ValueError: setup failed\n"));
}

/// f.7: a kernel that did not declare `Checkpoint` refuses to be restored.
#[test]
fn a_reinit_kernel_cannot_be_restored() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let kernel =
        PyKernel::new(stateful("awkward", "ad_h_no_resume")).expect("a well formed kernel");
    kernel.bind_allocator(Arc::clone(&alloc));
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc,
    };
    assert!(matches!(
        kernel.restore(&ctx, b"anything"),
        Err(AmoruError::Resume(_))
    ));
}

/// f.4 and j: the build information the run report carries names the interpreter and its GIL.
#[test]
fn the_build_info_names_the_interpreter_and_the_gil() {
    let info = python_build_info();
    assert!(info.contains("gil_enabled="), "build info was {info:?}");
    assert!(
        !info.contains('\n'),
        "build info must be one line: {info:?}"
    );
    assert_eq!(
        info.contains("gil_enabled=true"),
        python_gil_enabled(),
        "the build info and the probe disagree"
    );
}

const RESUME_SHAPES: &str = r#"
class BadCheckpoint:
    def setup(self, ctx):
        return {"n": 0}

    def __call__(self, state, batch):
        return batch

    def checkpoint(self, state):
        return "not bytes"

    def restore(self, ctx, data):
        return {"n": 0}

bad_checkpoint = BadCheckpoint()

class BadRestore:
    def setup(self, ctx):
        return {"n": 0}

    def __call__(self, state, batch):
        return batch

    def checkpoint(self, state):
        return b"anything"

    def restore(self, ctx, data):
        raise RuntimeError("cannot come back")

bad_restore = BadRestore()

class QuietFootprint:
    def setup(self, ctx):
        return repr(ctx)

    def __call__(self, state, batch):
        return batch

    def footprint(self, state):
        return None

quiet_footprint = QuietFootprint()
"#;

fn checkpointing(attribute: &str, module: &str) -> PyKernelSpec {
    let mut spec = PyKernelSpec::new(common::py_object(RESUME_SHAPES, attribute, module));
    spec.stateful = true;
    spec.instances = NonZeroUsize::new(1).expect("one is not zero");
    spec.resume = ResumePolicy::Checkpoint;
    spec
}

/// f.7: a `checkpoint` that does not return bytes is an error naming what it did return.
#[test]
fn a_checkpoint_that_is_not_bytes_is_an_error() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let kernel = PyKernel::new(checkpointing("bad_checkpoint", "ad_h_bad_checkpoint"))
        .expect("the class has all four methods");
    kernel.bind_allocator(Arc::clone(&alloc));
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc,
    };
    let mut state = kernel.init(&ctx).expect("setup runs");
    let Err(error) = state.checkpoint() else {
        panic!("a checkpoint that is not bytes must fail");
    };
    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("not a Kernel error: {error:?}");
    };
    assert!(
        msg.contains("checkpoint must return bytes"),
        "the message must say what was wanted: {msg}"
    );
}

/// f.7: a `restore` that raises is an error, with the exception's context (AD-I7).
#[test]
fn a_restore_that_raises_is_an_error() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let kernel = PyKernel::new(checkpointing("bad_restore", "ad_h_bad_restore"))
        .expect("the class has all four methods");
    kernel.bind_allocator(Arc::clone(&alloc));
    let ctx = InitCtx {
        instance: 0,
        device: None,
        alloc,
    };
    let Err(error) = kernel.restore(&ctx, b"anything") else {
        panic!("a restore that raises must fail");
    };
    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("not a Kernel error: {error:?}");
    };
    assert!(msg.starts_with("RuntimeError: cannot come back\n"));
}

/// e.1: a `footprint` that answers `None` is the kernel saying it does not know, not an error.
/// The `ctx` object's repr is the one `setup` saw.
#[test]
fn a_footprint_of_none_is_unknown_and_ctx_has_a_repr() {
    let alloc: Arc<dyn Allocator> = Arc::new(FakeAllocator::new());
    let mut spec = PyKernelSpec::new(common::py_object(
        RESUME_SHAPES,
        "quiet_footprint",
        "ad_h_quiet",
    ));
    spec.stateful = true;
    spec.instances = NonZeroUsize::new(1).expect("one is not zero");
    let kernel = PyKernel::new(spec).expect("the class has setup and __call__");
    kernel.bind_allocator(Arc::clone(&alloc));

    for (device, expected) in [
        (None, "KernelContext(instance=0, device=None)"),
        (
            Some(DeviceId(5)),
            "KernelContext(instance=0, device='cuda:5')",
        ),
    ] {
        let ctx = InitCtx {
            instance: 0,
            device,
            alloc: Arc::clone(&alloc),
        };
        let state = kernel.init(&ctx).expect("setup runs");
        assert_eq!(state.footprint(), None);
        let seen: String = Python::attach(|py| {
            let mut state = state;
            let py_state = state
                .as_any_mut()
                .downcast_mut::<amoru_adapters::python::PyState>()
                .expect("a Python state");
            py_state.state().bind(py).extract().expect("a string")
        });
        assert_eq!(seen, expected);
    }
}

/// h: a stateful kernel handed a state it did not make is an error, not a silent misuse.
#[test]
fn a_stateful_kernel_refuses_a_state_it_did_not_make() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let mut spec = PyKernelSpec::new(common::py_object(
        RESUME_SHAPES,
        "quiet_footprint",
        "ad_h_wrong_state",
    ));
    spec.stateful = true;
    spec.instances = NonZeroUsize::new(1).expect("one is not zero");
    let kernel = PyKernel::new(spec).expect("the class has setup and __call__");
    kernel.bind_allocator(alloc);
    let batch = common::arena_i64_batch(&fake, &[1]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let mut wrong = NoState;
    let Err(error) = kernel.apply(&mut wrong, input) else {
        panic!("a stateful kernel must refuse NoState");
    };
    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("not a Kernel error: {error:?}");
    };
    assert!(msg.contains("state it did not make"), "{msg}");
}
