//! Registration (system design 3E): what verifies and is kept, every refusal, the list, and the
//! refusal of a function a job uses that is not registered.

use super::*;
use crate::signer::{Signer, base64 as b64, registration, sha256_hex};
use alloc::{string::ToString, vec};
use serde_json::Value;

const NOW: u64 = 1_794_931_200;
const IMAGE: &[u8] = b"\x7fELF a native function";

fn body(answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body.bytes()).unwrap()
}

fn message(answer: &Answer) -> String {
    body(answer)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_signed_image_is_registered_listed_and_replaced() {
    let signer = Signer::new();
    let key = FunctionKey::new(signer.public());
    assert_eq!(key.id().len(), 16);
    let mut registry = Registry::new(vec![FunctionKey::new([9; 32]), key.clone()]);
    let address = "https://images.example.com/gate-score";
    let answer = registry.register(
        registration("gate-score", address, IMAGE, &signer).as_bytes(),
        NOW,
    );
    assert_eq!(answer.status, 201, "{answer:?}");
    let function = body(&answer);
    assert_eq!(function["name"], "gate-score");
    assert_eq!(function["address"], address);
    assert_eq!(function["sha256"], sha256_hex(IMAGE));
    assert_eq!(
        function["key"],
        key.id(),
        "the key that verified, of the two"
    );
    assert_eq!(function["registered"], NOW);
    assert_eq!(registry.get("gate-score").unwrap().registered, NOW);
    // Registering the name again replaces its entry; another name is another entry.
    let other = b"\x7fELF the second build";
    let again = registration(
        "gate-score",
        "https://x.blob.core.windows.net/code/gate-score?sv=1&sig=a",
        other,
        &signer,
    );
    assert_eq!(registry.register(again.as_bytes(), NOW + 5).status, 201);
    let enrich = registration(
        "gate-enrich",
        "https://images.example.com/gate-enrich",
        IMAGE,
        &signer,
    );
    assert_eq!(registry.register(enrich.as_bytes(), NOW + 6).status, 201);
    let list = registry.list();
    let names: Vec<&str> = list.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["gate-enrich", "gate-score"], "by name");
    assert_eq!(list.functions[1].sha256, sha256_hex(other));
    assert_eq!(list.functions[1].registered, NOW + 5);
}

/// The registration of [`registration`] with the function's measured memory beside it.
fn measured_registration(name: &str, image: &[u8], signer: &Signer, measured: Value) -> String {
    let address = "https://images.example.com/measured";
    let mut body: Value =
        serde_json::from_str(&registration(name, address, image, signer)).unwrap();
    body["measured"] = measured;
    body.to_string()
}

fn measured() -> Value {
    serde_json::json!({"fixed_bytes": 1310720.0, "per_input_byte": 0.054,
                       "output_per_input_byte": 0.21, "spread": 0.02, "units": 21,
                       "input_bytes_min": 149000, "input_bytes_max": 33525000})
}

