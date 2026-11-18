//! What a store's errors say of its credentials (system design 3B, item 3.5): never a value, and
//! when the store refuses them, the secrets they came from, by name.
//!
//! A store's error is its own text, and what a server answers may come back in it: S3 says the
//! access key ID it was given, and a request's URL carries a SAS token. So every error of a store
//! built with credentials is read again before it leaves the reactor: each credential's value
//! (and a SAS token's signature, as it is and percent-decoded) is replaced with the secret it came
//! from, and an answer of 401 or 403 says which secrets the store was given. The kind of error
//! is kept: a missing object is still `NotFound`.

use object_store::Error;

use crate::ObjectStoreConfig;

/// One credential a store is built with.
#[derive(Debug)]
struct Credential {
    /// The document's field it fills (`object_store.s3.secret_access_key`).
    field: &'static str,
    /// The secret it was resolved from, by name; `None` when the runner gave it.
    secret: Option<String>,
    /// What is replaced wherever it appears: the value, and for a SAS token its signature.
    hidden: Vec<String>,
}

impl Credential {
    /// What stands in an error where the credential was.
    fn label(&self) -> String {
        match &self.secret {
            Some(name) => format!("<secret `{name}`>"),
            None => format!("<the credential in {}>", self.field),
        }
    }

    /// How a refusal names it.
    fn named(&self) -> String {
        match &self.secret {
            Some(name) => format!("secret `{name}` ({})", self.field),
            None => format!("the runner's {}", self.field),
        }
    }
}

/// Which store a guard is for: its credentials are that store's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    S3,
    Gcs,
    Azure,
}

/// The credentials of one store, which its errors never carry. A guard with none passes every
/// error through as it is.
#[derive(Debug, Default)]
pub(crate) struct Guard {
    credentials: Vec<Credential>,
    /// Every hidden value with its label, the longest first, so one inside another is never
    /// left half replaced.
    hidden: Vec<(String, String)>,
}

