//! A credential's place in a document holds a reference to one of the job's secrets,
//! `{"secret": "<name>"}`, never the credential (system design 3B).

use moruna_job::{
    Job, JobError, JobSpec, LITERAL_CREDENTIAL, RefusalCode, SecretRef, check_secrets,
    is_secret_name,
};
use serde_json::{Value, json};

fn document(store: Value) -> String {
    json!({
        "moruna_spec": 1,
        "source": {"kind": "parquet", "url": "s3://in/"},
        "sink": {"kind": "parquet", "url": "s3://out/"},
        "object_store": store,
    })
    .to_string()
}

fn every_reference() -> Value {
    json!({
        "s3": {"endpoint": "https://s3.example.com", "bucket": "b",
               "access_key_id": {"secret": "aws.id"},
               "secret_access_key": {"secret": "aws.key"},
               "session_token": {"secret": "aws:token"}},
        "gcs": {"bucket": "g", "service_account_json": {"secret": "gcs"}},
        "azure": {"account": "a", "access_key": {"secret": "azure-key_1"}},
    })
}

#[test]
fn a_reference_is_taken_where_a_credential_belongs_and_kept_through_version_2() {
    let spec = JobSpec::from_json(&document(every_reference())).expect("references are taken");
    let job = Job::from_v1(&spec);
    let refs: Vec<(&str, &str)> = job
        .object_store
        .secret_refs()
        .into_iter()
        .map(|(field, r)| (field, r.secret.as_str()))
        .collect();
    assert_eq!(
        refs,
        [
            ("object_store.s3.access_key_id", "aws.id"),
            ("object_store.s3.secret_access_key", "aws.key"),
            ("object_store.s3.session_token", "aws:token"),
            ("object_store.gcs.service_account_json", "gcs"),
            ("object_store.azure.access_key", "azure-key_1"),
        ]
    );
    // Written back as JSON, a reference is the object it was read from.
    let written = serde_json::to_value(&job.object_store).unwrap();
    assert_eq!(
        written["s3"]["secret_access_key"],
        json!({"secret": "aws.key"})
    );
    // Through version 2's FlatBuffers and back, unchanged.
    let back = Job::from_flatbuffer(&job.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back.object_store, job.object_store);
}

#[test]
fn a_literal_credential_or_a_reference_that_is_not_one_is_refused_by_its_field() {
    let cases = [
        (
            json!({"s3": {"secret_access_key": "wJalrXUtnFEMI"}}),
            "object_store.s3.secret_access_key",
            LITERAL_CREDENTIAL,
        ),
        (
            json!({"gcs": {"service_account_json": {"type": "service_account"}}}),
            "object_store.gcs.service_account_json",
            "unknown field `type`",
        ),
        (
            json!({"azure": {"access_key": {"secret": "-key"}}}),
            "object_store.azure.access_key",
            "`-key` is not a secret's name",
        ),
        (
            json!({"s3": {"session_token": {"secret": "t", "value": "x"}}}),
            "object_store.s3.session_token",
            "unknown field `value`",
        ),
        (
            json!({"s3": {"access_key_id": 7}}),
            "object_store.s3.access_key_id",
            LITERAL_CREDENTIAL,
        ),
    ];
    for (store, field, reason) in cases {
        let refused = JobSpec::from_json(&document(store)).expect_err("refused");
        assert_eq!(refused.field, field);
        assert!(
            refused.reason.contains(reason),
            "{field}: {}",
            refused.reason
        );
        assert!(
            !refused.reason.contains("wJalrXUtnFEMI"),
            "the credential is never repeated"
        );
    }
}

#[test]
fn a_secrets_name_is_a_ports_name() {
    for name in ["k", "aws.key", "A-1:b_2", &"x".repeat(64)] {
        assert!(is_secret_name(name), "{name}");
    }
    for name in ["", "-k", ".k", "a b", "k/1", "k=v", &"x".repeat(65)] {
        assert!(!is_secret_name(name), "{name}");
    }
}

#[test]
fn a_reference_to_a_secret_not_given_is_refused_at_check() {
    let job = Job::from_v1(&JobSpec::from_json(&document(every_reference())).unwrap());
    let given = ["aws.id", "aws.key", "aws:token", "gcs", "azure-key_1"];
    assert_eq!(check_secrets(&job, &given), Ok(()));
    let refusals = check_secrets(&job, &["aws.key", "unused"]).unwrap_err();
    let fields: Vec<&str> = refusals.iter().map(|r| r.field.as_str()).collect();
    assert_eq!(
        fields,
        [
            "object_store.s3.access_key_id",
            "object_store.s3.session_token",
            "object_store.gcs.service_account_json",
            "object_store.azure.access_key",
        ]
    );
    assert_eq!(
        refusals[0].code,
        RefusalCode::Secret {
            name: "aws.id".to_string()
        }
    );
    assert!(refusals[0].message.contains("no secret named `aws.id`"));
    // A document that refers to no secret needs none.
    let plain = Job::from_v1(&JobSpec::from_json(&document(json!({}))).unwrap());
    assert_eq!(check_secrets(&plain, &[]), Ok(()));
}

