//! What an embedder depends on to read Moruna's kinds and check a job document without running
//! anything (SDD 22).
//!
//! Three things are here, and nothing that needs Arrow, DataFusion or the runtime:
//!
//! - the declaration of a kind and the listing `moruna kinds` prints ([`Declaration`],
//!   [`Listing`]; SDD 18 e.1 to e.3);
//! - the job document, [`JobSpec`], read, written and put in canonical form (MH 4.1);
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
mod kind;
pub mod schema;

pub use catalogue::{Catalogue, CatalogueError};
pub use check::{Refusal, RefusalCode, check};
pub use doc::{
    AzureDoc, BudgetDoc, CHECKPOINT_INTERVAL_MS, CheckpointDoc, ContractEntry, ElasticDoc,
    ErrorPolicyDoc, GcsDoc, GuaranteeDoc, HostProfileDoc, JobSpec, KindDoc, ObjectStoreDoc,
    ReportDoc, S3Doc, SPEC_VERSION, SizerDoc, SpecError, StagingDoc, TaskIdsDoc, TaskRefDoc,
    is_task_id,
};
pub use kind::{
    Declaration, Fact, Flow, Isolation, LISTING_VERSION, Listing, Needs, Role, SCHEMA_DIALECT,
    Urls, is_kind_name, settings_schema,
};
