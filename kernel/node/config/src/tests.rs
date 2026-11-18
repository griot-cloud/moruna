//! Host tests: the base64 and PEM readers, every refusal of node config, and the hand-over's
//! round trip.

extern crate std;

use std::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::{
    Api, CACHE_BYTES_DEFAULT, Cloud, Functions, Handover, HandoverError, MESSAGE_MAX, MIB,
    NodeConfig, PublicKey, READY, Refusal, Tls, base64, cloud_name, instance_valid,
    pem::{self, PemError},
};

/// The memory of the node these tests validate for: 8 GiB, a `Standard_D4ls_v5`.
const MEMORY: u64 = 8 << 30;

/// A key as node config writes it: 32 bytes of `seed`.
fn key(seed: u8) -> String {
    base64::encode(&[seed; 32])
}

/// A PEM block of `bytes`, its body in lines of 64 characters.
fn block(label: &str, bytes: &[u8]) -> String {
    let body = base64::encode(bytes);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|line| core::str::from_utf8(line).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// A certificate chain and a key in PEM armour. The bytes are not a certificate or a key: this
/// crate checks the armour, and `api` checks what is inside.
fn tls() -> (String, String) {
    let chain = block("CERTIFICATE", &[0x30; 300]) + &block("CERTIFICATE", &[0x31; 200]);
    (chain, block("PRIVATE KEY", &[0x32; 138]))
}

fn validate(text: &str) -> Result<NodeConfig, Refusal> {
    NodeConfig::validate(text.as_bytes(), MEMORY)
}

fn refusal(text: &str) -> Refusal {
    validate(text).expect_err("the config is refused")
}

/// A config with one token key and `rest` after it inside the top object.
fn with(rest: &str) -> String {
    format!(
        r#"{{"moruna":1,"api":{{"token_keys":["{}"]}}{rest}}}"#,
        key(1)
    )
}

#[test]
fn base64_reads_and_writes_the_standard_encoding() {
    // RFC 4648, section 10.
    for (bytes, text) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64::encode(bytes.as_bytes()), text);
        assert_eq!(base64::decode(text.as_bytes()).unwrap(), bytes.as_bytes());
    }
    let every: Vec<u8> = (0..=255).collect();
    assert_eq!(
        base64::decode(base64::encode(&every).as_bytes()).unwrap(),
        every
    );
    assert_eq!(base64::decode(b"+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
}

#[test]
fn base64_refuses_everything_that_is_not_canonical() {
    for text in [
        "Zg",       // no padding
        "Zg=",      // short padding
        "Z===",     // one character encodes nothing
        "====",     // only padding
        "Zg==Zg==", // padding before the end
        "Zm=v",     // padding inside a group
        "Zh==",     // unused bits set after one byte
        "Zm9=",     // unused bits set after two bytes
        "Zm9v\n",   // a line break
        " Zm9v",    // a space
        "Zm9-",     // the URL alphabet
        "Zm9_",
        "Zm\u{e9}v",
    ] {
        assert_eq!(base64::decode(text.as_bytes()), None, "{text:?}");
    }
}

#[test]
fn pem_lists_its_blocks() {
    let (chain, key) = tls();
    assert_eq!(pem::labels(&chain), Ok(vec!["CERTIFICATE", "CERTIFICATE"]));
    assert_eq!(pem::labels(&key), Ok(vec!["PRIVATE KEY"]));
    assert_eq!(pem::labels(""), Ok(vec![]));
    // CRLF line ends, blank lines between blocks, and no line end after the last.
    let spaced = format!(
        "\r\n{}\r\n\r\n{}",
        chain.replace('\n', "\r\n"),
        key.trim_end()
    );
    assert_eq!(
        pem::labels(&spaced),
        Ok(vec!["CERTIFICATE", "CERTIFICATE", "PRIVATE KEY"])
    );
}

#[test]
fn pem_refuses_what_is_not_blocks() {
    let (_, key) = tls();
    let outside = [
        format!("subject=CN=node\n{key}"),
        format!("{key}trailing"),
        "-----BEGIN private key-----\nAAAA\n-----END private key-----\n".to_string(),
        "-----BEGIN -----\nAAAA\n-----END -----\n".to_string(),
        "-----BEGIN PRIVATE KEY\nAAAA\n-----END PRIVATE KEY-----\n".to_string(),
        "-----END PRIVATE KEY-----\n".to_string(),
    ];
    for text in outside {
        assert_eq!(pem::labels(&text), Err(PemError::Outside), "{text:?}");
    }
    let unfinished = [
        "-----BEGIN PRIVATE KEY-----\nAAAA\n",
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END CERTIFICATE-----\n",
    ];
    for text in unfinished {
        assert_eq!(pem::labels(text), Err(PemError::Unfinished), "{text:?}");
    }
    let body = [
        "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n",
        "-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----\n",
        "-----BEGIN PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\nAAAA\n-----END PRIVATE KEY-----\n",
        "-----BEGIN PRIVATE KEY-----\n-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    ];
    for text in body {
        assert_eq!(pem::labels(text), Err(PemError::Body), "{text:?}");
    }
}

#[test]
fn the_design_s_example_is_taken() {
    let (certificate, private) = tls();
    let text = serde_json::json!({
        "moruna": 1,
        "api": {
            "token_keys": [key(1), key(2)],
            "tls": { "certificate": certificate, "key": private }
        },
        "functions": { "keys": [key(3)], "cache_bytes": 1_073_741_824u64 }
    })
    .to_string();
    assert_eq!(
        validate(&text),
        Ok(NodeConfig {
            api: Api {
                token_keys: vec![PublicKey([1; 32]), PublicKey([2; 32])],
                tls: Some(Tls {
                    certificate,
                    key: private
                }),
            },
            functions: Functions {
                keys: vec![PublicKey([3; 32])],
                cache_bytes: 1 << 30,
                cache_bytes_given: true,
            },
        })
    );
}

#[test]
fn what_is_optional_takes_its_default() {
    let least = validate(&with("")).unwrap();
    assert_eq!(least.api.token_keys, [PublicKey([1; 32])]);
    assert_eq!(least.api.tls, None);
    assert_eq!(least.functions, Functions::default());
    assert_eq!(least.functions.cache_bytes, CACHE_BYTES_DEFAULT);
    assert!(least.functions.keys.is_empty());
    // `functions` with nothing in it, with no keys, and with only one of its fields.
    assert_eq!(validate(&with(r#","functions":{}"#)), Ok(least.clone()));
    assert_eq!(
        validate(&with(r#","functions":{"keys":[]}"#)),
        Ok(least.clone())
    );
    let sized = validate(&with(r#","functions":{"cache_bytes":67108864}"#)).unwrap();
    assert_eq!(sized.functions.cache_bytes, 64 << 20);
    assert!(sized.functions.cache_bytes_given);
    assert!(sized.functions.keys.is_empty());
    assert!(!least.functions.cache_bytes_given);
    // White space around the document is JSON's.
    assert_eq!(validate(&format!("\n {} \n", with(""))), Ok(least));
}

#[test]
fn one_to_eight_token_keys_and_none_to_eight_function_keys() {
    let list = |count: u8| {
        let keys: Vec<String> = (0..count)
            .map(|seed| format!(r#""{}""#, key(seed)))
            .collect();
        format!("[{}]", keys.join(","))
    };
    let tokens = |count| format!(r#"{{"moruna":1,"api":{{"token_keys":{}}}}}"#, list(count));
    let functions = |count| with(&format!(r#","functions":{{"keys":{}}}"#, list(count)));
    assert_eq!(validate(&tokens(1)).unwrap().api.token_keys.len(), 1);
    assert_eq!(validate(&tokens(8)).unwrap().api.token_keys.len(), 8);
    assert_eq!(validate(&functions(8)).unwrap().functions.keys.len(), 8);
    for (count, text) in [(0, tokens(0)), (9, tokens(9))] {
        assert_eq!(
            refusal(&text),
            Refusal::KeyCount {
                path: "api.token_keys",
                count,
                least: 1
            }
        );
    }
    assert_eq!(
        refusal(&functions(9)),
        Refusal::KeyCount {
            path: "functions.keys",
            count: 9,
            least: 0
        }
    );
    assert_eq!(
        refusal(&tokens(9)).to_string(),
        "`api.token_keys` holds 9 keys: 1 to 8 are allowed"
    );
    assert_eq!(
        refusal(&tokens(0)).to_string(),
        "`api.token_keys` holds 0 keys: 1 to 8 are allowed"
    );
}

#[test]
fn a_key_is_the_standard_base64_of_32_bytes() {
    let bad = [
        base64::encode(&[1; 31]),
        base64::encode(&[1; 33]),
        key(1).trim_end_matches('=').to_string(),
        key(0xfb).replace('+', "-"),
        format!(" {}", key(1)),
        "not a key".to_string(),
        String::new(),
    ];
    for text in bad {
        let config = format!(
            r#"{{"moruna":1,"api":{{"token_keys":["{}","{text}"]}}}}"#,
            key(1)
        );
        assert_eq!(
            refusal(&config),
            Refusal::Key {
                path: "api.token_keys",
                index: 1
            },
            "{text:?}"
        );
    }
    let functions = with(r#","functions":{"keys":["AAAA"]}"#);
    assert_eq!(
        refusal(&functions).to_string(),
        "`functions.keys[0]` is not the standard base64 of a 32-byte Ed25519 public key"
    );
}

#[test]
fn a_missing_config_and_one_that_is_not_json_are_refused() {
    for text in ["", " ", "\n\t \r\n"] {
        assert_eq!(refusal(text), Refusal::Missing);
    }
    assert_eq!(
        Refusal::Missing.to_string(),
        "the instance has no user-data"
    );
    assert_eq!(
        Refusal::NotBase64.to_string(),
        "the user-data is not base64"
    );
    for text in [
        "{",
        "#cloud-config\nusers: []\n",
        &format!("{} trailing", with("")),
        &with(",").replace(",}", ",\"api\"}"),
    ] {
        assert!(matches!(refusal(text), Refusal::NotJson(_)), "{text:?}");
    }
    assert!(matches!(
        NodeConfig::validate(b"{\"moruna\":1,\"api\":\"\xff\"}", MEMORY),
        Err(Refusal::NotJson(_))
    ));
    assert_eq!(
        refusal("{").to_string(),
        "not JSON: EOF while parsing an object at line 1 column 1"
    );
}

#[test]
fn a_field_written_twice_is_refused() {
    let twice = format!(
        r#"{{"moruna":1,"api":{{"token_keys":["{0}"]}},"api":{{"token_keys":["{0}"]}}}}"#,
        key(1)
    );
    let refused = refusal(&twice);
    assert!(matches!(refused, Refusal::Twice(_)));
    assert!(
        refused
            .to_string()
            .starts_with("the field `api` is written twice at line 1 column "),
        "{refused}"
    );
    let nested = with(r#","functions":{"keys":[],"keys":[]}"#);
    assert!(matches!(refusal(&nested), Refusal::Twice(_)));
}

#[test]
fn only_version_1_is_read() {
    let versioned = |version: &str| {
        format!(
            r#"{{"moruna":{version},"api":{{"token_keys":["{}"]}},"later":true}}"#,
            key(1)
        )
    };
    // The version is judged before the fields a later version might add.
    for (version, found) in [
        ("2", "2"),
        ("0", "0"),
        ("\"1\"", "a string"),
        ("1.0", "not a whole number"),
        ("-1", "not a whole number"),
        ("null", "not a number"),
        ("true", "not a number"),
        ("[1]", "not a number"),
        ("{}", "not a number"),
    ] {
        assert_eq!(
            refusal(&versioned(version)),
            Refusal::Version(found.to_string()),
            "{version}"
        );
    }
    assert_eq!(
        refusal(&versioned("2")).to_string(),
        "`moruna` is 2: this node reads version 1"
    );
    let unversioned = format!(r#"{{"api":{{"token_keys":["{}"]}}}}"#, key(1));
    assert_eq!(refusal(&unversioned), Refusal::NoVersion);
    assert_eq!(
        refusal(&versioned("1")),
        Refusal::Unknown {
            within: "node config",
            field: "later".to_string()
        }
    );
}

#[test]
fn unknown_fields_are_refused_wherever_they_are() {
    let (certificate, private) = tls();
    let tls = serde_json::json!({ "certificate": certificate, "key": private, "password": "x" });
    let cases = [
        (with(r#","extra":1"#), "node config", "extra"),
        (
            format!(
                r#"{{"moruna":1,"api":{{"token_keys":["{}"],"admin_keys":[]}}}}"#,
                key(1)
            ),
            "api",
            "admin_keys",
        ),
        (
            format!(
                r#"{{"moruna":1,"api":{{"token_keys":["{}"],"tls":{tls}}}}}"#,
                key(1)
            ),
            "api.tls",
            "password",
        ),
        (with(r#","functions":{"cache":1}"#), "functions", "cache"),
    ];
    for (text, within, field) in cases {
        assert_eq!(
            refusal(&text),
            Refusal::Unknown {
                within,
                field: field.to_string()
            }
        );
    }
    assert_eq!(
        refusal(&with(r#","extra":1"#)).to_string(),
        "unknown field `extra` in node config"
    );
    // A name from outside the node is shown only as far as it is short and printable.
    let odd = with(&format!(r#","line\nbreaké{}":1"#, "x".repeat(40)));
    assert_eq!(
        refusal(&odd).to_string(),
        format!(
            "unknown field `line?break?{}...` in node config",
            "x".repeat(21)
        )
    );
}

#[test]
fn a_field_of_the_wrong_kind_or_not_there_is_refused() {
    let api = |inner: &str| format!(r#"{{"moruna":1,"api":{inner}}}"#);
    let tls = |inner: &str| {
        format!(
            r#"{{"moruna":1,"api":{{"token_keys":["{}"],"tls":{inner}}}}}"#,
            key(1)
        )
    };
    let (certificate, private) = tls_json();
    let cases = [
        ("[1]".to_string(), Refusal::NotObject("node config")),
        ("\"moruna\"".to_string(), Refusal::NotObject("node config")),
        ("1".to_string(), Refusal::NotObject("node config")),
        (r#"{"moruna":1}"#.to_string(), Refusal::Absent("api")),
        (api("[]"), Refusal::NotObject("api")),
        (api("null"), Refusal::NotObject("api")),
        (api("{}"), Refusal::Absent("api.token_keys")),
        (
            api(r#"{"token_keys":"AAAA"}"#),
            Refusal::NotStrings("api.token_keys"),
        ),
        (
            api(r#"{"token_keys":[1]}"#),
            Refusal::NotStrings("api.token_keys"),
        ),
        (tls("null"), Refusal::NotObject("api.tls")),
        (tls("\"pem\""), Refusal::NotObject("api.tls")),
        (
            tls(&format!(r#"{{"key":{private}}}"#)),
            Refusal::Absent("api.tls.certificate"),
        ),
        (
            tls(&format!(r#"{{"certificate":{certificate}}}"#)),
            Refusal::Absent("api.tls.key"),
        ),
        (
            tls(&format!(r#"{{"certificate":1,"key":{private}}}"#)),
            Refusal::NotString("api.tls.certificate"),
        ),
        (
            tls(&format!(r#"{{"certificate":{certificate},"key":[]}}"#)),
            Refusal::NotString("api.tls.key"),
        ),
        (with(r#","functions":[]"#), Refusal::NotObject("functions")),
        (
            with(r#","functions":null"#),
            Refusal::NotObject("functions"),
        ),
        (
            with(r#","functions":{"keys":{}}"#),
            Refusal::NotStrings("functions.keys"),
        ),
        (
            with(r#","functions":{"cache_bytes":"1073741824"}"#),
            Refusal::NotNumber("functions.cache_bytes"),
        ),
        (
            with(r#","functions":{"cache_bytes":1073741824.5}"#),
            Refusal::NotNumber("functions.cache_bytes"),
        ),
        (
            with(r#","functions":{"cache_bytes":-1048576}"#),
            Refusal::NotNumber("functions.cache_bytes"),
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(refusal(&text), expected, "{text}");
    }
}

/// The certificate chain and the key of [`tls`], each as a JSON string.
fn tls_json() -> (String, String) {
    let (certificate, private) = tls();
    (
        serde_json::Value::String(certificate).to_string(),
        serde_json::Value::String(private).to_string(),
    )
}

#[test]
fn tls_is_certificate_blocks_and_one_private_key_block() {
    let (certificate, private) = tls();
    let config = |certificate: &str, private: &str| {
        serde_json::json!({
            "moruna": 1,
            "api": {
                "token_keys": [key(1)],
                "tls": { "certificate": certificate, "key": private }
            }
        })
        .to_string()
    };
    assert!(validate(&config(&certificate, &private)).is_ok());
    let chain = "api.tls.certificate";
    let wanted_chain = "one or more CERTIFICATE blocks and nothing else";
    let wanted_key = "exactly one PRIVATE KEY block";
    let cases = [
        (
            config("", &private),
            Refusal::Blocks {
                path: chain,
                wanted: wanted_chain,
            },
        ),
        (
            config(&(certificate.clone() + &private), &private),
            Refusal::Blocks {
                path: chain,
                wanted: wanted_chain,
            },
        ),
        (
            config(&format!("subject=CN=node\n{certificate}"), &private),
            Refusal::Pem {
                path: chain,
                error: PemError::Outside,
            },
        ),
        (
            config(certificate.trim_end_matches("-----\n"), &private),
            Refusal::Pem {
                path: chain,
                error: PemError::Unfinished,
            },
        ),
        (
            config(&certificate, ""),
            Refusal::Blocks {
                path: "api.tls.key",
                wanted: wanted_key,
            },
        ),
        (
            config(&certificate, &(private.clone() + &private)),
            Refusal::Blocks {
                path: "api.tls.key",
                wanted: wanted_key,
            },
        ),
        (
            config(&certificate, &block("RSA PRIVATE KEY", &[1; 64])),
            Refusal::Blocks {
                path: "api.tls.key",
                wanted: wanted_key,
            },
        ),
        (
            config(&certificate, &private.replace("MjIy", "!!!!")),
            Refusal::Pem {
                path: "api.tls.key",
                error: PemError::Body,
            },
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(refusal(&text), expected, "{text}");
    }
    assert_eq!(
        refusal(&config("", &private)).to_string(),
        "`api.tls.certificate` does not hold one or more CERTIFICATE blocks and nothing else"
    );
}

#[test]
fn the_function_cache_is_whole_mib_from_64_mib_to_half_the_node_s_memory() {
    let sized = |bytes: u64| with(&format!(r#","functions":{{"cache_bytes":{bytes}}}"#));
    let cache = |bytes: u64, memory: u64| {
        NodeConfig::validate(sized(bytes).as_bytes(), memory).map(|c| c.functions.cache_bytes)
    };
    assert_eq!(cache(64 << 20, MEMORY), Ok(64 << 20));
    assert_eq!(cache(MEMORY / 2, MEMORY), Ok(MEMORY / 2));
    assert_eq!(
        cache((64 << 20) + 1, MEMORY),
        Err(Refusal::CacheNotMib((64 << 20) + 1))
    );
    assert_eq!(cache(0, MEMORY), Err(Refusal::CacheTooSmall(0)));
    assert_eq!(
        cache(63 << 20, MEMORY),
        Err(Refusal::CacheTooSmall(63 << 20))
    );
    assert_eq!(
        cache(MEMORY / 2 + (1 << 20), MEMORY),
        Err(Refusal::CacheTooLarge {
            given: MEMORY / 2 + (1 << 20),
            most: MEMORY / 2
        })
    );
    // A node of 100 MiB has no room for the least cache.
    assert_eq!(
        cache(64 << 20, 100 << 20),
        Err(Refusal::CacheTooLarge {
            given: 64 << 20,
            most: 50 << 20
        })
    );
    // The machine account of a 24 GiB node (PM, 2026-11-18): half of it, rounded down to a MiB.
    let node = 24_526_286_848;
    assert_eq!(cache(11_695 << 20, node), Ok(11_695 << 20));
    assert!(matches!(
        cache(11_696 << 20, node),
        Err(Refusal::CacheTooLarge { most, .. }) if most == node / 2
    ));
}

#[test]
fn the_default_function_cache_is_1_gib_or_half_the_node_s_memory_and_never_refused() {
    let default = |text: &str, memory: u64| {
        NodeConfig::validate(text.as_bytes(), memory).map(|c| c.functions.cache_bytes)
    };
    // With no `functions`, and with `functions` that leaves `cache_bytes` out.
    for text in [with(""), with(r#","functions":{"keys":[]}"#)] {
        // 1 GiB on a node with at least 2 GiB.
        assert_eq!(default(&text, MEMORY), Ok(1 << 30));
        assert_eq!(default(&text, 2 << 30), Ok(CACHE_BYTES_DEFAULT));
        // Half the node's memory on a smaller one, not rounded and not held to 64 MiB.
        assert_eq!(default(&text, (2 << 30) - 4096), Ok((1 << 30) - 2048));
        assert_eq!(default(&text, 1 << 30), Ok(512 << 20));
        assert_eq!(default(&text, 100 << 20), Ok(50 << 20));
        assert_eq!(default(&text, 0), Ok(0));
    }
}

#[test]
fn every_refusal_is_one_printable_line_of_its_own() {
    let refusals = [
        Refusal::Missing,
        Refusal::NotBase64,
        Refusal::NotJson("expected value at line 1 column 1".to_string()),
        Refusal::Twice("the field `api` is written twice at line 1 column 9".to_string()),
        Refusal::NotObject("api"),
        Refusal::Unknown {
            within: "api",
            field: "extra".to_string(),
        },
        Refusal::Absent("api"),
        Refusal::NotString("api.tls.key"),
        Refusal::NotStrings("api.token_keys"),
        Refusal::NotNumber("functions.cache_bytes"),
        Refusal::NoVersion,
        Refusal::Version("2".to_string()),
        Refusal::KeyCount {
            path: "api.token_keys",
            count: 9,
            least: 1,
        },
        Refusal::Key {
            path: "api.token_keys",
            index: 0,
        },
        Refusal::Pem {
            path: "api.tls.key",
            error: PemError::Outside,
        },
        Refusal::Pem {
            path: "api.tls.key",
            error: PemError::Unfinished,
        },
        Refusal::Pem {
            path: "api.tls.key",
            error: PemError::Body,
        },
        Refusal::Blocks {
            path: "api.tls.key",
            wanted: "exactly one PRIVATE KEY block",
        },
        Refusal::CacheNotMib(1),
        Refusal::CacheTooSmall(1 << 20),
        Refusal::CacheTooLarge {
            given: 1 << 40,
            most: 1 << 32,
        },
    ];
    let mut seen: Vec<String> = Vec::new();
    for refusal in refusals {
        let line = refusal.to_string();
        assert!(!line.is_empty());
        assert!(
            line.chars().all(|c| c == ' ' || c.is_ascii_graphic()),
            "{line:?}"
        );
        assert!(!seen.contains(&line), "{line:?} is said twice");
        seen.push(line);
    }
    // What the JSON reader says about bytes from outside is cut and cleaned too.
    let long = format!("{{\"{}\":", "\u{7}".repeat(300));
    let line = refusal(&long).to_string();
    assert!(line.len() < 200, "{line:?}");
    assert!(line.chars().all(|c| c == ' ' || c.is_ascii_graphic()));
}

#[test]
fn the_tls_key_is_never_printed() {
    let (certificate, key) = tls();
    let shown = format!(
        "{:?}",
        Tls {
            certificate: certificate.clone(),
            key: key.clone()
        }
    );
    assert!(shown.contains("CERTIFICATE"));
    assert!(!shown.contains("PRIVATE KEY"));
    assert!(!shown.contains(&base64::encode(&[0x32; 138])[..16]));
}

/// A hand-over with everything node config can hold.
fn handover() -> Handover {
    let (certificate, key) = tls();
    Handover {
        config: NodeConfig {
            api: Api {
                token_keys: vec![PublicKey([7; 32]), PublicKey([0xfb; 32])],
                tls: Some(Tls { certificate, key }),
            },
            functions: Functions {
                keys: vec![PublicKey([9; 32])],
                cache_bytes: 256 << 20,
                cache_bytes_given: true,
            },
        },
        instance: "0d6b3a3c-7b1f-4f0e-9d39-2f6c1a9e5b77".to_string(),
        cloud: Cloud::OpenStack,
        wall_clock: 1_794_931_200,
        read_at_ns: 2_140_000_000,
        memory_bytes: MEMORY,
        boot_ms: None,
    }
}

/// The node's `boot_ms` goes only in a later start's hand-over, and comes back as it went
/// (item 2.2c).
#[test]
fn a_later_start_s_hand_over_carries_the_node_s_boot_ms() {
    // The first start's has none: its message is as before.
    let first = handover();
    let message = first.encode().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
    assert!(json.get("boot_ms").is_none(), "{json}");
    assert_eq!(Handover::decode(&message).unwrap().boot_ms, None);
    // A later start's carries it.
    let later = Handover {
        boot_ms: Some(2_518),
        ..handover()
    };
    let message = later.encode().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
    assert_eq!(json["boot_ms"], 2_518);
    assert_eq!(Handover::decode(&message), Ok(later));
    // It is a count of milliseconds, or the hand-over is refused.
    for value in [
        serde_json::json!("2518"),
        serde_json::json!(-1),
        serde_json::json!(2.5),
    ] {
        let mut json = json.clone();
        json["boot_ms"] = value.clone();
        assert_eq!(
            Handover::decode(json.to_string().as_bytes()),
            Err(HandoverError::Shape(Refusal::NotNumber("boot_ms"))),
            "{value}"
        );
    }
}

#[test]
fn a_hand_over_survives_the_round_trip() {
    let full = handover();
    assert_eq!(Handover::decode(&full.encode().unwrap()), Ok(full.clone()));
    // Without TLS and with the defaults, on each cloud.
    for cloud in [Cloud::OpenStack, Cloud::Azure, Cloud::None] {
        let least = Handover {
            config: validate(&with("")).unwrap(),
            cloud,
            ..full.clone()
        };
        let message = least.encode().unwrap();
        assert_eq!(Handover::decode(&message), Ok(least));
    }
    // `api` judges the config on the node the hand-over says: a cache that fits a large node
    // is taken, and the same cache on a smaller one is refused.
    let large = Handover {
        config: NodeConfig {
            functions: Functions {
                keys: Vec::new(),
                cache_bytes: 1 << 40,
                cache_bytes_given: true,
            },
            ..full.config.clone()
        },
        memory_bytes: 2 << 40,
        ..full
    };
    assert_eq!(
        Handover::decode(&large.encode().unwrap()),
        Ok(large.clone())
    );
    let small = Handover {
        memory_bytes: (2 << 40) - (2 << 20),
        ..large
    };
    assert_eq!(
        Handover::decode(&small.encode().unwrap()),
        Err(HandoverError::Config(Refusal::CacheTooLarge {
            given: 1 << 40,
            most: (1 << 40) - (1 << 20)
        }))
    );
}

#[test]
fn a_hand_over_is_one_json_object_with_node_config_written_out() {
    let full = handover();
    let message = full.encode().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
    let (certificate, key) = tls();
    assert_eq!(
        json,
        serde_json::json!({
            "cloud": "openstack",
            "instance": "0d6b3a3c-7b1f-4f0e-9d39-2f6c1a9e5b77",
            "wall_clock": 1_794_931_200u64,
            "read_at_ns": 2_140_000_000u64,
            "memory_bytes": 8_589_934_592u64,
            "config": {
                "moruna": 1,
                "api": {
                    "token_keys": [base64::encode(&[7; 32]), base64::encode(&[0xfb; 32])],
                    "tls": { "certificate": certificate, "key": key }
                },
                "functions": { "keys": [base64::encode(&[9; 32])], "cache_bytes": 268_435_456 }
            }
        })
    );
    // The config inside is a node config: the validation that took it takes it again.
    let config = json["config"].to_string();
    assert_eq!(validate(&config), Ok(full.config));
    // Without TLS the field is left out, not null.
    let plain = Handover {
        config: validate(&with("")).unwrap(),
        ..handover()
    };
    let json: serde_json::Value = serde_json::from_slice(&plain.encode().unwrap()).unwrap();
    assert_eq!(json["config"]["api"].as_object().unwrap().len(), 1);
    // Nor are the defaults written: the operator set no `functions`.
    assert_eq!(json["config"].as_object().unwrap().len(), 2);
    assert!(json["config"].get("functions").is_none());
}

/// A node of 2 GiB, as QEMU's machine gives the supervisor: its machine account, the node's
/// memory, is 2 032 979 968 bytes, and half of it is not a whole number of MiB.
const QEMU_2_GIB: u64 = 2_032_979_968;

/// Item 1.4's booted check found `api` refusing such a node's hand-over: the default cache went
/// into it as if the operator had given it, and was judged as theirs (PM, 2026-11-18).
#[test]
fn a_config_with_no_functions_passes_on_a_2_gib_node() {
    let config = NodeConfig::validate(with("").as_bytes(), QEMU_2_GIB).expect("valid");
    assert_eq!(config.functions.cache_bytes, QEMU_2_GIB / 2);
    assert_ne!(config.functions.cache_bytes % MIB, 0);
    assert!(!config.functions.cache_bytes_given);
    let handed = Handover {
        config,
        memory_bytes: QEMU_2_GIB,
        ..handover()
    };
    let message = handed.encode().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
    assert!(json["config"].get("functions").is_none(), "{json}");
    // `api` takes it, and derives the same default.
    assert_eq!(Handover::decode(&message), Ok(handed));
}

/// What the hand-over carries is what the operator set, and `api` derives the rest as
/// `platform` did: on every node, whatever its memory.
#[test]
fn the_hand_over_carries_what_the_operator_set_and_the_defaults_come_out_the_same() {
    let configs = [
        with(""),
        with(r#","functions":{}"#),
        with(r#","functions":{"keys":[]}"#),
        with(&format!(r#","functions":{{"keys":["{}"]}}"#, key(9))),
        with(r#","functions":{"cache_bytes":67108864}"#),
        with(&format!(
            r#","functions":{{"keys":["{}"],"cache_bytes":134217728}}"#,
            key(9)
        )),
    ];
    let memories = [
        QEMU_2_GIB,
        2 << 30,
        (2 << 30) + 1,
        24_526_286_848,
        MEMORY,
        (1 << 30) - 4096,
        300 << 20,
        129 << 20,
        1,
    ];
    let mut handed_over = 0;
    for text in &configs {
        for memory in memories {
            // `platform`'s validation, on this node.
            let Ok(config) = NodeConfig::validate(text.as_bytes(), memory) else {
                // A given cache over half a small node's memory: refused by both alike.
                assert!(
                    text.contains("cache_bytes") && memory / 2 < 128 << 20,
                    "{text} {memory}"
                );
                continue;
            };
            let handed = Handover {
                config: config.clone(),
                memory_bytes: memory,
                ..handover()
            };
            let message = handed.encode().unwrap();
            let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
            let carried = &json["config"];
            // Only what the operator set.
            assert_eq!(
                carried["functions"].get("cache_bytes").is_some(),
                text.contains("cache_bytes"),
                "{text}: {carried}"
            );
            assert_eq!(
                carried.get("functions").is_some(),
                text.contains("cache_bytes") || text.contains(&key(9)),
                "{text}: {carried}"
            );
            // `api` comes out with exactly `platform`'s config, the default included.
            let taken =
                Handover::decode(&message).unwrap_or_else(|e| panic!("{text} {memory}: {e}"));
            assert_eq!(taken, handed, "{text} {memory}");
            assert_eq!(
                taken.config.functions.cache_bytes,
                config.functions.cache_bytes
            );
            handed_over += 1;
        }
    }
    assert!(handed_over > 40, "{handed_over}");
}

/// A cache the operator gives is carried even when it is the default's value, and judged again
/// as theirs.
#[test]
fn a_given_cache_is_carried_even_at_the_default_s_value() {
    let given = NodeConfig::validate(
        with(r#","functions":{"cache_bytes":1073741824}"#).as_bytes(),
        MEMORY,
    )
    .unwrap();
    let left_out = NodeConfig::validate(with("").as_bytes(), MEMORY).unwrap();
    assert_eq!(given.functions.cache_bytes, left_out.functions.cache_bytes);
    assert_ne!(given, left_out);
    let message = Handover {
        config: given.clone(),
        ..handover()
    }
    .encode()
    .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&message).unwrap();
    assert_eq!(json["config"]["functions"]["cache_bytes"], 1 << 30);
    assert_eq!(Handover::decode(&message).unwrap().config, given);
}

#[test]
fn a_hand_over_that_is_not_one_is_refused() {
    let good: serde_json::Value = serde_json::from_slice(&handover().encode().unwrap()).unwrap();
    let changed = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut json = good.clone();
        change(&mut json);
        Handover::decode(json.to_string().as_bytes()).expect_err("the hand-over is refused")
    };
    assert_eq!(
        changed(&|json| json["cloud"] = "gcp".into()),
        HandoverError::Cloud
    );
    for id in ["", "two words", "line\nbreak", &"x".repeat(65), "caf\u{e9}"] {
        assert_eq!(
            changed(&|json| json["instance"] = id.into()),
            HandoverError::Instance,
            "{id:?}"
        );
    }
    assert_eq!(
        changed(&|json| json["config"]["api"]["token_keys"][0] = "AAAA".into()),
        HandoverError::Config(Refusal::Key {
            path: "api.token_keys",
            index: 0
        })
    );
    assert_eq!(
        changed(&|json| json["config"]["moruna"] = 2.into()),
        HandoverError::Config(Refusal::Version("2".to_string()))
    );
    assert_eq!(
        changed(&|json| json["booted_ms"] = 1.into()),
        HandoverError::Shape(Refusal::Unknown {
            within: "the hand-over",
            field: "booted_ms".to_string()
        })
    );
    assert_eq!(
        changed(&|json| json["wall_clock"] = "now".into()),
        HandoverError::Shape(Refusal::NotNumber("wall_clock"))
    );
    assert_eq!(
        changed(&|json| json["read_at_ns"] = (-1).into()),
        HandoverError::Shape(Refusal::NotNumber("read_at_ns"))
    );
    assert_eq!(
        changed(&|json| json["memory_bytes"] = "8 GiB".into()),
        HandoverError::Shape(Refusal::NotNumber("memory_bytes"))
    );
    assert_eq!(
        changed(&|json| json["cloud"] = 1.into()),
        HandoverError::Shape(Refusal::NotString("cloud"))
    );
    for field in [
        "cloud",
        "instance",
        "wall_clock",
        "read_at_ns",
        "memory_bytes",
        "config",
    ] {
        let refused = changed(&|json| {
            json.as_object_mut().unwrap().remove(field);
        });
        assert!(
            matches!(refused, HandoverError::Shape(Refusal::Absent(name)) if name == field),
            "{field}"
        );
    }
    assert_eq!(
        Handover::decode(b"[]"),
        Err(HandoverError::Shape(Refusal::NotObject("the hand-over")))
    );
    assert!(matches!(
        Handover::decode(b"ready"),
        Err(HandoverError::Shape(Refusal::NotJson(_)))
    ));
    assert!(matches!(
        Handover::decode(b""),
        Err(HandoverError::Shape(Refusal::NotJson(_)))
    ));
}

#[test]
fn a_hand_over_fits_one_channel_message_or_is_refused() {
    assert_eq!(MESSAGE_MAX, 65536);
    // A certificate chain that makes the hand-over larger than one channel message.
    let (_, key) = tls();
    let mut large = handover();
    large.config.api.tls = Some(Tls {
        certificate: block("CERTIFICATE", &[0x30; 1500]).repeat(32),
        key,
    });
    let Err(HandoverError::TooLarge(bytes)) = large.encode() else {
        panic!("a hand-over over the limit was encoded");
    };
    assert!(bytes > MESSAGE_MAX);
    assert_eq!(
        HandoverError::TooLarge(70000).to_string(),
        "the hand-over is 70000 bytes, and a channel message carries 65536"
    );
    assert_eq!(
        Handover::decode(&vec![b' '; MESSAGE_MAX + 1]),
        Err(HandoverError::TooLarge(MESSAGE_MAX + 1))
    );
}

#[test]
fn hand_over_errors_say_what_they_are() {
    let lines = [
        HandoverError::Shape(Refusal::Absent("cloud")).to_string(),
        HandoverError::Cloud.to_string(),
        HandoverError::Instance.to_string(),
        HandoverError::Config(Refusal::NoVersion).to_string(),
    ];
    assert_eq!(
        lines,
        [
            "the hand-over is refused: `cloud` is missing",
            "the hand-over's `cloud` is not a cloud",
            "the hand-over's `instance` is not an instance ID",
            "the hand-over's node config is refused: `moruna` is missing: node config says its version",
        ]
    );
}

#[test]
fn the_wall_clock_is_carried_forward_by_the_monotonic_clock() {
    let handed = handover();
    assert_eq!(handed.wall_clock_at(handed.read_at_ns), 1_794_931_200);
    assert_eq!(
        handed.wall_clock_at(handed.read_at_ns + 999_999_999),
        1_794_931_200
    );
    assert_eq!(
        handed.wall_clock_at(handed.read_at_ns + 61_000_000_000),
        1_794_931_261
    );
    // A reading from before the clock was read does not move it back.
    assert_eq!(handed.wall_clock_at(0), 1_794_931_200);
    let last = Handover {
        wall_clock: u64::MAX,
        ..handed
    };
    assert_eq!(last.wall_clock_at(u64::MAX), u64::MAX);
}

#[test]
fn names_of_clouds_and_instances() {
    assert_eq!(cloud_name(Cloud::OpenStack), "openstack");
    assert_eq!(cloud_name(Cloud::Azure), "azure");
    assert_eq!(cloud_name(Cloud::None), "none");
    assert_eq!(READY, b"ready");
    assert!(instance_valid("0d6b3a3c-7b1f-4f0e-9d39-2f6c1a9e5b77"));
    assert!(instance_valid("i-0abc_DEF.1"));
    assert!(instance_valid(&"a".repeat(64)));
    for id in ["", " ", "a b", "a/b", "a\n", "caf\u{e9}", &"a".repeat(65)] {
        assert!(!instance_valid(id), "{id:?}");
    }
    assert_eq!(PublicKey([5; 32]).to_base64(), key(5));
    assert_eq!(PublicKey::from_base64(&key(5)), Some(PublicKey([5; 32])));
}
