//! The `amoru-bench` integration tests, in one binary.
//!
//! Two modules: `generator` for the data generator (F1.6) and `kernels` for the
//! benchmark kernels (F1.7). They share a binary because each integration test
//! target links the whole of arrow, parquet and object_store and is instrumented
//! again under `cargo llvm-cov`, and `bench/Cargo.toml` says so.

mod generator;
mod kernels;
