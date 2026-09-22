//! Amoru test fakes: one fake per contracts trait, exactly the table in contracts d.15.
//!
//! Design: `architecture/sdd/01-contracts.md` (d.15). Every fake has the knobs (builder methods)
//! and observables that table lists and nothing else, and a `shutdown_calls` counter wherever
//! the trait it implements has a `shutdown` method. A component's test names a fake and a knob
//! from that table only; a test that needs a knob the table lacks is a contracts change (E10),
//! never a local addition.
//!
//! The crate depends on `amoru-kernel` only. Its behaviour is deterministic: no fake reads a
//! clock except where a knob says so (`with_latency`, `latency`, `with_delay`), and each of
//! those resolves from a thread of its own so submission never blocks the caller.
//!
//! Three deviations from d.15, each reported as an escalation on the pull request that adds
//! this crate (issue 12): `FakeKernel` keys `fail_on` and `panic_on` on the apply call index
//! and reports `u16::MAX` for the worker, because `Kernel::apply` receives a `Payload` and so
//! sees neither a sequence number nor a worker; `FakeSink::shutdown_calls` and
//! `FakeTrace::finish_calls` count the fakes' own inherent `shutdown()` and `finish()`, because
//! `Sink` has no `shutdown` and `TraceSink` has no `finish`.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod allocator;
pub mod kernel;
pub mod knobs;
pub mod placement;
pub mod reactor;
pub mod sampler;
pub mod sink;
pub mod source;
pub mod trace;

pub use allocator::FakeAllocator;
pub use kernel::FakeKernel;
pub use knobs::FakeKnobs;
pub use placement::FakePlacement;
pub use reactor::{FakeReactor, OpKind, OpRecord};
pub use sampler::FakeSampler;
pub use sink::FakeSink;
pub use source::FakeSource;
pub use trace::FakeTrace;
