//! Fetching a function's image over HTTPS on a node (system design 3D, 3E): `node-admission`
//! mints itself a channel limited to the address's host on port 443
//! ([`moruna_node::egress::mint`], from its own client, which holds `grant,connect` on every
//! name at 443), connects through it by name with the network server's TLS, and reads the image
//! with the node's HTTP client. No credentials: an address is `https://` only, a pre-signed or
//! SAS URL for a private store.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use moruna_api::address_shown;
use moruna_node::egress;
use moruna_node_http::{HttpError, Request, exchange_within, net::NetStream};
use moruna_svc::{Account, deadline_in};
use netstack_protocol::{Target, net};

use crate::{Fetch, FetchError};

/// The port an image is fetched on: the one a minted channel reaches.
pub const PORT: u16 = egress::PORT;

/// The bytes an answer's head may take besides its image.
pub const HEAD_MAX: usize = 64 * 1024;

/// An `https://` address's host, by name and lowercased, and its path with its query; at port
/// 443 only, the one a job's or a fetch's channel reaches. Refused, in words: another scheme,
/// credentials before the host, another port, no host, or an address for a host.
pub fn target(address: &str) -> Result<(String, String), &'static str> {
    let rest = address
        .strip_prefix("https://")
        .ok_or("the address is not https://")?;
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err("the address carries credentials before its host");
    }
    if authority.starts_with('[') {
        return Err("the address names an address, not a host");
    }
    let host = match authority.split_once(':') {
        Some((host, "443")) => host,
        Some(_) => return Err("the address names a port other than 443"),
        None => authority,
    };
    if host.is_empty() {
        return Err("the address names no host");
    }
    // A fragment is the client's own, never sent.
    let path = path.split('#').next().unwrap_or_default();
    let path = match path {
        "" => String::from("/"),
        query if query.starts_with('?') => format!("/{query}"),
        path => path.to_string(),
    };
    Ok((host.to_ascii_lowercase(), path))
}

/// Function images fetched through `node-admission`'s own network client.
pub struct HttpsFetch<'a> {
    /// The client that mints each fetch's channel.
    admission: &'a mut net::Client,
    /// The account each fetch's client is bound to, as `moruna.net` asks.
    account: &'a Account,
    /// How long one fetch may take, in nanoseconds.
    timeout: u64,
}

impl<'a> HttpsFetch<'a> {
    /// Fetches through `admission`, each fetch's client bound to `account`, each within
    /// `timeout` nanoseconds.
    pub fn new(admission: &'a mut net::Client, account: &'a Account, timeout: u64) -> Self {
        HttpsFetch {
            admission,
            account,
            timeout,
        }
    }
}

impl Fetch for HttpsFetch<'_> {
    fn fetch(&mut self, address: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
        // The failure names the address as the node shows it: its query is a credential (3E),
        // and the failure reaches the job's events, the console and the logs.
        let failed = |why: String| FetchError::Failed(format!("{}: {why}", address_shown(address)));
        let (host, path) = target(address).map_err(|why| failed(why.into()))?;
        let deadline = deadline_in(self.timeout);
        // One connection, to the address's host at 443, and nothing else.
        let channel = egress::mint(self.admission, core::slice::from_ref(&host), 1)
            .map_err(|error| failed(format!("no network to its host: {error:?}")))?;
        let account = self
            .account
            .0
            .info()
            .and_then(|(_, rights)| self.account.0.duplicate(rights))
            .map(Account)
            .map_err(|error| failed(format!("no account to bind: {}", error.name())))?;
        let connection =
            moruna_svc::connect(&channel).map_err(|error| failed(format!("{error:?}")))?;
        let mut client = net::Client::connect(connection, deadline, account)
            .map_err(|error| failed(format!("the network server: {error:?}")))?;
        let mut stream = NetStream::open(
            &mut client,
            Target::Name(host.clone(), PORT),
            Some(&host),
            deadline,
        )
        .map_err(|error| failed(error.to_string()))?;
        let most = usize::try_from(limit).unwrap_or(usize::MAX);
        let response = exchange_within(
            &mut stream,
            &Request::get(&host, &path, &[]),
            deadline,
            most.saturating_add(HEAD_MAX),
        )
        .map_err(|error| match error {
            HttpError::TooLarge(_) => FetchError::TooLarge,
            error => failed(error.to_string()),
        })?;
        if response.status != 200 {
            return Err(failed(format!("answered {}", response.status)));
        }
        if response.body.len() > most {
            return Err(FetchError::TooLarge);
        }
        Ok(response.body)
    }
}
