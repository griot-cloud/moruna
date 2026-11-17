//! Node mode's rules, field by field.

use super::*;
use moruna_job::JobSpec;
use serde_json::json;

fn node() -> Node {
    Node {
        channel: "channel:engine".to_string(),
    }
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

#[test]
fn schemes_are_read_without_regard_to_case_and_only_at_the_start() {
    assert!(is_file_url("file:///x") && is_file_url("File:/x") && !is_file_url("fil"));
    assert!(!is_file_url("s3://bucket/file://x"));
    assert!(is_host_socket("unix:/x") && is_host_socket("VSOCK://2:1"));
    assert!(!is_host_socket("https://unix.example.com") && !is_host_socket("uni"));
}
