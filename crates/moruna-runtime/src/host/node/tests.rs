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
    // Nor is it with s3:// URLs and no object_store.s3 at all.
    assert_eq!(
        with(&|d| {
            d["object_store"] = json!({});
            d["sink"]["url"] = json!("S3://bucket/out/");
        }),
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

/// The job's network reaches object storage over TLS, by host name, at port 443 alone (system
/// design 3D, item 3.4): an endpoint that is not exactly `https://<host name>[:443]` is refused
/// at validate, by its field, not at its first request, and is never said back.
#[test]
fn an_endpoint_not_https_to_a_host_name_at_443_is_refused_by_its_field() {
    let with = |endpoint: &str| {
        let mut document = sound();
        document["object_store"]["s3"]["endpoint"] = json!(endpoint);
        refusals(&job(document), &node())
    };
    for endpoint in [
        "https://objects.test",
        "https://objects.test:443",
        "HTTPS://Objects.Test:443",
        "https://s3.eu-west-1.amazonaws.com",
        "https://localhost",
        "https://1a.example",
    ] {
        assert!(
            with(endpoint).is_empty(),
            "{endpoint}: {:?}",
            with(endpoint)
        );
    }
    for (endpoint, why) in [
        // Another port, or none that is a number.
        ("https://objects.test:8443", "the endpoint is at port 8443"),
        ("https://objects.test:", "the endpoint names no port number"),
        (
            "https://objects.test:x443",
            "the endpoint names no port number",
        ),
        (
            "https://objects.test:70000",
            "the endpoint names no port number",
        ),
        // Plain http, at any port: the job's network speaks only TLS.
        ("http://objects.test", "the endpoint is plain http"),
        ("http://objects.test:443", "the endpoint is plain http"),
        ("HTTP://objects.test", "the endpoint is plain http"),
        // An address: egress rules are by name.
        ("https://10.0.0.5", "the endpoint names an address"),
        ("https://10.0.0.5:443", "the endpoint names an address"),
        ("https://[2001:db8::1]", "the endpoint names an address"),
        ("https://[2001:db8::1]:443", "the endpoint names an address"),
        // More than a host and its port.
        (
            "https://objects.test/",
            "the endpoint names more than a host",
        ),
        (
            "https://objects.test/bucket",
            "the endpoint names more than a host",
        ),
        (
            "https://objects.test?x=1",
            "the endpoint names more than a host",
        ),
        (
            "https://key:secret@objects.test",
            "the endpoint names more than a host",
        ),
        // No URL, another scheme, or no host name.
        ("objects.test", "the endpoint is not a URL"),
        ("objects.test:443", "the endpoint is not a URL"),
        ("ftp://objects.test", "the endpoint is not an https:// URL"),
        ("https://", "the endpoint names no host name"),
        ("https://objects_test", "the endpoint names no host name"),
        ("https://-objects.test", "the endpoint names no host name"),
        ("https://objects..test", "the endpoint names no host name"),
        ("https://objects.test.", "the endpoint names no host name"),
        ("https://1.2.3", "the endpoint names no host name"),
    ] {
        let refused = with(endpoint);
        assert_eq!(refused.len(), 1, "{endpoint}: {refused:?}");
        assert_eq!(refused[0].field, "object_store.s3.endpoint");
        let reason = &refused[0].reason;
        assert!(reason.starts_with(why), "{endpoint}: {reason}");
        assert!(
            reason.ends_with(
                "a job on a node reaches object storage over HTTPS, by host name, at port 443 \
                 and nothing else: name the endpoint as https://<host name>"
            ),
            "{endpoint}: {reason}"
        );
        // The endpoint is not said back: its authority may hold a credential.
        assert!(
            !reason.contains("objects.test") && !reason.contains("secret"),
            "{endpoint}: {reason}"
        );
    }
    let long = format!("https://{}.test", "a".repeat(64));
    assert_eq!(with(&long).len(), 1, "a label of 64 bytes");
    let longest = format!("https://{}.test", vec!["a".repeat(63); 4].join("."));
    assert_eq!(with(&longest).len(), 1, "a host of more than 253 bytes");
    // A host socket is refused as one, once.
    let socket = with("unix:///run/egress.sock");
    assert_eq!(socket.len(), 1);
    assert!(socket[0].reason.contains("host socket"), "{socket:?}");
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
    // The engine sizes itself to the arena's share of the whole.
    assert_eq!(at.budget.memory_bytes, Some(sizes(1 << 30, false).arena));
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
    assert_eq!(none.budget.memory_bytes, Some(sizes(1 << 30, false).arena));
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
            // GCS is not reached from a node: its settings mint no host (item 3.5).
            hosts: vec![
                "myaccount.blob.core.windows.net".to_string(),
                "s3.example.com".to_string(),
            ],
            volumes: vec![
                VolumeNeed {
                    serial: "vol-a".into(),
                    write: true,
                    staging: false,
                },
                VolumeNeed {
                    serial: "vol-b".into(),
                    write: false,
                    staging: false,
                },
                VolumeNeed {
                    serial: "vol-c".into(),
                    write: true,
                    staging: false,
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

/// What a job's network may reach comes from its object-store settings and nothing else (system
/// design 3D, item 3.5): exactly the S3 endpoint's host, path-style or the bucket's own host,
/// and exactly `<account>.blob.core.windows.net`; never a host a URL, a credential or another
/// setting names.
#[test]
fn the_hosts_a_node_mints_are_exactly_the_endpoints_and_the_accounts() {
    let with = |store: Value| {
        needs(&job(json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet",
                       "url": ["s3://in/taxi/", "az://c/taxi/", "https://elsewhere.example.com/x"]},
            "sink": {"kind": "parquet", "url": "s3://out/score/"},
            "object_store": store,
        })))
        .hosts
    };
    // MinIO as the boot has it: path-style, the endpoint's host.
    assert_eq!(
        with(
            json!({"s3": {"endpoint": "https://minio.test", "region": "us-east-1",
                           "access_key_id": {"secret": "s3-key"},
                           "secret_access_key": {"secret": "s3-secret"}}})
        ),
        ["minio.test"]
    );
    // Virtual-host style: the bucket's own host, which the endpoint names.
    assert_eq!(
        with(
            json!({"s3": {"endpoint": "https://in.objects.example.com:443",
                           "virtual_hosted_style_request": true}})
        ),
        ["in.objects.example.com"]
    );
    // Azure Blob by account key or by SAS token: the account's host, whatever the credential.
    for credential in [
        json!({"access_key": {"secret": "blob-key"}}),
        json!({"sas_token": {"secret": "blob-sas"}}),
    ] {
        let mut azure = json!({"account": "moruna35", "container": "c"});
        azure
            .as_object_mut()
            .unwrap()
            .extend(credential.as_object().unwrap().clone());
        assert_eq!(
            with(json!({"azure": azure})),
            ["moruna35.blob.core.windows.net"]
        );
    }
    // Both stores: both hosts, each once, sorted; nothing for GCS.
    assert_eq!(
        with(json!({"s3": {"endpoint": "https://minio.test"},
                    "azure": {"account": "moruna35"},
                    "gcs": {"bucket": "g"}})),
        ["minio.test", "moruna35.blob.core.windows.net"]
    );
}

/// A job on a node that uses Azure Blob names a storage account Azure could have: with none, or
/// with a name that is not one, the host it would reach is not known, and it is refused by its
/// field.
#[test]
fn azure_blob_on_a_node_names_its_account() {
    let with = |store: Value, url: &str| {
        refused(json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": url},
            "sink": {"kind": "count"},
            "object_store": store,
        }))
    };
    assert!(with(json!({"azure": {"account": "moruna35"}}), "az://c/in/").is_empty());
    for account in ["Moruna35", "ab", "a.evil.example", &"a".repeat(25)] {
        assert_eq!(
            with(json!({"azure": {"account": account}}), "az://c/in/"),
            ["object_store.azure.account"],
            "{account}"
        );
    }
    assert_eq!(
        with(json!({"azure": {"container": "c"}}), "az://c/in/"),
        ["object_store.azure.account"]
    );
    for url in ["az://c/in/", "ABFS://c/in/", "azure://c/in/"] {
        assert_eq!(
            with(json!({}), url),
            ["object_store.azure.account"],
            "{url}"
        );
    }
    assert!(with(json!({}), "disk://vol/in/").is_empty());
    assert!(is_storage_account("abc") && is_storage_account(&"0".repeat(24)));
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

