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
//! - Its volumes (system design 3C): given the node's disks (`--volumes`, which
//!   `node-admission` grants every check), a `disk://<serial>` no disk has is refused, and so is
//!   the disk the node booted from, which is never a volume, and a sink's volume served read only.

use moruna_job::{Job, SpecError};
use serde_json::Value;

use super::protocol::VolumeNeed;

/// What node mode needs to know: the channel the job is served on, and the most its budget may
/// be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The channel's address, as `report.socket` names it: `channel:<slot>`.
    pub channel: String,
    /// The most a job's budget may be on this node, in bytes: the node's memory less 768 MiB
    /// (system design 3A). `None`: no bound, and a document that names no budget keeps none.
    pub budget_max: Option<u64>,
    /// The node's disks (system design 3C). `None`: not known, and no volume is refused.
    pub volumes: Option<Vec<NodeVolume>>,
}

impl Node {
    /// Node mode on `channel`, with no bound on the budget and its disks not known.
    pub fn on(channel: impl Into<String>) -> Node {
        Node {
            channel: channel.into(),
            budget_max: None,
            volumes: None,
        }
    }
}

/// One of the node's disks, as `node-admission` grants a check them: its serial, its bytes,
/// whether its driver serves it read only, and whether the node booted from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeVolume {
    /// The serial the cloud gives it, which a job names it by.
    pub serial: String,
    /// Its bytes.
    pub bytes: u64,
    /// Its driver refuses every write.
    pub read_only: bool,
    /// The disk the node booted from, which is never a volume.
    pub boot: bool,
}

/// The node's disks from `node-admission`'s list: a line each, `<serial> <bytes> <ro|rw>
/// <boot|data>` (`node_protocol::volume_lines`).
pub fn parse_volumes(text: &str) -> Result<Vec<NodeVolume>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            let bad = || format!("the node's volumes, line {}: `{line}`", i + 1);
            match line.split_ascii_whitespace().collect::<Vec<_>>().as_slice() {
                [serial, bytes, access, kind] => Ok(NodeVolume {
                    serial: (*serial).to_string(),
                    bytes: bytes.parse().map_err(|_| bad())?,
                    read_only: match *access {
                        "ro" => true,
                        "rw" => false,
                        _ => return Err(bad()),
                    },
                    boot: match *kind {
                        "boot" => true,
                        "data" => false,
                        _ => return Err(bad()),
                    },
                }),
                _ => Err(bad()),
            }
        })
        .collect()
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

/// What a job needs of a node, beyond its budget, which the node's check answers and
/// `node-admission` keeps with the job (system design 3A, 3C, 3D, 3E).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeNeeds {
    /// The hosts it reaches from its object-store settings: the S3 endpoint's host,
    /// `<account>.blob.core.windows.net` for Azure, `storage.googleapis.com` for GCS. Each once,
    /// sorted.
    pub hosts: Vec<String>,
    /// Each `disk://<serial>` it names, written by a sink and only read otherwise; a serial
    /// both read and written is written. By serial.
    pub volumes: Vec<VolumeNeed>,
    /// The kinds of its function tasks, each once, in the order the document names them: a
    /// function by its registered name (3E).
    pub functions: Vec<String>,
}

/// The host of a URL's authority (`https://s3.example.com:9000/x` is `s3.example.com`),
/// lowercase; `None` for a URL with no host or a host socket.
fn url_host(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The serial of a `disk://<serial>/...` URL, in any case of the scheme.
fn disk_serial(text: &str) -> Option<&str> {
    let rest = text
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("disk://"))
        .map(|_| &text[7..])?;
    let serial = rest.split('/').next().unwrap_or_default();
    (!serial.is_empty()).then_some(serial)
}

/// Every `disk://` URL's serial in `value`, with its place under `field`.
fn disk_urls(field: &str, value: &Value, found: &mut Vec<(String, String)>) {
    match value {
        Value::String(text) => {
            if let Some(serial) = disk_serial(text) {
                found.push((field.to_string(), serial.to_string()));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                disk_urls(&format!("{field}[{i}]"), item, found);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                disk_urls(&format!("{field}.{key}"), item, found);
            }
        }
        _ => {}
    }
}

/// What the node's disks refuse of `job`'s volumes, each by its field: a serial no disk has,
/// the boot disk, a sink's volume served read only.
fn volume_refusals(job: &Job, attached: &[NodeVolume], refused: &mut Vec<SpecError>) {
    let named: Vec<&str> = attached
        .iter()
        .filter(|v| !v.boot)
        .map(|v| v.serial.as_str())
        .collect();
    let has = if named.is_empty() {
        "it has none".to_string()
    } else {
        format!("its volumes are {}", named.join(", "))
    };
    for (i, task) in job.tasks.iter().enumerate() {
        let write = task.role == moruna_job::Role::Sink;
        let mut found = Vec::new();
        for (key, value) in &task.settings {
            disk_urls(&format!("{}.{key}", job.field(i)), value, &mut found);
        }
        for (field, serial) in found {
            let reason = match attached.iter().find(|v| v.serial == serial) {
                None => format!(
                    "volume `{serial}` is not attached to this node: {has}; attach a disk with \
                     that serial, or name one of them"
                ),
                Some(v) if v.boot => format!(
                    "volume `{serial}` is the disk this node booted from, which is never a job's \
                     volume: {has}"
                ),
                Some(v) if write && v.read_only => format!(
                    "volume `{serial}` is read only on this node: a sink writes a volume the \
                     node can write"
                ),
                Some(_) => continue,
            };
            refused.push(SpecError::new(field, reason));
        }
    }
}

/// Every `disk://` serial in `value`.
fn disk_serials<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => found.extend(disk_serial(text)),
        Value::Array(items) => items.iter().for_each(|item| disk_serials(item, found)),
        Value::Object(map) => map.values().for_each(|item| disk_serials(item, found)),
        _ => {}
    }
}

/// What `job` needs of a node ([`NodeNeeds`]).
pub fn needs(job: &Job) -> NodeNeeds {
    let mut hosts = Vec::new();
    let store = &job.object_store;
    if let Some(host) = store
        .s3
        .as_ref()
        .and_then(|s3| s3.endpoint.as_deref())
        .and_then(url_host)
    {
        hosts.push(host);
    }
    if let Some(account) = store.azure.as_ref().and_then(|a| a.account.as_deref()) {
        hosts.push(format!(
            "{}.blob.core.windows.net",
            account.to_ascii_lowercase()
        ));
    }
    if store.gcs.is_some() {
        hosts.push("storage.googleapis.com".to_string());
    }
    hosts.sort();
    hosts.dedup();
    let mut volumes: std::collections::BTreeMap<String, bool> = Default::default();
    let mut functions: Vec<String> = Vec::new();
    for task in &job.tasks {
        let write = task.role == moruna_job::Role::Sink;
        let mut found = Vec::new();
        for value in task.settings.values() {
            disk_serials(value, &mut found);
        }
        for serial in found {
            *volumes.entry(serial.to_string()).or_insert(write) |= write;
        }
        if task.role == moruna_job::Role::Kernel && !functions.contains(&task.kind) {
            functions.push(task.kind.clone());
        }
    }
    NodeNeeds {
        hosts,
        volumes: volumes
            .into_iter()
            .map(|(serial, write)| VolumeNeed { serial, write })
            .collect(),
        functions,
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
    if let Some(attached) = &node.volumes {
        volume_refusals(job, attached, &mut refused);
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
