//! Functions end to end, over TLS (item 3.6, phase 1): registration with an admin token, the
//! list, each refusal (a bad signature, a key node config does not hold, an observe token), the
//! audit record, and a job refused at `validate` for a function that is not registered.
mod common;

use common::{
    Harness,
    signer::{Signer, registration, sha256_hex},
};
use serde_json::{Value, json};

const IMAGE: &[u8] = b"\x7fELF gate-score, a native function";
const ADDRESS: &str = "https://images.example.com/gate-score";
const JOB: &str = r#"{"v1":{"moruna_spec":1,"source":{"kind":"rows","rows":10},"kernels":[{"kind":"function","function":"gate-score"}],"sink":{"kind":"count"}}}"#;

#[test]
fn a_signed_function_is_registered_and_listed() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let admin = harness.token(&["admin"]);
    let body = registration("gate-score", ADDRESS, IMAGE, &harness.signer);
    let registered = harness.exchange(client, &Harness::post("/v1/functions", &admin, &body));
    assert_eq!(registered.status, 201);
    let function = registered.json();
    assert_eq!(
        (&function["name"], &function["address"], &function["sha256"]),
        (
            &json!("gate-score"),
            &json!(ADDRESS),
            &json!(sha256_hex(IMAGE))
        )
    );
    // The list is open to an observe token.
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let list = harness.get(client, "/v1/functions", &observer);
    assert_eq!(list.status, 200);
    assert_eq!(list.json(), json!({ "functions": [function] }));
    // Each registration leaves its audit record.
    let audit = harness.get(client, "/v1/node/audit", &Harness::bearer(&admin));
    let records = audit.json()["records"].clone();
    let operations: Vec<&str> = records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        ["functions_list", "functions_register"],
        "newest first"
    );
    assert_eq!(records[1]["status"], 201);
}

#[test]
fn a_sas_address_is_answered_and_listed_without_its_query() {
    // A private store's image, by its SAS (3E): the query is a credential, never shown.
    let sas = "sv=2022-11-02&sr=c&sp=r&se=2026-11-18T18%3A00Z&spr=https&sig=FAKEfake%2Bfake%3D";
    let address = format!("https://acct.blob.core.windows.net/functions/gate-score?{sas}");
    let shown = "https://acct.blob.core.windows.net/functions/gate-score?…";
    let mut harness = Harness::new();
    let client = harness.connect();
    let admin = harness.token(&["admin"]);
    let body = registration("gate-score", &address, IMAGE, &harness.signer);
    let registered = harness.exchange(client, &Harness::post("/v1/functions", &admin, &body));
    assert_eq!(registered.status, 201);
    assert_eq!(registered.json()["address"], shown);
    let observer = Harness::bearer(&harness.token(&["observe"]));
    let list = harness.get(client, "/v1/functions", &observer);
    assert_eq!(list.status, 200);
    assert_eq!(list.json()["functions"][0]["address"], shown);
    assert_eq!(list.json()["functions"][0]["sha256"], sha256_hex(IMAGE));
    for answer in [&registered, &list] {
        let text = answer.json().to_string();
        assert!(
            !text.contains("sig=") && !text.contains("FAKEfake"),
            "{text}"
        );
    }
}

#[test]
fn a_bad_signature_an_unknown_key_and_a_lesser_scope_are_refused() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let admin = harness.token(&["admin"]);
    // Signed by a key node config does not hold.
    let stranger = registration("gate-score", ADDRESS, IMAGE, &Signer::new());
    let refused = harness.exchange(client, &Harness::post("/v1/functions", &admin, &stranger));
    assert_eq!(refused.status, 400);
    assert!(
        refused.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not verify")
    );
    // Signed by the node's key over another image.
    let mut altered: Value =
        serde_json::from_str(&registration("gate-score", ADDRESS, IMAGE, &harness.signer)).unwrap();
    altered["sha256"] = sha256_hex(b"an altered image").into();
    let refused = harness.exchange(
        client,
        &Harness::post("/v1/functions", &admin, &altered.to_string()),
    );
    assert_eq!(refused.status, 400);
    // Registration is an admin's: a submit token is forbidden, before the body is read.
    let submit = harness.token(&["submit"]);
    let good = registration("gate-score", ADDRESS, IMAGE, &harness.signer);
    let forbidden = harness.exchange(client, &Harness::post("/v1/functions", &submit, &good));
    assert_eq!(forbidden.status, 403);
    // A refusal on the head leaves the body unread, so the connection closes: a new one lists.
    let client = harness.connect();
    let list = harness.get(client, "/v1/functions", &Harness::bearer(&admin));
    assert_eq!(
        list.json(),
        json!({ "functions": [] }),
        "nothing refused is kept"
    );
}

#[test]
fn validate_refuses_a_function_that_is_not_registered() {
    let mut harness = Harness::new();
    let client = harness.connect();
    let submitter = harness.token(&["submit"]);
    // The engine's check finds the document sound, and answers that the job uses gate-score.
    harness
        .engine
        .0
        .borrow_mut()
        .functions
        .push_back(vec!["gate-score".to_string()]);
    let refused = harness.exchange(client, &Harness::post("/v1/jobs/validate", &submitter, JOB));
    assert_eq!(refused.status, 200);
    let verdict = refused.json();
    assert_eq!(verdict["ok"], false);
    assert_eq!(verdict["refusals"][0]["field"], "functions");
    assert!(
        verdict["refusals"][0]["message"]
            .as_str()
            .unwrap()
            .contains("`gate-score` is not registered")
    );
    // Registered, the same job validates.
    let admin = harness.token(&["admin"]);
    let body = registration("gate-score", ADDRESS, IMAGE, &harness.signer);
    let registered = harness.exchange(client, &Harness::post("/v1/functions", &admin, &body));
    assert_eq!(registered.status, 201);
    harness
        .engine
        .0
        .borrow_mut()
        .functions
        .push_back(vec!["gate-score".to_string()]);
    let ok = harness.exchange(client, &Harness::post("/v1/jobs/validate", &submitter, JOB));
    assert_eq!(ok.json(), json!({ "ok": true }));
}
