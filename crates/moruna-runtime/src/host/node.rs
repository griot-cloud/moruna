//! Node mode (system design 3A, 3B): how `moruna serve` takes a document on a Moruna node, where
//! it serves on a channel of the kernel (`--listen channel:<slot>`) whose other end is the node's
//! `api`.
//!
//! - It fills in what the node decides when the document leaves it out: `report.socket` (the
//!   channel), `staging` (none: a document that names no staging directory has none) and the
//!   run's ID.
//! - It refuses, each by its field and with a sentence that says what to change: a host socket
//!   (`report.socket` naming anything but the channel, an object store's endpoint on `unix://` or
//!   `vsock://`), an object store's endpoint that is not exactly `https://<host name>[:443]` (an
//!   `http://` one, an address, another port, a path, a credential, or no URL), since the job's
//!   network reaches its hosts over TLS, by name, at 443 alone (system design 3D), a `file://`
//!   path outside the job's volumes, and `resume`. A job on a node reads and writes the volumes the cloud attaches (`disk://`,
//!   item 3.3) and object storage; it has no file system of its own, so in this release every
//!   `file://` path is outside its volumes, and so is a root for them (`object_store.local_root`).
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

/// What the engine keeps of a job's account for itself: the supervisor and the Linux
/// personality's process (system design 3A).
pub const ENGINE_OVERHEAD: u64 = 128 << 20;

/// How a job's whole memory, its account, is spent on a node (system design 3A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    /// The engine's own: [`ENGINE_OVERHEAD`], or all of a budget smaller than it.
    pub overhead: u64,
    /// In-memory scratch: a quarter of what is left, none when the job names a staging volume.
    pub scratch: u64,
    /// The rest: what `moruna` sizes itself to.
    pub arena: u64,
}

/// [`Sizes`] of a budget of `whole` bytes, in whole pages; `staging_volume` when the job names
/// one, so that it needs no in-memory scratch.
pub fn sizes(whole: u64, staging_volume: bool) -> Sizes {
    let overhead = ENGINE_OVERHEAD.min(whole);
    let rest = whole - overhead;
    let scratch = if staging_volume {
        0
    } else {
        (rest / 4) & !4095
    };
    Sizes {
        overhead,
        scratch,
        arena: rest - scratch,
    }
}

/// Whether `job` names a staging volume: a `disk://` staging directory, the volume its check's
/// needs mark `staging` (item 3.3).
pub fn names_staging_volume(job: &Job) -> bool {
    job.staging
        .dir
        .as_deref()
        .is_some_and(|dir| disk_serial(dir).is_some())
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

/// The job's network (system design 3D, item 3.4): on the Moruna kernel, the channel
/// `node-admission` minted for the hosts the job's check named, installed as the object-store
/// connector's network, its sockets charged to the engine's own account. `Ok(false)` when the
/// engine was given none (its job names no host, or it runs off the kernel); an error when it
/// was given one it cannot use.
pub fn install_network() -> Result<bool, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use moruna_storage::net::native;
        if !moruna_micro::start::slots()
            .iter()
            .any(|slot| slot == native::SLOT)
        {
            return Ok(false);
        }
        let account = moruna_micro::start::take("account")
            .ok_or("the engine was given a network and no account to charge its sockets to")?;
        if !native::install_from_start(moruna_sys::Account(account)) {
            return Err("the job's network was taken or installed already".into());
        }
        Ok(true)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    Ok(false)
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
    /// both read and written is written; the one `staging.dir` names is written, and its
    /// staging volume. By serial.
    pub volumes: Vec<VolumeNeed>,
    /// The registered functions it uses (3E), each once, in the order the document names them:
    /// each function task of kind `function` names one by its `function` setting.
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

/// What the job's network reaches, which an object store's endpoint on a node is held to
/// (system design 3D): HTTPS, to a host by its name, at port 443.
const REACHED: &str = "a job on a node reaches object storage over HTTPS, by host name, at port \
                       443 and nothing else: name the endpoint as https://<host name>";

/// Why a node refuses `endpoint`, an object store's that is not a host socket: anything but
/// exactly `https://<host name>` or `https://<host name>:443`. `None` when it is one. The
/// endpoint itself is never said back, since its authority may hold a credential.
fn endpoint_refusal(endpoint: &str) -> Option<String> {
    let why = match endpoint.split_once("://") {
        None => "the endpoint is not a URL".to_string(),
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("http") => {
            "the endpoint is plain http, and the job's network speaks only TLS".to_string()
        }
        Some((scheme, _)) if !scheme.eq_ignore_ascii_case("https") => {
            "the endpoint is not an https:// URL".to_string()
        }
        Some((_, authority)) if authority.contains(['/', '?', '#', '@']) => {
            "the endpoint names more than a host and its port (a path, a query or a credential)"
                .to_string()
        }
        Some((_, authority)) if authority.starts_with('[') => {
            "the endpoint names an address, and the job's network reaches hosts by name".to_string()
        }
        Some((_, authority)) => {
            let (host, port) = match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            };
            match port.map(str::parse::<u16>) {
                Some(Ok(443)) | None => {}
                Some(Ok(port)) => {
                    return Some(format!("the endpoint is at port {port}: {REACHED}"));
                }
                Some(Err(_)) => {
                    return Some(format!("the endpoint names no port number: {REACHED}"));
                }
            }
            if host.parse::<std::net::IpAddr>().is_ok() {
                "the endpoint names an address, and the job's network reaches hosts by name"
                    .to_string()
            } else if !is_host_name(host) {
                "the endpoint names no host name".to_string()
            } else {
                return None;
            }
        }
    };
    Some(format!("{why}: {REACHED}"))
}