impl Guard {
    /// The guard of `store` as `cfg` builds it.
    pub(crate) fn of(cfg: &ObjectStoreConfig, store: Store) -> Guard {
        let mut credentials = Vec::new();
        let mut add = |field: &'static str, value: &Option<String>, sas: bool| {
            let Some(value) = value.as_deref().filter(|v| !v.is_empty()) else {
                return;
            };
            let mut hidden = vec![value.to_string()];
            if sas {
                hidden.extend(signature(value));
            }
            credentials.push(Credential {
                field,
                secret: cfg.secret_names.get(field).cloned(),
                hidden,
            });
        };
        match store {
            Store::S3 => {
                if let Some(s3) = &cfg.s3 {
                    add("object_store.s3.access_key_id", &s3.access_key_id, false);
                    add(
                        "object_store.s3.secret_access_key",
                        &s3.secret_access_key,
                        false,
                    );
                    add("object_store.s3.session_token", &s3.session_token, false);
                }
            }
            Store::Gcs => {
                if let Some(gcs) = &cfg.gcs {
                    add(
                        "object_store.gcs.service_account_json",
                        &gcs.service_account_json,
                        false,
                    );
                }
            }
            Store::Azure => {
                if let Some(azure) = &cfg.azure {
                    add("object_store.azure.access_key", &azure.access_key, false);
                    add("object_store.azure.sas_token", &azure.sas_token, true);
                }
            }
        }
        let mut hidden: Vec<(String, String)> = credentials
            .iter()
            .flat_map(|c| c.hidden.iter().map(|h| (h.clone(), c.label())))
            .collect();
        hidden.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        Guard {
            credentials,
            hidden,
        }
    }

    /// `text` with every credential's value replaced by the secret it came from.
    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_string();
        for (value, label) in &self.hidden {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), label);
            }
        }
        text
    }

    /// What a refusal of the store's adds: the secrets it was given, by name.
    fn refused(&self) -> String {
        let named: Vec<String> = self.credentials.iter().map(Credential::named).collect();
        format!(
            "the store refused the credentials this job gave it: {}",
            named.join(", ")
        )
    }

    /// `error` as it leaves the reactor: of the same kind, with no credential's value in it,
    /// and, when the store refused the credentials, naming the secrets they came from.
    pub(crate) fn error(&self, error: Error) -> Error {
        if self.credentials.is_empty() {
            return error;
        }
        let refused = is_refusal(&error);
        let said = |source: &dyn std::fmt::Display| -> Box<dyn std::error::Error + Send + Sync> {
            let mut text = self.redact(&source.to_string());
            if refused {
                text = format!("{text}; {}", self.refused());
            }
            text.into()
        };
        match error {
            Error::Generic { store, source } => Error::Generic {
                store,
                source: said(&source),
            },
            Error::NotFound { path, source } => Error::NotFound {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::AlreadyExists { path, source } => Error::AlreadyExists {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::Precondition { path, source } => Error::Precondition {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::NotModified { path, source } => Error::NotModified {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::PermissionDenied { path, source } => Error::PermissionDenied {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::Unauthenticated { path, source } => Error::Unauthenticated {
                path: self.redact(&path),
                source: said(&source),
            },
            Error::NotSupported { source } => Error::NotSupported {
                source: said(&source),
            },
            // A path that does not parse, a task that did not join, an operation or a
            // configuration key a store does not know: none holds what a server said.
            other => other,
        }
    }
}

/// Whether `error` is the store refusing its credentials: a 401 or a 403, as `object_store`
/// says it, whichever kind it made of it.
fn is_refusal(error: &Error) -> bool {
    if matches!(
        error,
        Error::PermissionDenied { .. } | Error::Unauthenticated { .. }
    ) {
        return true;
    }
    let text = error.to_string();
    ["401 Unauthorized", "403 Forbidden"]
        .iter()
        .any(|status| text.contains(status))
}

/// A SAS token's signature, `sig`, as the token has it and percent-decoded: the part of it that
/// is the secret, which a URL or an answer may carry apart from the rest.
fn signature(token: &str) -> Vec<String> {
    let token = token.trim_start_matches('?');
    let Some(sig) = token
        .split('&')
        .find_map(|pair| pair.strip_prefix("sig="))
        .filter(|sig| !sig.is_empty())
    else {
        return Vec::new();
    };
    let decoded = percent_decoded(sig);
    if decoded == sig {
        vec![sig.to_string()]
    } else {
        vec![sig.to_string(), decoded]
    }
}

/// `text` with each `%XX` read as its byte; anything else as it is.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AzureConfig, GcsConfig, S3Config};

    const KEY_ID: &str = "AKIA-TEST-0001";
    const SECRET: &str = "wJalrXUtnFEMI-K7MDENG-TEST-SECRET";

    fn s3(names: bool) -> ObjectStoreConfig {
        let mut cfg = ObjectStoreConfig {
            s3: Some(S3Config {
                access_key_id: Some(KEY_ID.into()),
                secret_access_key: Some(SECRET.into()),
                ..S3Config::default()
            }),
            ..ObjectStoreConfig::default()
        };
        if names {
            cfg.secret_names
                .insert("object_store.s3.access_key_id".into(), "s3-key".into());
            cfg.secret_names.insert(
                "object_store.s3.secret_access_key".into(),
                "s3-secret".into(),
            );
        }
        cfg
    }

    fn generic(text: &str) -> Error {
        Error::Generic {
            store: "S3",
            source: text.to_string().into(),
        }
    }

    #[test]
    fn a_refusal_names_the_secrets_and_never_says_their_values() {
        let guard = Guard::of(&s3(true), Store::S3);
        // S3 says back the key ID it was given; a careless server, the secret too.
        let body = format!(
            "<Error><Code>InvalidAccessKeyId</Code><AWSAccessKeyId>{KEY_ID}</AWSAccessKeyId>\
             <Echo>{SECRET}</Echo></Error>"
        );
        let error = guard.error(Error::PermissionDenied {
            path: "bucket/key".into(),
            source: body.into(),
        });
        assert!(matches!(error, Error::PermissionDenied { .. }), "{error}");
        let said = error.to_string();
        assert!(!said.contains(KEY_ID) && !said.contains(SECRET), "{said}");
        assert!(said.contains("<secret `s3-key`>"), "{said}");
        assert!(
            said.contains(
                "the store refused the credentials this job gave it: secret `s3-key` \
                 (object_store.s3.access_key_id), secret `s3-secret` \
                 (object_store.s3.secret_access_key)"
            ),
            "{said}"
        );
        // A 403 the store made a generic error of is a refusal too, and so is a 401.
        for status in ["403 Forbidden", "401 Unauthorized"] {
            let said = guard
                .error(generic(&format!(
                    "Server returned non-2xx status code: {status}"
                )))
                .to_string();
            assert!(said.contains("secret `s3-secret`"), "{said}");
        }
        let said = guard
            .error(Error::Unauthenticated {
                path: "p".into(),
                source: "no".into(),
            })
            .to_string();
        assert!(said.contains("refused the credentials"), "{said}");
    }

    #[test]
    fn every_kind_of_error_is_kept_and_read_again() {
        let guard = Guard::of(&s3(true), Store::S3);
        let source =
            || -> Box<dyn std::error::Error + Send + Sync> { format!("echo {SECRET}").into() };
        let errors = [
            guard.error(Error::NotFound {
                path: "k".into(),
                source: source(),
            }),
            guard.error(Error::AlreadyExists {
                path: "k".into(),
                source: source(),
            }),
            guard.error(Error::Precondition {
                path: "k".into(),
                source: source(),
            }),
            guard.error(Error::NotModified {
                path: "k".into(),
                source: source(),
            }),
            guard.error(Error::NotSupported { source: source() }),
            guard.error(generic(&format!("connection reset; {SECRET}"))),
        ];
        assert!(matches!(errors[0], Error::NotFound { .. }));
        assert!(matches!(errors[1], Error::AlreadyExists { .. }));
        assert!(matches!(errors[2], Error::Precondition { .. }));
        assert!(matches!(errors[3], Error::NotModified { .. }));
        assert!(matches!(errors[4], Error::NotSupported { .. }));
        for error in &errors {
            let said = error.to_string();
            assert!(!said.contains(SECRET), "{said}");
            assert!(said.contains("<secret `s3-secret`>"), "{said}");
            assert!(!said.contains("refused the credentials"), "{said}");
        }
        // What holds nothing a server said passes as it is.
        let kept = guard.error(Error::NotImplemented {
            operation: "op".into(),
            implementer: "x".into(),
        });
        assert!(matches!(kept, Error::NotImplemented { .. }));
    }

    #[test]
    fn a_credential_the_runner_gave_is_named_by_its_field() {
        let guard = Guard::of(&s3(false), Store::S3);
        let said = guard
            .error(Error::PermissionDenied {
                path: "k".into(),
                source: format!("id {KEY_ID}").into(),
            })
            .to_string();
        assert!(!said.contains(KEY_ID), "{said}");
        assert!(
            said.contains("<the credential in object_store.s3.access_key_id>"),
            "{said}"
        );
        assert!(
            said.contains("the runner's object_store.s3.secret_access_key"),
            "{said}"
        );
    }

    #[test]
    fn a_store_with_no_credentials_passes_its_errors_as_they_are() {
        let guard = Guard::of(&ObjectStoreConfig::default(), Store::S3);
        let said = guard.error(generic("403 Forbidden")).to_string();
        assert_eq!(said, "Generic S3 error: 403 Forbidden");
        // Another store's credentials are not this one's.
        let guard = Guard::of(&s3(true), Store::Azure);
        assert_eq!(guard.redact(SECRET), SECRET);
        assert!(Guard::of(&s3(true), Store::Gcs).credentials.is_empty());
    }

    #[test]
    fn a_sas_token_and_its_signature_are_hidden_and_the_key_of_gcs_too() {
        let token = "?sv=2024-11-04&ss=b&sp=rl&sig=AbC%2BdEf%3D";
        let mut cfg = ObjectStoreConfig {
            azure: Some(AzureConfig {
                account: Some("acct".into()),
                sas_token: Some(token.into()),
                ..AzureConfig::default()
            }),
            gcs: Some(GcsConfig {
                service_account_json: Some("{\"private_key\":\"PEM-KEY\"}".into()),
                ..GcsConfig::default()
            }),
            ..ObjectStoreConfig::default()
        };
        cfg.secret_names
            .insert("object_store.azure.sas_token".into(), "blob-sas".into());
        let guard = Guard::of(&cfg, Store::Azure);
        for text in [
            token.to_string(),
            "GET https://acct.blob.core.windows.net/c/k?sig=AbC%2BdEf%3D".to_string(),
            "signature AbC+dEf= did not match".to_string(),
        ] {
            let said = guard.redact(&text);
            assert!(
                !said.contains("AbC%2BdEf%3D") && !said.contains("AbC+dEf="),
                "{said}"
            );
            assert!(said.contains("<secret `blob-sas`>"), "{said}");
        }
        let said =
            Guard::of(&cfg, Store::Gcs).redact("key PEM-KEY in {\"private_key\":\"PEM-KEY\"}");
        assert!(!said.contains("{\"private_key\""), "{said}");
    }

    #[test]
    fn a_signature_is_read_from_the_token() {
        assert_eq!(signature("sv=1&sig=abc"), ["abc"]);
        assert_eq!(signature("sig=a%2Fb&sv=1"), ["a%2Fb", "a/b"]);
        assert!(signature("sv=1&sp=r").is_empty());
        assert!(signature("sig=").is_empty());
        assert_eq!(percent_decoded("%zz%4"), "%zz%4");
        assert_eq!(percent_decoded("a%20b"), "a b");
    }
}
