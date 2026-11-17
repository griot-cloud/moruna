//! Node mode (system design 3A, 3B): how `moruna serve` takes a document on a Moruna node, where
//! it serves on a channel of the kernel (`--listen channel:<slot>`) whose other end is the node's
//! `api`.
//!
//! - It fills in what the node decides when the document leaves it out: `report.socket` (the
//!   channel), `staging` (none: a document that names no staging directory has none) and the
//!   run's ID.
//! - It refuses, each by its field and with a sentence that says what to change: a host socket
//!   (`report.socket` naming anything but the channel, an object store's endpoint on `unix://` or
//!   `vsock://`), a `file://` path outside the job's volumes, and `resume`. A job on a node reads
//!   and writes the volumes the cloud attaches (`disk://`, item 3.3) and object storage; it has
//!   no file system of its own, so in this release every `file://` path is outside its volumes,
//!   and so is a root for them (`object_store.local_root`).
//! - Its budget (system design 3A): given the most a job on the node may have (`--budget-max`,
//!   the node's memory less 768 MiB, which `node-admission` knows), a larger
//!   `budget.memory_bytes` is refused, and a document that names none is given all of it: a node
//!   runs one job.

use moruna_job::{Job, SpecError};
use serde_json::Value;

/// What node mode needs to know: the channel the job is served on, and the most its budget may
/// be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The channel's address, as `report.socket` names it: `channel:<slot>`.
    pub channel: String,
    /// The most a job's budget may be on this node, in bytes: the node's memory less 768 MiB
    /// (system design 3A). `None`: no bound, and a document that names no budget keeps none.
    pub budget_max: Option<u64>,
}

impl Node {
    /// Node mode on `channel`, with no bound on the budget.
    pub fn on(channel: impl Into<String>) -> Node {
        Node {
            channel: channel.into(),
            budget_max: None,
        }
    }
}

/// The budget `job` runs with on `node`: the document's, or, when it names none, the most the
/// node allows.
pub fn budget(job: &Job, node: &Node) -> Option<u64> {
    job.budget.memory_bytes.or(node.budget_max)
}

/// Whether `text` names a `file:` URL, in any case.
fn is_file_url(text: &str) -> bool {
    text.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
}

/// Whether `text` names a socket of the host: a `unix:` or a `vsock:` address.
fn is_host_socket(text: &str) -> bool {
    ["unix:", "vsock:"].iter().any(|scheme| {
        text.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

/// Every `file:` URL in `value`, with its place under `field`.
fn file_urls(field: &str, value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) if is_file_url(text) => found.push(field.to_string()),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                file_urls(&format!("{field}[{i}]"), item, found);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                file_urls(&format!("{field}.{key}"), item, found);
            }
        }
        _ => {}
    }
}

/// What node mode refuses in `job`, each by its field, in the order the document is read.
pub fn refusals(job: &Job, node: &Node) -> Vec<SpecError> {
    let mut refused = Vec::new();
    for (i, task) in job.tasks.iter().enumerate() {
        let mut found = Vec::new();
        for (key, value) in &task.settings {
            file_urls(&format!("{}.{key}", job.field(i)), value, &mut found);
        }
        refused.extend(found.into_iter().map(|field| {
            SpecError::new(
                field,
                "a file:// path is outside the job's volumes: on a node a job reads and writes \
                 attached volumes and object storage",
            )
        }));
    }
    if let (Some(budget), Some(max)) = (job.budget.memory_bytes, node.budget_max)
        && budget > max
    {
        refused.push(SpecError::new(
            "budget.memory_bytes",
            format!(
                "{budget} bytes is more than a job on this node may have, {max} bytes (the \
                 node's memory less 768 MiB, kept for its services and a check): ask for less, \
                 or leave it out for all of it"
            ),
        ));
    }
    if job.object_store.local_root.is_some() {
        refused.push(SpecError::new(
            "object_store.local_root",
            "a root for file:// paths is outside the job's volumes: on a node a job has no file \
             system of its own",
        ));
    }
    let endpoints = [(
        "object_store.s3.endpoint",
        job.object_store
            .s3
            .as_ref()
            .and_then(|s| s.endpoint.as_deref()),
    )];
    for (field, endpoint) in endpoints {
        if endpoint.is_some_and(is_host_socket) {
            refused.push(SpecError::new(
                field,
                "a host socket is not reachable from a node: name the object store's endpoint by \
                 its URL",
            ));
        }
    }
    if let Some(socket) = &job.report.socket
        && *socket != node.channel
    {
        refused.push(SpecError::new(
            "report.socket",
            format!(
                "a job on a node reports on its channel, {}: leave report.socket out",
                node.channel
            ),
        ));
    }
    if job.resume.is_some() {
        refused.push(SpecError::new(
            "resume",
            "a job on a node is not resumed in this release: leave resume out",
        ));
    }
    refused
}

/// Takes `job` in node mode: the first refusal, or the job with what the node decides filled
/// in. `mint` gives the run's ID when the document names none.
pub fn admit(
    job: &mut Job,
    node: &Node,
    mint: impl FnOnce() -> Option<String>,
) -> Result<(), SpecError> {
    if let Some(refusal) = refusals(job, node).into_iter().next() {
        return Err(refusal);
    }
    job.report.socket = Some(node.channel.clone());
    job.budget.memory_bytes = budget(job, node);
    if job.id.is_none() {
        job.id = Some(mint().ok_or_else(|| SpecError::new("run_id", "no run ID could be made"))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