/// A job's whole memory is its account: the engine's 128 MiB, a quarter of the rest as
/// scratch unless it names a staging volume, and the arena the rest, so that a job at its
/// budget never needs more than its account.
#[test]
fn a_jobs_budget_is_spent_on_the_engine_its_scratch_and_its_arena() {
    let mib = 1u64 << 20;
    let at = sizes(1 << 30, false);
    assert_eq!(
        at,
        Sizes {
            overhead: 128 * mib,
            scratch: 224 * mib,
            arena: 672 * mib
        }
    );
    assert_eq!(at.overhead + at.scratch + at.arena, 1 << 30);
    let volume = sizes(1 << 30, true);
    assert_eq!((volume.scratch, volume.arena), (0, 896 * mib));
    // Pages, and a budget smaller than the overhead leaves nothing else.
    let odd = sizes((1 << 30) + 4096, false);
    assert_eq!(odd.overhead + odd.scratch + odd.arena, (1 << 30) + 4096);
    assert_eq!(odd.scratch % 4096, 0);
    assert_eq!(
        sizes(64 * mib, false),
        Sizes {
            overhead: 64 * mib,
            scratch: 0,
            arena: 0
        }
    );
    // A disk:// staging directory is a volume, the one the check's needs mark staging (3.3);
    // no staging directory is none.
    let mut staged = sound();
    staged["staging"] = json!({"dir": "disk://vol-s/spill/"});
    assert!(names_staging_volume(&job(staged.clone())));
    assert!(
        needs(&job(staged))
            .volumes
            .iter()
            .any(|v| v.staging && v.serial == "vol-s")
    );
    assert!(!names_staging_volume(&job(sound())));
}