#[test]
fn version_2_refuses_a_reference_that_is_not_a_secrets_name() {
    let mut job = Job::from_v1(&JobSpec::from_json(&document(json!({}))).unwrap());
    job.object_store.s3 = Some(moruna_job::S3Doc {
        secret_access_key: Some(SecretRef::new("not a name")),
        ..Default::default()
    });
    let bytes = job.to_flatbuffer().expect("encodes");
    match Job::from_flatbuffer(&bytes) {
        Err(JobError::Field { field, reason }) => {
            assert_eq!(field, "object_store.s3.secret_access_key");
            assert!(reason.contains("not a secret's name"), "{reason}");
        }
        other => panic!("refused by its field, not {other:?}"),
    }
}

/// Azure Blob's credential is an account key or a SAS token (item 3.5), each a reference to one
/// of the job's secrets, kept through version 2; both at once are refused by the token's field,
/// in either version, and a literal token as any literal credential is.
#[test]
fn a_blob_sas_token_is_a_reference_in_place_of_the_account_key() {
    let sas = json!({"azure": {"account": "acct", "sas_token": {"secret": "blob-sas"}}});
    let job = Job::from_v1(&JobSpec::from_json(&document(sas)).expect("a SAS token is taken"));
    let refs: Vec<(&str, &str)> = job
        .object_store
        .secret_refs()
        .into_iter()
        .map(|(field, r)| (field, r.secret.as_str()))
        .collect();
    assert_eq!(refs, [("object_store.azure.sas_token", "blob-sas")]);
    let back = Job::from_flatbuffer(&job.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back.object_store, job.object_store);
    assert_eq!(check_secrets(&job, &["blob-sas"]), Ok(()));
    assert_eq!(
        check_secrets(&job, &[]).unwrap_err()[0].field,
        "object_store.azure.sas_token"
    );

    let both = json!({"azure": {"account": "acct", "access_key": {"secret": "blob-key"},
                                "sas_token": {"secret": "blob-sas"}}});
    let refused = JobSpec::from_json(&document(both)).expect_err("both are refused");
    assert_eq!(refused.field, "object_store.azure.sas_token");
    assert!(refused.reason.contains("not both"), "{}", refused.reason);
    let mut job = Job::from_v1(&JobSpec::from_json(&document(json!({}))).unwrap());
    job.object_store.azure = Some(moruna_job::AzureDoc {
        access_key: Some(SecretRef::new("blob-key")),
        sas_token: Some(SecretRef::new("blob-sas")),
        ..Default::default()
    });
    match Job::from_flatbuffer(&job.to_flatbuffer().expect("encodes")) {
        Err(JobError::Field { field, reason }) => {
            assert_eq!(field, "object_store.azure.sas_token");
            assert!(reason.contains("not both"), "{reason}");
        }
        other => panic!("refused by its field, not {other:?}"),
    }

    let literal = json!({"azure": {"sas_token": "sv=2024-11-04&sig=c2VjcmV0"}});
    let refused = JobSpec::from_json(&document(literal)).expect_err("a literal token");
    assert_eq!(refused.field, "object_store.azure.sas_token");
    assert!(refused.reason.contains(LITERAL_CREDENTIAL));
    assert!(!refused.reason.contains("c2VjcmV0"));
}

/// An S3 endpoint is path-style unless the document says it is the bucket's own host: the
/// setting is written only when it is on, and kept through version 2.
#[test]
fn an_s3_endpoint_is_path_style_unless_it_is_the_buckets_own_host() {
    let path = json!({"s3": {"endpoint": "https://objects.example.com", "region": "r"}});
    let job = Job::from_v1(&JobSpec::from_json(&document(path)).unwrap());
    let s3 = job.object_store.s3.as_ref().expect("s3");
    assert!(!s3.virtual_hosted_style_request);
    assert!(
        serde_json::to_value(s3)
            .unwrap()
            .get("virtual_hosted_style_request")
            .is_none(),
        "off is not written"
    );
    let host = json!({"s3": {"endpoint": "https://in.objects.example.com", "region": "r",
                             "virtual_hosted_style_request": true}});
    let job = Job::from_v1(&JobSpec::from_json(&document(host)).unwrap());
    assert!(
        job.object_store
            .s3
            .as_ref()
            .unwrap()
            .virtual_hosted_style_request
    );
    let back = Job::from_flatbuffer(&job.to_flatbuffer().expect("encodes")).expect("reads");
    assert_eq!(back.object_store, job.object_store);
    // With no endpoint there is no bucket's host to name: refused by the setting's field.
    let nowhere = json!({"s3": {"region": "r", "virtual_hosted_style_request": true}});
    let refused = JobSpec::from_json(&document(nowhere)).expect_err("no endpoint");
    assert_eq!(
        refused.field,
        "object_store.s3.virtual_hosted_style_request"
    );
    assert!(refused.reason.contains("endpoint"), "{}", refused.reason);
}
