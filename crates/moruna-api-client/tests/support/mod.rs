//! The node the clients' tests call: a node API server run on the host over TLS with a generated
//! identity, and the tokens it accepts (item 2.5, "Success").
//!
//! Until item 2.2's `moruna-api-server` is merged, the server is [`fake`], with [`cert`] for its
//! identity. Every test reaches it through [`TestNode`] only, so the swap is
//! [`TestNode::start`]'s body: drive `moruna-api-server` over a `TcpListener` on 127.0.0.1 with
//! `Identity::generate`, the same `instance` and `document` (as `NodeFacts`), and `key`'s public
//! half as its one token key; then delete `fake.rs` and `cert.rs`. [`token`] stays: it makes the
//! tokens 2.3 verifies.

pub mod cert;
pub mod fake;
pub mod token;

use std::time::{SystemTime, UNIX_EPOCH};

use moruna_api::{Cloud, Executor, ExecutorKind, Node, NodeHealth, Scope};
use moruna_api_client::Fingerprint;

/// The node's instance ID, its token's `aud`.
pub const INSTANCE: &str = "test-instance-0001";

/// A node on the host, serving the API over TLS.
pub struct TestNode {
    /// `https://127.0.0.1:<port>`.
    pub url: String,
    /// The SHA-256 of its certificate, what its console prints and clients pin.
    pub fingerprint: String,
    /// The node document it answers `node` with.
    pub document: Node,
    /// The key its tokens are signed with, one of its `token_keys`.
    pub key: token::Key,
}

impl TestNode {
    /// Start a node with a fresh identity and one token key.
    pub fn start() -> Self {
        let key = token::Key::generate();
        let document = document();
        let identity = cert::generate(INSTANCE, now());
        let fingerprint = Fingerprint::of(&identity.certificate).to_string();
        let address = fake::serve(fake::Config {
            instance: INSTANCE.to_string(),
            document: document.clone(),
            token_keys: vec![key.public()],
            identity,
        });
        Self {
            url: format!("https://{address}"),
            fingerprint,
            document,
            key,
        }
    }

    /// A token for this node with this scope, valid for ten minutes.
    pub fn token(&self, scope: Scope) -> String {
        self.key.token(INSTANCE, &[scope], 600)
    }

    /// The ID the node's audit gives the token key.
    pub fn key_id(&self) -> String {
        fake::key_id(&self.key.public())
    }

    /// A pin that is not this node's: its fingerprint with the last digit changed.
    pub fn wrong_pin(&self) -> String {
        let mut pin = self.fingerprint.clone();
        let last = if pin.ends_with('0') { "1" } else { "0" };
        pin.replace_range(63.., last);
        pin
    }
}

/// The node document of system design 2A's example.
pub fn document() -> Node {
    Node {
        version: "0.7.0".to_string(),
        instance: INSTANCE.to_string(),
        cloud: Cloud::OpenStack,
        cores: 4,
        memory_bytes: 8_589_934_592,
        executors: vec![Executor {
            kind: ExecutorKind::Cpu,
            slots: 4,
        }],
        boot_ms: 2140,
        time: 1_794_931_200,
        health: NodeHealth::Ok,
    }
}

/// The wall clock, in Unix seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
}