#[test]
fn schemes_are_read_without_regard_to_case_and_only_at_the_start() {
    assert!(is_file_url("file:///x") && is_file_url("File:/x") && !is_file_url("fil"));
    assert!(!is_file_url("s3://bucket/file://x"));
    assert!(is_host_socket("unix:/x") && is_host_socket("VSOCK://2:1"));
    assert!(!is_host_socket("https://unix.example.com") && !is_host_socket("uni"));
}

fn attached() -> Vec<NodeVolume> {
    parse_volumes("vol-1 1073741824 ro data\nvol-out 2147483648 rw data\nboot 4294967296 ro boot\n")
        .expect("the node's volumes")
}

/// The node's disks as `node-admission` writes them, and every line it does not.
#[test]
fn the_nodes_volumes_are_a_line_a_disk() {
    let volumes = attached();
    assert_eq!(
        volumes[0],
        NodeVolume {
            serial: "vol-1".into(),
            bytes: 1 << 30,
            read_only: true,
            boot: false,
        }
    );
    assert!(volumes[2].boot && !volumes[1].read_only);
    assert_eq!(parse_volumes("").unwrap(), Vec::new());
    assert_eq!(parse_volumes("\n  \n").unwrap(), Vec::new());
    for bad in [
        "vol-1 12 ro",
        "vol-1 twelve ro data",
        "vol-1 12 rx data",
        "vol-1 12 ro root",
        "vol-1 12 ro data extra",
    ] {
        assert!(parse_volumes(bad).unwrap_err().contains("line 1"), "{bad}");
    }
}

