//! The job document as 0.6.2 writes it: role 1 is `Function`, the job carries its name, and no
//! credential reaches a document.

use moruna_job::{Edge, Job, JobSpec, KindDoc, Role, Task, fb};
use serde_json::json;

fn entry(value: serde_json::Value) -> KindDoc {
    KindDoc::from_value(value).expect("an entry")
}

fn job() -> Job {
    Job::new(
        vec![
            Task::source(
                "rows",
                entry(json!({"kind": "parquet", "url": "file:///in"})),
            ),
            Task::kernel(
                "keep",
                entry(json!({"kind": "std", "name": "select",
                                                "args": {"columns": ["a"]}})),
            ),
            Task::sink(
                "out",
                entry(json!({"kind": "parquet", "url": "file:///out"})),
            ),
        ],
        vec![Edge::between("rows", "keep"), Edge::between("keep", "out")],
    )
}

/// The role a function has is named `function` wherever it is written, and keeps the value 1 in
/// the FlatBuffers enum.
#[test]
fn the_function_role_is_named_function() {
    assert_eq!(Role::Kernel.as_str(), "function");
    assert_eq!(
        serde_json::to_value(Role::Kernel).expect("serialises"),
        json!("function")
    );
    assert_eq!(
        serde_json::from_value::<Role>(json!("function")).expect("reads"),
        Role::Kernel
    );
    assert!(serde_json::from_value::<Role>(json!("kernel")).is_err());
    assert_eq!(fb::Role::Function.0, 1);
    assert_eq!(fb::Role::Function.variant_name(), Some("Function"));
    let rendered: serde_json::Value =
        serde_json::from_str(&job().to_json()).expect("the rendering is JSON");
    assert_eq!(rendered["tasks"][1]["role"], "function");
}

/// A job's name is written and read back, and a job without one writes none.
#[test]
fn the_name_round_trips() {
    let mut named = job();
    named.name = Some("orders".into());
    let back = Job::from_flatbuffer(&named.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back.name.as_deref(), Some("orders"));
    assert_eq!(back, named);
    let unnamed = Job::from_flatbuffer(&job().to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(unnamed.name, None);
    assert_ne!(
        named.to_flatbuffer().expect("encodes"),
        job().to_flatbuffer().expect("encodes")
    );
}

/// A version 1 document that carries a credential is refused, naming the field: a document holds
/// endpoints and names only.
#[test]
fn a_credential_in_a_document_is_refused() {
    for (store, field) in [
        (json!({"s3": {"access_key_id": "AKIA"}}), "access_key_id"),
        (
            json!({"s3": {"secret_access_key": "s"}}),
            "secret_access_key",
        ),
        (json!({"s3": {"session_token": "t"}}), "session_token"),
        (
            json!({"gcs": {"service_account_json": "{}"}}),
            "service_account_json",
        ),
        (
            json!({"gcs": {"service_account_path": "/sa.json"}}),
            "service_account_path",
        ),
        (json!({"azure": {"access_key": "k"}}), "access_key"),
    ] {
        let text = json!({
            "moruna_spec": 1,
            "source": {"kind": "parquet", "url": "file:///in"},
            "sink": {"kind": "parquet", "url": "file:///out"},
            "object_store": store,
        })
        .to_string();
        let refused = JobSpec::from_json(&text).expect_err("a credential is refused");
        assert!(
            refused.to_string().contains(field),
            "{field} is named: {refused}"
        );
    }
    let fine = json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "file:///in"},
        "sink": {"kind": "parquet", "url": "file:///out"},
        "object_store": {"s3": {"endpoint": "http://minio:9000", "region": "r", "bucket": "b"}},
    })
    .to_string();
    let spec = JobSpec::from_json(&fine).expect("endpoints and names are kept");
    let job = Job::from_v1(&spec);
    let back = Job::from_flatbuffer(&job.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back.object_store, job.object_store);
}
