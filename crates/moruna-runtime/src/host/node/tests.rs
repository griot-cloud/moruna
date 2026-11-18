//! Node mode's rules, field by field.

use super::*;
use moruna_job::JobSpec;
use serde_json::json;

fn node() -> Node {
    Node::on("channel:engine")
}

fn job(document: Value) -> Job {
    Job::from_v1(&JobSpec::from_value(document).expect("a version 1 document"))
}

fn sound() -> Value {
    json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "disk://vol-1/in/"},
        "sink": {"kind": "parquet", "url": "s3://bucket/out/"},
        "object_store": {"s3": {"endpoint": "https://s3.example.com",
                                "secret_access_key": {"secret": "aws"}}},
    })
}

#[test]
fn a_sound_document_is_taken_and_filled_in() {
    let mut taken = job(sound());
    assert!(refusals(&taken, &node()).is_empty());
    admit(&mut taken, &node(), || {
        Some("0123456789abcdef0123456789abcdef".into())
    })
    .unwrap();
    assert_eq!(taken.report.socket.as_deref(), Some("channel:engine"));
    assert_eq!(
        taken.id.as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(taken.staging.dir, None, "no staging directory");
    // A run ID the document names is kept, and so is a socket that is the channel.
    let mut named = sound();
    named["run_id"] = json!("ffffffffffffffffffffffffffffffff");
    named["report"] = json!({"socket": "channel:engine"});
    let mut taken = job(named);
    admit(&mut taken, &node(), || {
        unreachable!("the document names its run")
    })
    .unwrap();
    assert_eq!(
        taken.id.as_deref(),
        Some("ffffffffffffffffffffffffffffffff")
    );
    // No run ID to be had.
    let mut taken = job(sound());
    let refused = admit(&mut taken, &node(), || None).unwrap_err();
    assert_eq!(refused.field, "run_id");
}

/// The fields `document` is refused by.
fn refused(document: Value) -> Vec<String> {
    refusals(&job(document), &node())
        .into_iter()
        .map(|refusal| refusal.field)
        .collect()
}

#[test]
fn what_a_node_refuses_is_refused_by_its_field() {
    let with = |change: &dyn Fn(&mut Value)| {
        let mut document = sound();
        change(&mut document);
        refused(document)
    };
    assert_eq!(
        with(&|d| d["source"]["url"] = json!("file:///data/in/")),
        ["source.url"]
    );
    assert_eq!(
        with(&|d| d["sink"]["options"] = json!({"spill": ["s3://b/", "FILE:/tmp/x"]})),
        ["sink.options.spill[1]"]
    );
    assert_eq!(
        with(&|d| d["object_store"]["local_root"] = json!("/data")),
        ["object_store.local_root"]
    );
    for endpoint in ["unix:///run/egress.sock", "vsock://2:5000", "UNIX:/x"] {
        assert_eq!(
            with(&|d| d["object_store"]["s3"]["endpoint"] = json!(endpoint)),
            ["object_store.s3.endpoint"],
            "{endpoint}"
        );
    }
    assert_eq!(
        with(&|d| d["report"] = json!({"socket": "unix:///run/moruna.sock"})),
        ["report.socket"]
    );
    assert_eq!(
        with(&|d| d["report"] = json!({"socket": "channel:another"})),
        ["report.socket"]
    );
    assert_eq!(with(&|d| d["resume"] = json!("auto")), ["resume"]);
    // AWS's own S3 with no region: its host is not known.
    assert_eq!(
        with(&|d| d["object_store"]["s3"] = json!({"bucket": "b"})),
        ["object_store.s3.region"]
    );
    // Every one at once, in the order the document is read; the first is what admit refuses.
    let mut all = sound();
    all["source"]["url"] = json!("file:///in");
    all["resume"] = json!("auto");
    all["report"] = json!({"socket": "vsock://2:1"});
    assert_eq!(
        refused(all.clone()),
        ["source.url", "report.socket", "resume"]
    );
    let refusal = admit(&mut job(all), &node(), || None).unwrap_err();
    assert_eq!(refusal.field, "source.url");
    assert!(
        refusal.reason.contains("outside the job's volumes"),
        "{}",
        refusal.reason
    );
}

/// Given the most a job may have on the node, a larger budget is refused and none is all of it.
#[test]
fn a_nodes_budget_bounds_the_jobs_and_fills_it_in() {
    let bounded = Node {
        budget_max: Some(1 << 30),
        ..node()
    };
    let with_budget = |bytes: u64| {
        let mut document = sound();
        document["budget"] = json!({"memory_bytes": bytes});
        job(document)
    };
    // At the bound, taken as it is.
    let mut at = with_budget(1 << 30);
    assert!(refusals(&at, &bounded).is_empty());
    admit(&mut at, &bounded, || Some("0".repeat(32))).unwrap();
    assert_eq!(at.budget.memory_bytes, Some(1 << 30));
    // Over it, refused by its field, saying what the node allows.
    let over = with_budget((1 << 30) + 4096);
    let refused = refusals(&over, &bounded);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field, "budget.memory_bytes");
    assert!(
        refused[0].reason.contains("1073741824 bytes") && refused[0].reason.contains("768 MiB"),
        "{}",
        refused[0].reason
    );
    // None: all of it, and so the budget the check reports.
    let mut none = job(sound());
    assert_eq!(budget(&none, &bounded), Some(1 << 30));
    admit(&mut none, &bounded, || Some("0".repeat(32))).unwrap();
    assert_eq!(none.budget.memory_bytes, Some(1 << 30));
    // With no bound, the document's own, or none.
    assert_eq!(budget(&job(sound()), &node()), None);
    assert_eq!(budget(&with_budget(4096), &node()), Some(4096));
    assert!(refusals(&with_budget(u64::MAX), &node()).is_empty());
}