/// System design 3A, 3E (6.3g): a registration may carry the function's measured memory. The
/// node keeps it with the function's image, shows it and whose it is, and hands it to a job
/// that uses the function; what is not a measurement is refused.
#[test]
fn a_registration_s_measured_memory_is_kept_shown_and_handed_to_a_job() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    let body_of = |measured| measured_registration("gate-score", IMAGE, &signer, measured);
    let answer = registry.register(body_of(measured()).as_bytes(), NOW);
    assert_eq!(answer.status, 201, "{answer:?}");
    let function = body(&answer);
    assert_eq!(function["measured"], measured());
    assert_eq!(function["measured_by"], "registration");
    let listed = serde_json::to_value(registry.list()).unwrap();
    assert_eq!(listed["functions"][0]["measured"], measured());
    assert_eq!(listed["functions"][0]["measured_by"], "registration");
    let handed = registry.registered(&["gate-score".to_string()]);
    assert_eq!(
        handed[0]
            .measured
            .as_ref()
            .map(|m| (m.per_input_byte, m.units)),
        Some((0.054, 21))
    );

    // The same image registered again with nothing measured keeps what was measured of it.
    let plain = registration(
        "gate-score",
        "https://images.example.com/again",
        IMAGE,
        &signer,
    );
    assert_eq!(registry.register(plain.as_bytes(), NOW + 1).status, 201);
    let kept = registry.get("gate-score").unwrap();
    assert_eq!(kept.registered, NOW + 1);
    assert_eq!(kept.measured_by, Some(MeasuredBy::Registration));
    assert_eq!(kept.measured.as_ref().map(|m| m.units), Some(21));
    // A newer measurement of it replaces the one kept.
    let mut newer = measured();
    newer["units"] = serde_json::json!(40);
    assert_eq!(
        registry.register(body_of(newer).as_bytes(), NOW + 2).status,
        201
    );
    assert_eq!(
        registry
            .get("gate-score")
            .unwrap()
            .measured
            .as_ref()
            .map(|m| m.units),
        Some(40)
    );
    // Another image under the name is another function: nothing measured of it yet.
    let other = registration(
        "gate-score",
        "https://images.example.com/next",
        b"\x7fELF the next build",
        &signer,
    );
    assert_eq!(registry.register(other.as_bytes(), NOW + 3).status, 201);
    let next = registry.get("gate-score").unwrap();
    assert_eq!((next.measured.clone(), next.measured_by), (None, None));
    assert!(
        body(&registry.register(other.as_bytes(), NOW + 4))
            .get("measured")
            .is_none()
    );

    // What is not a measurement is refused, and so is a term it does not have.
    for (change, why) in [
        (
            ("per_input_byte", serde_json::json!(-1.0)),
            "finite amounts of zero or more",
        ),
        (("units", serde_json::json!(0)), "at least 1"),
        (
            ("input_bytes_min", serde_json::json!(0)),
            "the range of input",
        ),
    ] {
        let mut wrong = measured();
        wrong[change.0] = change.1;
        let refused = registry.register(body_of(wrong).as_bytes(), NOW + 5);
        assert_eq!(refused.status, 400, "{refused:?}");
        let said = message(&refused);
        assert!(
            said.contains("measured is not a measurement of the function's memory")
                && said.contains(why),
            "{said}"
        );
    }
    let mut unknown = measured();
    unknown["runs"] = serde_json::json!(3);
    assert_eq!(
        registry
            .register(body_of(unknown).as_bytes(), NOW + 6)
            .status,
        400
    );
    assert_eq!(
        registry.get("gate-score").unwrap().registered,
        NOW + 4,
        "none replaced it"
    );
}

/// System design 3A (6.3g phase 3): the node keeps a function's readings with its image and
/// measures it from them; a registration's measurement gives them its slope; another image
/// starts again.
#[test]
fn the_node_s_readings_of_an_image_refine_its_measurement() {
    const MIB: u64 = 1 << 20;
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    let sha = sha256_hex(IMAGE);
    let plain = registration("gate-score", "https://images.example.com/s", IMAGE, &signer);
    assert_eq!(registry.register(plain.as_bytes(), NOW).status, 201);
    assert_eq!(registry.get("gate-score").unwrap().measured, None);

    // A run's readings: the node's measurement, from them alone.
    registry.observe(
        "gate-score",
        &sha,
        &[(16 * MIB, 4 * MIB), (16 * MIB, 5 * MIB)],
    );
    let kept = registry.get("gate-score").unwrap();
    assert_eq!(kept.measured_by, Some(MeasuredBy::Node));
    assert_eq!(
        kept.measured.as_ref().map(|m| m.fixed_bytes),
        Some((5 * MIB) as f64)
    );
    let listed = serde_json::to_value(registry.list()).unwrap();
    assert_eq!(listed["functions"][0]["measured_by"], "node");

    // Readings of another image, of a name not registered, or of nothing change nothing.
    registry.observe("gate-score", &"cd".repeat(32), &[(16 * MIB, 90 * MIB)]);
    registry.observe("nobody", &sha, &[(16 * MIB, 90 * MIB)]);
    registry.observe("gate-score", &sha, &[]);
    registry.observe("gate-score", &sha, &[(0, 90 * MIB)]);
    assert_eq!(
        registry
            .get("gate-score")
            .unwrap()
            .measured
            .as_ref()
            .map(|m| (m.fixed_bytes, m.units)),
        Some(((5 * MIB) as f64, 2))
    );

    // The same image registered with its client's measurement: the client's slope, the node's
    // fixed term, the node's measurement still.
    let body = measured_registration("gate-score", IMAGE, &signer, measured());
    assert_eq!(registry.register(body.as_bytes(), NOW + 1).status, 201);
    let refined = registry.get("gate-score").unwrap();
    assert_eq!(refined.measured_by, Some(MeasuredBy::Node));
    let m = refined.measured.clone().unwrap();
    assert_eq!(m.per_input_byte, 0.054);
    assert!((m.fixed_bytes - ((5 * MIB) as f64 - 0.054 * (16 * MIB) as f64)).abs() < 1e-6);

    // The newest READINGS_KEPT are kept.
    for _ in 0..READINGS_KEPT {
        registry.observe("gate-score", &sha, &[(16 * MIB, 3 * MIB)]);
    }
    let newest = registry
        .get("gate-score")
        .unwrap()
        .measured
        .clone()
        .unwrap();
    assert_eq!(newest.units, READINGS_KEPT as u64);
    assert!((newest.fixed_bytes - ((3 * MIB) as f64 - 0.054 * (16 * MIB) as f64)).abs() < 1e-6);

    // Another image under the name starts again: its client's measurement, the registration's.
    let next = measured_registration("gate-score", b"\x7fELF the next build", &signer, measured());
    assert_eq!(registry.register(next.as_bytes(), NOW + 2).status, 201);
    let fresh = registry.get("gate-score").unwrap();
    assert_eq!(fresh.measured_by, Some(MeasuredBy::Registration));
    assert_eq!(fresh.measured.as_ref().map(|m| m.units), Some(21));
    // And a plain registration of a new image carries nothing measured.
    let other = registration(
        "gate-score",
        "https://images.example.com/o",
        b"\x7fELF o",
        &signer,
    );
    assert_eq!(registry.register(other.as_bytes(), NOW + 3).status, 201);
    assert_eq!(registry.get("gate-score").unwrap().measured, None);
}

