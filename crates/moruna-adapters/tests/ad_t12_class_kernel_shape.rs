//! AD-T12 class_kernel_shape (b, e.1, f.7): the shape of a class kernel. `init` runs once per
//! instance with its own `ctx.instance` and returns a state of its own; `apply` hands that state
//! back as the first argument; `footprint` is the class's own answer, or `None` when the class
//! has no such method; a class missing `setup` or `__call__` is refused when the kernel is built,
//! as is `resume='checkpoint'` without `restore`; and a checkpointed state comes back through
//! `restore` behaving as the original did.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use core::num::NonZeroUsize;
use std::sync::Arc;

use moruna_adapters::python::{PyKernel, PyKernelSpec};
use moruna_kernel::{Allocator, MorunaError, InitCtx, Kernel, KernelKind, Payload, ResumePolicy};
use moruna_testkit::FakeAllocator;

/// A class kernel whose state counts the morsels it has seen and whose output is the instance
/// index, so a test can see which state `apply` was given.
const COUNTER: &str = r#"
import ast
import pyarrow

class Counter:
    def setup(self, ctx):
        return {"instance": ctx.instance, "device": ctx.device, "seen": 0}

    def __call__(self, state, batch):
        state["seen"] += 1
        return pyarrow.record_batch(
            [pyarrow.array([state["instance"]] * batch.num_rows, type=pyarrow.int64())],
            names=["n"],
        )

    def footprint(self, state):
        return 4096 + state["seen"]

    def checkpoint(self, state):
        return repr(state).encode()

    def restore(self, ctx, data):
        return ast.literal_eval(data.decode())

counter = Counter()
"#;

/// The same class without `footprint`, and without the checkpoint pair.
const PLAIN: &str = r#"
class Plain:
    def setup(self, ctx):
        return {"instance": ctx.instance}

    def __call__(self, state, batch):
        return batch

plain = Plain()
"#;

const NO_SETUP: &str = r#"
class NoSetup:
    def __call__(self, state, batch):
        return batch

no_setup = NoSetup()
"#;

const NO_CALL: &str = r#"
class NoCall:
    def setup(self, ctx):
        return None

no_call = NoCall()
"#;

const NO_RESTORE: &str = r#"
class NoRestore:
    def setup(self, ctx):
        return 0

    def __call__(self, state, batch):
        return batch

    def checkpoint(self, state):
        return b""

no_restore = NoRestore()
"#;

const INSTANCES: usize = 3;

fn stateful_spec(source: &str, attribute: &str, module: &str) -> PyKernelSpec {
    let mut spec = PyKernelSpec::new(common::py_object(source, attribute, module));
    spec.stateful = true;
    spec.instances = NonZeroUsize::new(INSTANCES).expect("three is not zero");
    spec
}

fn init_ctx(instance: usize, alloc: &Arc<dyn Allocator>) -> InitCtx {
    InitCtx {
        instance,
        device: None,
        alloc: Arc::clone(alloc),
    }
}

#[test]
fn ad_t12_instances_are_independent_and_apply_gets_the_matching_state() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let mut spec = stateful_spec(COUNTER, "counter", "ad_t12_counter");
    spec.resume = ResumePolicy::Checkpoint;
    let kernel = PyKernel::new(spec).expect("the class has setup, __call__, checkpoint, restore");
    kernel.bind_allocator(Arc::clone(&alloc));

    assert!(matches!(
        kernel.kind(),
        KernelKind::Stateful { max_instances } if max_instances.get() == INSTANCES
    ));

    let mut states: Vec<Box<dyn moruna_kernel::KernelState>> = (0..INSTANCES)
        .map(|instance| {
            kernel
                .init(&init_ctx(instance, &alloc))
                .expect("setup returns a state")
        })
        .collect();

    // e.1: every instance has its own state, and `apply` is given the matching one.
    for (instance, state) in states.iter_mut().enumerate() {
        let batch = common::arena_i64_batch(&fake, &[0, 0, 0, 0]);
        let input = Payload::table(batch).expect("the batch is arena owned");
        let out = kernel
            .apply(state.as_mut(), input)
            .expect("the kernel runs");
        assert_eq!(
            common::i64_column(&out),
            vec![instance as i64; 4],
            "apply was given the wrong instance's state"
        );
        // The class reports its own footprint, which grew with the morsel it just saw.
        assert_eq!(state.footprint(), Some(4096 + 1));
    }

    // f.7: the checkpoint is what the class produced, and restore rebuilds a state that behaves
    // the way the original did.
    let saved = states[1]
        .checkpoint()
        .expect("checkpoint succeeds")
        .expect("a Checkpoint kernel must return Some");
    assert!(
        String::from_utf8_lossy(&saved).contains("'instance': 1"),
        "the checkpoint bytes are not the ones the class produced"
    );
    let mut restored = kernel
        .restore(&init_ctx(1, &alloc), &saved)
        .expect("restore rebuilds the state");
    let batch = common::arena_i64_batch(&fake, &[0, 0]);
    let input = Payload::table(batch).expect("the batch is arena owned");
    let out = kernel
        .apply(restored.as_mut(), input)
        .expect("the restored state runs");
    assert_eq!(common::i64_column(&out), vec![1, 1]);
}

#[test]
fn ad_t12_footprint_is_none_when_the_class_has_no_such_method() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake);
    let kernel = PyKernel::new(stateful_spec(PLAIN, "plain", "ad_t12_plain"))
        .expect("the class has setup and __call__");
    kernel.bind_allocator(Arc::clone(&alloc));
    let state = kernel.init(&init_ctx(0, &alloc)).expect("setup runs");
    assert_eq!(state.footprint(), None);
    // A Reinit kernel has nothing to save.
    let mut state = state;
    assert_eq!(state.checkpoint().expect("no error"), None);
}

#[test]
fn ad_t12_a_class_without_setup_or_call_is_refused_when_the_kernel_is_built() {
    for (source, attribute, module, missing) in [
        (NO_SETUP, "no_setup", "ad_t12_no_setup", "setup"),
        (NO_CALL, "no_call", "ad_t12_no_call", "__call__"),
    ] {
        let error = PyKernel::new(stateful_spec(source, attribute, module))
            .expect_err("a class missing a method is refused");
        let MorunaError::Plan(message) = &error else {
            panic!("a missing method is a Plan error, not {error:?}");
        };
        assert!(
            message.contains(missing),
            "the Plan error must name {missing}; it said {message:?}"
        );
    }
}

#[test]
fn ad_t12_checkpoint_without_restore_is_refused_when_the_kernel_is_built() {
    let mut spec = stateful_spec(NO_RESTORE, "no_restore", "ad_t12_no_restore");
    spec.resume = ResumePolicy::Checkpoint;
    let error = PyKernel::new(spec).expect_err("resume='checkpoint' needs restore");
    let MorunaError::Plan(message) = &error else {
        panic!("a missing restore is a Plan error, not {error:?}");
    };
    assert!(
        message.contains("restore"),
        "the Plan error must name restore; it said {message:?}"
    );
}