/// What a job needs of a node: each kind, and none.
#[test]
fn a_jobs_needs_are_its_hosts_volumes_and_functions() {
    // None: no object store, no volume, no function.
    let none = job(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "s3://bucket/in/"},
        "sink": {"kind": "parquet", "url": "s3://bucket/out/"},
    }));
    assert_eq!(needs(&none), NodeNeeds::default());
    // Each kind.
    let every = job(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": ["disk://vol-a/in/1.parquet", "DISK://vol-b/2"]},
        "kernels": [
            {"kind": "function", "function": "taxi.clean"},
            {"kind": "std", "name": "select", "args": {"columns": ["a"]}},
            {"kind": "function", "function": "taxi.score"},
            {"kind": "function", "function": "taxi.clean"},
        ],
        "sink": {"kind": "parquet", "url": "disk://vol-c/out/",
                 "options": {"spill": ["disk://vol-a/spill/"]}},
        "object_store": {
            "s3": {"endpoint": "HTTPS://S3.Example.com:9000/base", "region": "r"},
            "azure": {"account": "MyAccount", "container": "c"},
            "gcs": {"bucket": "g"},
        },
    }));
    assert_eq!(
        needs(&every),
        NodeNeeds {
            hosts: vec![
                "myaccount.blob.core.windows.net".to_string(),
                "s3.example.com".to_string(),
                "storage.googleapis.com".to_string(),
            ],
            volumes: vec![
                VolumeNeed {
                    serial: "vol-a".into(),
                    write: true
                },
                VolumeNeed {
                    serial: "vol-b".into(),
                    write: false
                },
                VolumeNeed {
                    serial: "vol-c".into(),
                    write: true
                },
            ],
            functions: vec!["taxi.clean".to_string(), "taxi.score".to_string()],
        }
    );
    // AWS's own S3: its region's host, path-style.
    let aws = job(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "s3://b/in/"},
        "sink": {"kind": "parquet", "url": "s3://b/out/"},
        "object_store": {"s3": {"region": "EU-West-1"}},
    }));
    assert_eq!(needs(&aws).hosts, ["s3.eu-west-1.amazonaws.com"]);
    assert!(refusals(&aws, &node()).is_empty());
    // An endpoint with no host of its own (a host socket, or AWS's default) gives none.
    let socket = job(json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "s3://b/in/"},
        "sink": {"kind": "parquet", "url": "s3://b/out/"},
        "object_store": {"s3": {"endpoint": "unix:///run/egress.sock"}},
    }));
    assert!(needs(&socket).hosts.is_empty());
}

#[test]
fn a_urls_host_is_its_authoritys_name() {
    for (url, host) in [
        ("https://s3.example.com", Some("s3.example.com")),
        ("http://user@MinIO.local:9000/x?y", Some("minio.local")),
        ("https://[::1]:9000/", Some("::1")),
        ("https://", None),
        ("unix:///run/s.sock", None),
        ("s3.example.com", None),
    ] {
        assert_eq!(url_host(url).as_deref(), host, "{url}");
    }
    assert_eq!(disk_serial("disk://serial-1/a/b"), Some("serial-1"));
    assert_eq!(disk_serial("disk:///a"), None);
    assert_eq!(disk_serial("file:///a"), None);
}

#[test]
fn schemes_are_read_without_regard_to_case_and_only_at_the_start() {
    assert!(is_file_url("file:///x") && is_file_url("File:/x") && !is_file_url("fil"));
    assert!(!is_file_url("s3://bucket/file://x"));
    assert!(is_host_socket("unix:/x") && is_host_socket("VSOCK://2:1"));
    assert!(!is_host_socket("https://unix.example.com") && !is_host_socket("uni"));
}