/// Whether `host` is a DNS host name: 1 to 253 bytes of labels of 1 to 63 letters, digits and
/// `-`, none starting or ending with `-`, the last not all digits (so no address passes).
fn is_host_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    (1..=253).contains(&host.len())
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && labels
            .last()
            .is_some_and(|last| !last.bytes().all(|b| b.is_ascii_digit()))
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

/// Whether `value` holds an `s3://` URL, in any case of the scheme.
fn names_s3(value: &Value) -> bool {
    match value {
        Value::String(text) => text
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("s3://")),
        Value::Array(items) => items.iter().any(names_s3),
        Value::Object(map) => map.values().any(names_s3),
        _ => false,
    }
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
    if let Some(serial) = job.staging.dir.as_deref().and_then(disk_serial) {
        let reason = match attached.iter().find(|v| v.serial == serial) {
            None => Some(format!(
                "staging volume `{serial}` is not attached to this node: {has}; attach a disk \
                 with that serial, or name one of them"
            )),
            Some(v) if v.boot => Some(format!(
                "staging volume `{serial}` is the disk this node booted from, which is never a \
                 job's volume: {has}"
            )),
            Some(v) if v.read_only => Some(format!(
                "staging volume `{serial}` is read only on this node: a job stages on a volume \
                 the node can write"
            )),
            Some(_) => None,
        };
        refused.extend(reason.map(|reason| SpecError::new("staging.dir", reason)));
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
    if let Some(s3) = &store.s3 {
        // With no endpoint the engine asks AWS's own S3, path-style, at its region's host.
        let host = match (&s3.endpoint, &s3.region) {
            (Some(endpoint), _) => url_host(endpoint),
            (None, Some(region)) => {
                Some(format!("s3.{}.amazonaws.com", region.to_ascii_lowercase()))
            }
            (None, None) => None,
        };
        hosts.extend(host);
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
    // Each serial: whether the job writes it, and whether it stages there.
    let mut volumes: std::collections::BTreeMap<String, (bool, bool)> = Default::default();
    if let Some(serial) = job.staging.dir.as_deref().and_then(disk_serial) {
        volumes.insert(serial.to_string(), (true, true));
    }
    let mut functions: Vec<String> = Vec::new();
    for task in &job.tasks {
        let write = task.role == moruna_job::Role::Sink;
        let mut found = Vec::new();
        for value in task.settings.values() {
            disk_serials(value, &mut found);
        }
        for serial in found {
            volumes
                .entry(serial.to_string())
                .or_insert((write, false))
                .0 |= write;
        }
        if let Some(name) = task.function_name()
            && !functions.iter().any(|f| f == name)
        {
            functions.push(name.to_string());
        }
    }
    NodeNeeds {
        hosts,
        volumes: volumes
            .into_iter()
            .map(|(serial, (write, staging))| VolumeNeed {
                serial,
                write,
                staging,
            })
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
    // AWS's own S3 is its region's host: one with neither endpoint nor region, or `s3://`
    // URLs with no `object_store.s3` at all, would leave a host the node cannot name.
    let unnamed = match &job.object_store.s3 {
        Some(s3) => s3.endpoint.is_none() && s3.region.is_none(),
        None => job.tasks.iter().any(|task| {
            let mut found = false;
            for value in task.settings.values() {
                found |= names_s3(value);
            }
            found
        }),
    };
    if unnamed {
        refused.push(SpecError::new(
            "object_store.s3.region",
            "a job on a node that uses AWS's own S3 names its region: the node lets the job \
             reach s3.<region>.amazonaws.com and nothing else",
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
        let Some(endpoint) = endpoint else { continue };
        if is_host_socket(endpoint) {
            refused.push(SpecError::new(
                field,
                "a host socket is not reachable from a node: name the object store's endpoint by \
                 its URL",
            ));
        } else if let Some(why) = endpoint_refusal(endpoint) {
            // The job's network reaches its hosts over HTTPS, by name, at 443 alone (system
            // design 3D): any other endpoint would pass here and fail at its first request.
            refused.push(SpecError::new(field, why));
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
    // The budget is the job's whole memory, its account exactly (system design 3A): the
    // engine sizes itself to the arena's share of it, never the whole.
    if let Some(whole) = budget(job, node) {
        job.budget.memory_bytes = Some(sizes(whole, names_staging_volume(job)).arena);
    }
    if job.id.is_none() {
        job.id = Some(mint().ok_or_else(|| SpecError::new("run_id", "no run ID could be made"))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