/// Given the node's disks, a job's volumes are refused by their field: a serial no disk has,
/// the boot disk, a sink's volume served read only; a node whose disks are not known refuses none.
#[test]
fn a_volume_no_disk_has_or_the_boot_disk_is_refused_by_its_field() {
    let on = Node {
        volumes: Some(attached()),
        ..node()
    };
    assert!(refusals(&job(sound()), &on).is_empty(), "vol-1 is attached");
    let with = |source: &str, sink: &str| {
        let mut document = sound();
        document["source"]["url"] = json!(source);
        document["sink"]["url"] = json!(sink);
        job(document)
    };
    let fields = |job: &Job, node: &Node| -> Vec<(String, String)> {
        refusals(job, node)
            .into_iter()
            .map(|r| (r.field, r.reason))
            .collect()
    };
    let unknown = fields(&with("disk://vol-9/in/", "disk://vol-out/out/"), &on);
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    assert!(
        unknown[0].1.contains("volume `vol-9` is not attached"),
        "{unknown:?}"
    );
    assert!(
        unknown[0].1.contains("its volumes are vol-1, vol-out"),
        "{unknown:?}"
    );
    let boot = fields(&with("disk://boot/in/", "s3://bucket/out/"), &on);
    assert_eq!(boot.len(), 1, "{boot:?}");
    assert!(
        boot[0].1.contains("the disk this node booted from"),
        "{boot:?}"
    );
    assert_eq!(boot[0].0, unknown[0].0, "the source's url both times");
    let read_only = fields(&with("disk://vol-out/in/", "disk://vol-1/out/"), &on);
    assert_eq!(read_only.len(), 1, "{read_only:?}");
    assert!(
        read_only[0].1.contains("read only on this node"),
        "{read_only:?}"
    );
    let none = Node {
        volumes: Some(Vec::new()),
        ..node()
    };
    let nothing = fields(&job(sound()), &none);
    assert!(nothing[0].1.contains("it has none"), "{nothing:?}");
    assert!(
        refusals(&with("disk://vol-9/in/", "disk://boot/out/"), &node()).is_empty(),
        "disks not known: nothing refused"
    );
}

/// The volume `staging.dir` names is the job's staging volume: written, and said so, and
/// refused as any volume is when the node has no such disk.
#[test]
fn a_staging_volume_is_written_and_said_to_be_staging() {
    let mut document = sound();
    document["staging"] = json!({"dir": "disk://vol-out/stage/"});
    let staged = job(document.clone());
    let volumes = needs(&staged).volumes;
    assert_eq!(
        volumes,
        vec![
            VolumeNeed {
                serial: "vol-1".into(),
                write: false,
                staging: false,
            },
            VolumeNeed {
                serial: "vol-out".into(),
                write: true,
                staging: true,
            },
        ]
    );
    // Only a staging volume says so on the wire.
    let line = serde_json::to_string(&volumes).unwrap();
    assert_eq!(
        line,
        r#"[{"serial":"vol-1","write":false},{"serial":"vol-out","write":true,"staging":true}]"#
    );
    let back: Vec<VolumeNeed> = serde_json::from_str(&line).unwrap();
    assert_eq!(back, volumes);
    // A source's volume that is also the staging volume is written.
    document["staging"] = json!({"dir": "disk://vol-1/stage/"});
    assert_eq!(
        needs(&job(document.clone())).volumes,
        vec![VolumeNeed {
            serial: "vol-1".into(),
            write: true,
            staging: true,
        }]
    );
    // Against the node's disks: none with the serial, the boot disk, a read-only one.
    let on = Node {
        volumes: Some(attached()),
        ..node()
    };
    for (dir, said) in [
        ("disk://vol-9/s/", "is not attached"),
        ("disk://boot/s/", "booted from"),
        ("disk://vol-1/s/", "read only on this node"),
    ] {
        document["staging"] = json!({ "dir": dir });
        let refused = refusals(&job(document.clone()), &on);
        let staging: Vec<_> = refused
            .iter()
            .filter(|r| r.field == "staging.dir")
            .collect();
        assert_eq!(staging.len(), 1, "{dir}: {refused:?}");
        assert!(staging[0].reason.contains(said), "{dir}: {refused:?}");
    }
    document["staging"] = json!({"dir": "disk://vol-out/s/"});
    assert!(refusals(&job(document), &on).is_empty());
}

#[test]
fn off_the_kernel_an_engine_has_no_network_to_install() {
    // The job's network is a channel of the Moruna kernel: anywhere else there is none, and the
    // job's object stores are built as they are off a node (item 3.4).
    assert_eq!(install_network(), Ok(false));
}
