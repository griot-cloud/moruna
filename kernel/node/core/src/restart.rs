//! A service the supervisor starts again when it dies (system design 1B and 2B, "If `api`
//! dies"; work item 2.2c).
//!
//! node.conf names it with one record, which says everything a fresh start needs:
//!
//! ```text
//! restart SERVICE SLOT SERVER GRANTING POLICY...
//! restart api network network listen-supervisor rights=listen local=0.0.0.0/0@443 sockets=81 rx=65536 tx=65536 per-peer=8 linger=5000
//! ```
//!
//! - `SERVICE` is the service that is started again; `SLOT` is the start slot its listener of
//!   the network server goes in.
//! - `SERVER` is the network server's service, and `GRANTING` its listener slot whose other end
//!   the supervisor keeps: the server's grant for it holds the `grant` right, and every start of
//!   `SERVICE` mints that listener from it (`Client::grant`).
//! - `POLICY` is the minted listener's policy, in network.conf's words: `rights=`, `remote=`,
//!   `local=`, `names=`, `sockets=`, and item 1.3b's `rx=`, `tx=`, `per-peer=` and `linger=`
//!   ([`parse_policy`]). The cloud profile's is 1.3b's grant for api's listener.
//!
//! A service that is started again has only what a start can make afresh: its console, its
//! account, the minted listener, and, when node.conf bootstraps `platform`, a fresh channel to
//! `platform` on its slot `platform`, whose other end `platform` is sent with [`API_STARTED`]
//! (PM, item 2.2c). So it may take no grant, link or bootstrap of node.conf's.
//!
//! It is started again at most [`RESTARTS_MAX`] times in [`WINDOW_NS`]: the next death in that
//! time stops the node ([`Deaths`]).

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::net::IpAddr;

use netstack_protocol::{
    Addr, BUFFER_MAX, BUFFER_MIN, Cidr, LINGER_MAX_MS, NameRule, PER_PEER_MAX, Policy, Rule, rights,
};

use crate::{ConfigError, identifier};

/// The most times the service is started again within [`WINDOW_NS`].
pub const RESTARTS_MAX: usize = 5;
/// Ten minutes on the monotonic clock.
pub const WINDOW_NS: u64 = 10 * 60 * 1_000_000_000;
/// The slot a restarted service gets its fresh channel to `platform` on.
pub const PLATFORM_SLOT: &str = "platform";

/// The message the supervisor sends `platform` on its bootstrap channel at each start of the
/// service that restarts: these bytes and one handle, the service's fresh channel's other end
/// (PM, item 2.2c). It follows the bootstrap's own message, the node's memory.
pub const API_STARTED: &[u8] = b"api-started";

/// Whether a message is exactly [`API_STARTED`] with its one handle.
pub fn api_started(bytes: &[u8], handles: usize) -> bool {
    bytes == API_STARTED && handles == 1
}

/// node.conf's `restart` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    /// The service started again.
    pub service: String,
    /// The start slot its minted listener goes in.
    pub slot: String,
    /// The network server's service.
    pub server: String,
    /// The server's listener slot the supervisor grants from.
    pub granting: String,
    /// The minted listener's policy.
    pub policy: Policy,
}

impl Restart {
    /// The record's words after `restart`.
    pub(crate) fn parse(words: &[&str]) -> Result<Restart, ConfigError> {
        let [service, slot, server, granting, options @ ..] = words else {
            return Err(ConfigError::Syntax);
        };
        if ![service, slot, server, granting]
            .iter()
            .all(|word| identifier(word))
            || service == server
            || ["account", "console", PLATFORM_SLOT].contains(slot)
        {
            return Err(ConfigError::Syntax);
        }
        Ok(Restart {
            service: service.to_string(),
            slot: slot.to_string(),
            server: server.to_string(),
            granting: granting.to_string(),
            policy: parse_policy(options)?,
        })
    }
}

/// A policy in network.conf's words (`kernel/net/stack/src/config.rs`, `grant`): `rights=` (one
/// or more of `connect`, `listen`, `udp`, `ping`, `resolve`, `grant`, and required), `remote=`
/// and `local=` rules (`any`, or `ADDR/PREFIX[@PORT|@FIRST-LAST]`, comma separated), `names=`
/// patterns (`PATTERN[@PORTS]`), `sockets=` (1 to 65536, 64 by default), and item 1.3b's
/// bounds: `rx=` and `tx=` (buffers, [`BUFFER_MIN`] to [`BUFFER_MAX`] bytes), `per-peer=` (1 to
/// [`PER_PEER_MAX`]) and `linger=` (1 to [`LINGER_MAX_MS`] ms), each left to the network server
/// when not given. Anything else is a syntax error.
pub fn parse_policy(words: &[&str]) -> Result<Policy, ConfigError> {
    let mut policy = Policy {
        sockets: 64,
        ..Policy::default()
    };
    let mut had_rights = false;
    let number = |value: &str, range: core::ops::RangeInclusive<u32>| {
        value.parse().ok().filter(|v| range.contains(v))
    };
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        let parsed = match key {
            "rights" => parse_rights(value).map(|bits| {
                policy.rights = bits;
                had_rights = true;
            }),
            "remote" => parse_rules(value).map(|rules| policy.remote.extend(rules)),
            "local" => parse_rules(value).map(|rules| policy.local.extend(rules)),
            "names" => parse_names(value).map(|names| policy.names.extend(names)),
            "sockets" => value
                .parse()
                .ok()
                .filter(|sockets| (1..=65536).contains(sockets))
                .map(|sockets| policy.sockets = sockets),
            "rx" => number(value, BUFFER_MIN..=BUFFER_MAX).map(|bytes| policy.rx = Some(bytes)),
            "tx" => number(value, BUFFER_MIN..=BUFFER_MAX).map(|bytes| policy.tx = Some(bytes)),
            "per-peer" => {
                number(value, 1..=PER_PEER_MAX).map(|peers| policy.per_peer = Some(peers))
            }
            "linger" => number(value, 1..=LINGER_MAX_MS).map(|ms| policy.linger_ms = Some(ms)),
            _ => None,
        };
        if parsed.is_none() {
            return Err(ConfigError::Syntax);
        }
    }
    if !had_rights || policy.validate().is_err() {
        return Err(ConfigError::Syntax);
    }
    Ok(policy)
}