#[test]
fn a_bad_signature_or_an_unknown_key_is_refused() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    // Signed by a key node config does not hold.
    let stranger = Signer::new();
    let unknown = registry.register(
        registration("gate-score", "https://x/gate-score", IMAGE, &stranger).as_bytes(),
        NOW,
    );
    assert_eq!(unknown.status, 400);
    assert!(
        message(&unknown).contains("does not verify under any of node config's functions.keys")
    );
    // Signed by the node's key, but over another image's SHA-256.
    let mut altered: Value = serde_json::from_str(&registration(
        "gate-score",
        "https://x/gate-score",
        IMAGE,
        &signer,
    ))
    .unwrap();
    altered["sha256"] = sha256_hex(b"another image").into();
    let bad = registry.register(altered.to_string().as_bytes(), NOW);
    assert_eq!(bad.status, 400);
    assert!(message(&bad).contains("does not verify"));
    // A signature over the SHA-256's hex text rather than its 32 bytes does not verify either.
    let mut over_text = altered.clone();
    over_text["sha256"] = sha256_hex(IMAGE).into();
    over_text["signature"] = b64(&signer.signature(sha256_hex(IMAGE).as_bytes())).into();
    assert_eq!(
        registry
            .register(over_text.to_string().as_bytes(), NOW)
            .status,
        400
    );
    assert!(
        registry.list().functions.is_empty(),
        "nothing refused is kept"
    );
    // A node whose config has no functions.keys takes no image at all.
    let mut none = Registry::new(Vec::new());
    let refused = none.register(
        registration("gate-score", "https://x/gate-score", IMAGE, &signer).as_bytes(),
        NOW,
    );
    assert!(message(&refused).contains("node config has no functions.keys"));
}

#[test]
fn each_malformed_field_is_refused_with_its_reason() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    let good: Value = serde_json::from_str(&registration(
        "gate-score",
        "https://x/gate-score",
        IMAGE,
        &signer,
    ))
    .unwrap();
    let with = |field: &str, value: &str| {
        let mut changed = good.clone();
        changed[field] = value.into();
        changed.to_string()
    };
    let cases = [
        ("{", "is not a function's registration".to_string()),
        (r#"{"name":"a"}"#, "is not a function's registration".into()),
        (
            &{
                let mut extra = good.clone();
                extra["key"] = "k".into();
                extra.to_string()
            },
            "is not a function's registration".into(),
        ),
        (&with("name", ""), "the name is not a function's".into()),
        (&with("name", "Gate"), "the name is not a function's".into()),
        (
            &with("name", "9gate"),
            "the name is not a function's".into(),
        ),
        (
            &with("name", "gate score"),
            "the name is not a function's".into(),
        ),
        (
            &with("name", &"a".repeat(65)),
            "the name is not a function's".into(),
        ),
        (&with("address", "http://x/gate"), "the address".into()),
        (&with("address", "file:///gate"), "the address".into()),
        (&with("address", "https://"), "the address".into()),
        // Only https://: an object store's scheme would need its credentials where it is fetched.
        (&with("address", "s3://bucket/gate"), "the address".into()),
        (
            &with("address", "az://container/gate"),
            "the address".into(),
        ),
        (&with("address", "gs://bucket/gate"), "the address".into()),
        (&with("address", "HTTPS://x/gate"), "the address".into()),
        (&with("address", "https://?a=b"), "the address".into()),
        (&with("address", "gate-score"), "the address".into()),
        (&with("address", "https://x/a b"), "the address".into()),
        (
            &with(
                "address",
                &format!("https://x/{}", "a".repeat(ADDRESS_LIMIT)),
            ),
            "the address".into(),
        ),
        (
            &with("sha256", &sha256_hex(IMAGE).to_uppercase()),
            "sha256".into(),
        ),
        (&with("sha256", &sha256_hex(IMAGE)[..62]), "sha256".into()),
        (
            &with("sha256", &format!("{}zz", &sha256_hex(IMAGE)[..62])),
            "sha256".into(),
        ),
        (
            &with("signature", "not base64!"),
            "the signature is an Ed25519 signature".into(),
        ),
        (
            &with("signature", &b64(&[1; 32])),
            "the signature is an Ed25519 signature".into(),
        ),
    ];
    for (text, why) in cases {
        let answer = registry.register(text.as_bytes(), NOW);
        assert_eq!(answer.status, 400, "{text}");
        assert!(
            message(&answer).contains(&why),
            "{text}: {}",
            message(&answer)
        );
    }
    // The names a job's kind may have.
    for name in ["gate-score", "a", "std.v2", "x_1-y", &"a".repeat(64)] {
        assert!(is_function_name(name), "{name}");
    }
    for address in [
        "https://x",
        "https://x/a?b=c",
        "https://acct.blob.core.windows.net/c/k?sv=2025-01-05&sig=abc%2Fdef",
        "https://b.s3.amazonaws.com/k?X-Amz-Signature=0f",
    ] {
        assert!(is_address(address), "{address}");
    }
}

