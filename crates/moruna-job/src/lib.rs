//! What an embedder depends on to read Moruna's kinds and check a job document without running
//! anything (SDD 22).
//!
//! Three things are here, and nothing that needs Arrow, DataFusion or the runtime:
//!
//! - the declaration of a kind and the listing `moruna kinds` prints ([`Declaration`],
//!   [`Listing`]; SDD 18 e.1 to e.3);
//! - the job document: version 2, [`Job`], a DAG of tasks read from FlatBuffers and checked as
//!   one (SDD 22 e.4, e.5), and version 1, [`JobSpec`], the linear chain of MH 4.1, read as a
//!   DAG with one path ([`Job::from_v1`]);
//! - the [`Catalogue`], every declaration an embedder can name, and [`check`], which reads a
//!   document against it and collects every refusal: an unknown kind, settings its kind's schema
//!   refuses, flows that do not fit, a contract a kind needs and the job does not carry.
//!
//! Building a run from a document stays in `moruna-runtime`, which is where kinds are code.

#![deny(missing_docs)]

mod canonical;
mod catalogue;
mod check;
mod doc;
mod flatbuffer;
mod flex;
mod job;
mod kind;
pub mod schema;

/// The Rust flatc generates from `schema/job.fbs` (`tools/flatc/generate.sh`), committed and
/// checked in CI against what the schema generates.
#[allow(missing_docs, clippy::all, clippy::pedantic, unsafe_op_in_unsafe_fn, deprecated)]
#[rustfmt::skip]
mod generated {
    include!("generated/job_generated.rs");
}

/// The job document's FlatBuffers tables as flatc generates them from `schema/job.fbs`: what a
/// program that writes a document without [`Job`] builds with. Bytes from anyone are read with
/// [`Job::from_flatbuffer`], which verifies them first.
pub use generated::moruna::job::fb;

pub use catalogue::{Catalogue, CatalogueError};
pub use check::{Refusal, RefusalCode, check, check_graph};
pub use doc::{
    AzureDoc, BudgetDoc, CHECKPOINT_INTERVAL_MS, CheckpointDoc, ContractEntry, ElasticDoc,
    ErrorPolicyDoc, GcsDoc, GuaranteeDoc, HostProfileDoc, JobSpec, KindDoc, ObjectStoreDoc,
    ReportDoc, S3Doc, SPEC_VERSION, SizerDoc, SpecError, StagingDoc, TaskIdsDoc, TaskRefDoc,
    is_task_id,
};
pub use flatbuffer::{JOB_IDENTIFIER, JobError, is_job_flatbuffer};
pub use job::{Edge, IN, JOB_VERSION, Job, OUT, PortRef, Reporting, Task, V1_SINK, V1_SOURCE};
pub use kind::{
    Declaration, Fact, Flow, Isolation, LISTING_VERSION, Listing, Needs, Role, SCHEMA_DIALECT,
    Urls, is_kind_name, settings_schema,
};