fn parse_addr(text: &str) -> Option<Addr> {
    text.parse::<IpAddr>().ok().map(Addr)
}

/// `ADDR/PREFIX`, the prefix within the family's bits.
fn parse_cidr(text: &str) -> Option<Cidr> {
    let (addr, prefix) = text.split_once('/')?;
    let addr = parse_addr(addr)?;
    let prefix: u8 = prefix.parse().ok()?;
    (prefix <= addr.bits()).then_some(Cidr::new(addr, prefix))
}

/// `PORT` or `FIRST-LAST`.
fn parse_ports(text: &str) -> Option<(u16, u16)> {
    let (first, last) = match text.split_once('-') {
        Some((first, last)) => (first.parse().ok()?, last.parse().ok()?),
        None => {
            let port = text.parse().ok()?;
            (port, port)
        }
    };
    (first <= last).then_some((first, last))
}

/// A rule list: `any`, or `ADDR/PREFIX[@PORTS]`, comma separated.
fn parse_rules(text: &str) -> Option<Vec<Rule>> {
    let mut rules = Vec::new();
    for item in text.split(',') {
        if item == "any" {
            rules.extend(Policy::everything(0).remote);
            continue;
        }
        let (net, ports) = match item.split_once('@') {
            Some((net, ports)) => (net, Some(ports)),
            None => (item, None),
        };
        let net = parse_cidr(net)?;
        let (first, last) = match ports {
            Some(ports) => parse_ports(ports)?,
            None => (0, u16::MAX),
        };
        rules.push(Rule { net, first, last });
    }
    Some(rules)
}

fn parse_names(text: &str) -> Option<Vec<NameRule>> {
    text.split(',')
        .map(|item| {
            let (pattern, ports) = match item.split_once('@') {
                Some((pattern, ports)) => (pattern, Some(ports)),
                None => (item, None),
            };
            let (first, last) = match ports {
                Some(ports) => parse_ports(ports)?,
                None => (0, u16::MAX),
            };
            Some(NameRule {
                pattern: pattern.to_ascii_lowercase(),
                first,
                last,
            })
        })
        .collect()
}

fn parse_rights(text: &str) -> Option<u32> {
    let mut bits = 0;
    for name in text.split(',') {
        bits |= rights::NAMES.iter().find(|(known, _)| *known == name)?.1;
    }
    Some(bits)
}

/// What a death of the service that restarts leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Start it again: the `n`th time within the window, 1 to [`RESTARTS_MAX`].
    Restart(usize),
    /// It has died once more than it may in the window: the node stops.
    Fatal,
}

/// The deaths of the service that restarts within the last [`WINDOW_NS`].
#[derive(Debug, Default)]
pub struct Deaths {
    /// When each death in the window was seen, oldest first.
    at: Vec<u64>,
}

impl Deaths {
    /// No deaths yet.
    pub fn new() -> Deaths {
        Deaths::default()
    }

    /// The service died at `now` (the monotonic clock, in nanoseconds). A death
    /// [`WINDOW_NS`] or more before `now` no longer counts.
    pub fn died(&mut self, now: u64) -> Verdict {
        self.at.retain(|at| now.saturating_sub(*at) < WINDOW_NS);
        self.at.push(now);
        if self.at.len() > RESTARTS_MAX {
            Verdict::Fatal
        } else {
            Verdict::Restart(self.at.len())
        }
    }
}

/// The console line of a restart: `moruna: service <service> restarted (<n> of 5)`.
pub fn restarted_line(service: &str, n: usize) -> String {
    format!("moruna: service {service} restarted ({n} of {RESTARTS_MAX})")
}

/// The fatal line of a death too many: `moruna: fatal service <service>: ended 6 times in 10
/// minutes` (work item 2.2c).
pub fn ended_line(service: &str) -> String {
    format!(
        "moruna: fatal service {service}: ended {} times in {} minutes",
        RESTARTS_MAX + 1,
        WINDOW_NS / 60_000_000_000
    )
}

/// A fatal line of the service that restarts, for what stops it being started at all.
pub fn fatal_line(service: &str, reason: &str) -> String {
    format!("moruna: fatal service {service}: {reason}")
}
