//! A job's network (system design 3D): the policy of the engine's channel to the
//! network server, and its minting. Nothing else is reachable from a job.
//!
//! The policy holds exactly what a job's object stores need: the `connect` right, and
//! one name rule per host name in the job's object-store settings, at port 443. It has
//! no `resolve` right (which would resolve any name), no remote address rules (so no
//! address is reachable but by one of its names), no listening, no UDP, no ping and no
//! `grant`. Its local rules hold every address: the network server sends only from an
//! address a policy's local rules hold, the node's own address comes from DHCP, and
//! with neither `listen` nor `udp` a local rule permits nothing else. The network server
//! checks every request against the policy before doing anything.
//!
//! `node-admission` mints it per job with `grant` (`moruna.net`'s request 14) on its own
//! network client, which holds `grant,connect` on every name at 443 (the rule `*`,
//! `moruna.net` 3.2) and never connects itself. The network server refuses a grant its
//! client's policy does not cover. A job's hosts never include `*`: it is no host name.

use alloc::{string::String, vec::Vec};
use core::net::{IpAddr, Ipv6Addr};

use moruna_svc::{CallError, Channel};
use netstack_protocol::{
    Addr, Cidr, NameRule, Policy, RULES_MAX, Rule, net, normalise_name, rights,
};

/// The one port a job reaches: HTTPS.
pub const PORT: u16 = 443;

/// Why a job's hosts make no policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressError {
    /// No host: a job that names no object store gets no network channel at all.
    NoHosts,
    /// This is not a host name: an address, or no name the network server takes.
    NotAName(String),
    /// More distinct hosts than one policy holds (`RULES_MAX`).
    TooMany(usize),
    /// A policy that may hold no socket.
    NoSockets,
}

/// The policy of a job's channel: `connect` to each of `hosts` (lowercased, a trailing
/// dot removed, each once) at port 443, holding at most `sockets` sockets at a time.
pub fn policy(hosts: &[String], sockets: u32) -> Result<Policy, EgressError> {
    if sockets == 0 {
        return Err(EgressError::NoSockets);
    }
    let mut names: Vec<String> = Vec::new();
    for host in hosts {
        // A name only: an address literal parses as an IP address, never as a name.
        let name = normalise_name(host)
            .filter(|n| n.parse::<core::net::IpAddr>().is_err())
            .ok_or_else(|| EgressError::NotAName(host.clone()))?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Err(EgressError::NoHosts);
    }
    if names.len() > RULES_MAX {
        return Err(EgressError::TooMany(names.len()));
    }
    let any4 = Rule::any_port(Cidr::new(Addr::v4(0, 0, 0, 0), 0));
    let any6 = Rule::any_port(Cidr::new(Addr(IpAddr::V6(Ipv6Addr::UNSPECIFIED)), 0));
    Ok(Policy {
        rights: rights::CONNECT,
        remote: Vec::new(),
        local: alloc::vec![any4, any6],
        names: names
            .into_iter()
            .map(|pattern| NameRule {
                pattern,
                first: PORT,
                last: PORT,
            })
            .collect(),
        sockets,
    })
}

/// Why a job's channel was not minted.
#[derive(Debug, PartialEq)]
pub enum MintError {
    /// The hosts make no policy.
    Policy(EgressError),
    /// The network server refused the grant: `NotPermitted` when the minting client's
    /// own policy does not cover the job's.
    Refused(CallError<net::Error>),
}

/// Mints a job's channel to the network server on `client`: a listener whose every
/// connection holds [`policy`]`(hosts, sockets)`. The engine connects through it.
pub fn mint(
    client: &mut net::Client,
    hosts: &[String],
    sockets: u32,
) -> Result<Channel, MintError> {
    let policy = policy(hosts, sockets).map_err(MintError::Policy)?;
    client.grant(policy).map_err(MintError::Refused)
}