#[test]
fn the_registry_is_bounded() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    for i in 0..FUNCTIONS_LIMIT {
        let text = registration(&format!("f{i}"), "https://x/f", IMAGE, &signer);
        assert_eq!(registry.register(text.as_bytes(), NOW).status, 201);
    }
    let over = registry.register(
        registration("one-more", "https://x/f", IMAGE, &signer).as_bytes(),
        NOW,
    );
    assert_eq!(over.status, 409);
    assert!(message(&over).contains(&format!("{FUNCTIONS_LIMIT} functions")));
    // A name already there may still be registered again: it replaces, and takes no room.
    let again = registration("f0", "https://x/f0", IMAGE, &signer);
    assert_eq!(registry.register(again.as_bytes(), NOW).status, 201);
    assert_eq!(registry.list().functions.len(), FUNCTIONS_LIMIT);
}

#[test]
fn a_sas_address_is_shown_without_its_query_and_kept_whole_for_the_fetch() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    let address = "https://acct.blob.core.windows.net/functions/gate-score?sp=r&sig=FAKEfake%3D";
    let shown = "https://acct.blob.core.windows.net/functions/gate-score?…";
    let answer = registry.register(
        registration("gate-score", address, IMAGE, &signer).as_bytes(),
        NOW,
    );
    assert_eq!(answer.status, 201);
    assert_eq!(body(&answer)["address"], shown, "the answer shows no query");
    assert_eq!(
        registry.list().functions[0].address,
        shown,
        "nor does the list"
    );
    // Only what node-admission is given to fetch keeps it.
    let fetched = registry.registered(&["gate-score".to_string()]);
    assert_eq!(fetched[0].address, address);
    // An address with no query is shown as it is.
    let plain = "https://images.example.com/gate-enrich";
    let text = registration("gate-enrich", plain, IMAGE, &signer);
    assert_eq!(
        body(&registry.register(text.as_bytes(), NOW))["address"],
        plain
    );
    assert_eq!(registry.list().functions[0].address, plain);
}

#[test]
fn a_function_a_job_uses_that_is_not_registered_is_refused() {
    let signer = Signer::new();
    let mut registry = Registry::new(vec![FunctionKey::new(signer.public())]);
    let text = registration("gate-score", "https://x/gate-score", IMAGE, &signer);
    assert_eq!(registry.register(text.as_bytes(), NOW).status, 201);
    assert!(registry.refusals(&[]).is_empty());
    assert!(registry.refusals(&["gate-score".to_string()]).is_empty());
    // The check names a job's registered functions only (its `kernels` entries of kind
    // `function`), never the engine's own kinds.
    let refusals = registry.refusals(&[
        "gate-score".to_string(),
        "gate-audit".to_string(),
        "gate-enrich".to_string(),
    ]);
    assert_eq!(refusals.len(), 2);
    assert_eq!(refusals[0].field, "functions");
    assert!(
        refusals[0]
            .message
            .contains("`gate-audit` is not registered")
    );
    assert!(refusals[1].message.contains("`gate-enrich`"));
}
