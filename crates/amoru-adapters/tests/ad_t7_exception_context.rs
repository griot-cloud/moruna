//! AD-T7 exception_context (AD-I7): a Python exception becomes an `AmoruError::Kernel` whose
//! message is the exception's type, its text, a newline and the formatted traceback, and the
//! thread that called `apply` survives.

#![cfg(feature = "python")]
#![allow(clippy::result_large_err)]

mod common;

use std::sync::Arc;

use amoru_kernel::{Allocator, AmoruError, Kernel, NoState, Payload};
use amoru_testkit::FakeAllocator;

const RAISER: &str = r#"
def raiser(batch):
    raise ValueError("x")
"#;

#[test]
fn ad_t7_exception_context() {
    let fake = FakeAllocator::new();
    let alloc: Arc<dyn Allocator> = Arc::new(fake.clone());
    let kernel = Arc::new(common::stateless_kernel(
        RAISER,
        "raiser",
        "ad_t7_raiser",
        alloc,
    ));
    let batch = common::arena_i64_batch(&fake, &[1, 2, 3]);
    let input = Payload::table(batch).expect("the batch is arena owned");

    // The worker never sees a panic: the call runs on its own thread and that thread joins.
    let worker = {
        let kernel = Arc::clone(&kernel);
        std::thread::spawn(move || {
            let mut state = NoState;
            kernel.apply(&mut state, input)
        })
    };
    let result = worker
        .join()
        .expect("the worker thread survived the exception");
    let error = result.expect_err("a raising kernel is an error");

    let AmoruError::Kernel { msg, .. } = &error else {
        panic!("a Python exception must become AmoruError::Kernel, not {error:?}");
    };
    assert!(
        msg.starts_with("ValueError: x\n"),
        "the message must start with the type, the text and a newline; it was {msg:?}"
    );
    let traceback = &msg["ValueError: x\n".len()..];
    assert!(
        traceback.contains("Traceback"),
        "the rest of the message must be the formatted traceback; it was {traceback:?}"
    );
    assert!(
        traceback.contains("ad_t7_raiser.py") && traceback.contains("raiser"),
        "the traceback must name the kernel's source line; it was {traceback:?}"
    );
    assert_eq!(kernel.stats().exceptions, 1);
    assert_eq!(kernel.stats().calls, 1);
}
