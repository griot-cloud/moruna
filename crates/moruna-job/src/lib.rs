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
//! - the [`Catalogue`], every declaration an embedder can name, Moruna's and its own, and
//!   [`check`], which reads a document against it and collects every refusal: an unknown kind,
//!   settings its kind's schema refuses, flows that do not fit, columns that do not fit a served
//!   kind's schema rules, a contract a kind needs and the job does not carry;
//! - what a kind its embedder serves is ([`Served`], SDD 18 e.4): the port that serves it
//!   ([`PortDoc`]), its schema rules ([`SchemaRules`]), and the call its port is handed for each
//!   batch ([`ServedCall`]).
//!
//! Building a run from a document stays in `moruna-runtime`, which is where kinds are code.

#![deny(missing_docs)]

mod call;
mod canonical;
mod catalogue;
mod check;
mod doc;
mod flatbuffer;
mod flex;
mod job;
mod kind;
mod port;
pub mod rules;
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

pub use call::{CALL_HEADER_MAX, CALL_MAGIC, CALL_VERSION, CallError, ServedCall};
/// The canonical JSON of a value (MH 4.1): object keys sorted by their bytes, no whitespace, whatever
/// `serde_json`'s features are. What a document's identity is taken over, and a served kind's
/// fingerprint (SDD 18 e.4).
pub use canonical::to_string as canonical_json;
pub use catalogue::{Catalogue, CatalogueError};
pub use check::{Refusal, RefusalCode, check, check_graph, settings_refusals};
pub use doc::{
    AzureDoc, BudgetDoc, CHECKPOINT_INTERVAL_BOUNDS_MS, CHECKPOINT_INTERVAL_MS, CheckpointDoc,
    ContractEntry, ElasticDoc, ErrorPolicyDoc, GcsDoc, GuaranteeDoc, HostProfileDoc, JobSpec,
    KindDoc, ObjectStoreDoc, ReportDoc, S3Doc, SPEC_VERSION, SizerDoc, SpecError, StagingDoc,
    TaskIdsDoc, TaskRefDoc, checkpoint_interval_ms, is_task_id,
};
pub use flatbuffer::{JOB_IDENTIFIER, JobError, is_job_flatbuffer};
pub use job::{Edge, IN, JOB_VERSION, Job, OUT, PortRef, Reporting, Task, V1_SINK, V1_SOURCE};
pub use kind::{
    Declaration, Fact, Flow, Isolation, LISTING_VERSION, Listing, Needs, Role, SCHEMA_DIALECT,
    Served, Urls, is_kind_name, settings_schema,
};
pub use port::{PORT_DEADLINE_MAX_MS, PORT_DEADLINE_MS, Place, PortDoc, PortFault, is_port_name};
pub use rules::SchemaRules;
