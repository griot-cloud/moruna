//! The network server's configuration (SDD 31, e.2 and i): the text a boot gives it as
//! its `config` slot.
//!
//! ```text
//! moruna-netstack 1
//! setting NAME VALUE                      # SDD 31 i; every setting has a default
//! interface NAME card=SERVICE [ipv4=ADDR/PREFIX] [ipv6=ADDR/PREFIX]... [gateway4=ADDR]
//!                [gateway6=ADDR] [dhcp]
//! dns ADDR...                             # the resolver's servers, before DHCP's
//! grant SLOT rights=R,... [remote=RULE,...] [local=RULE,...] [names=PATTERN[@PORTS],...]
//!            [sockets=N]
//! ```
//!
//! A rule is `ADDR/PREFIX` with an optional `@PORT` or `@FIRST-LAST` (every port
//! without), or `any` for every IPv4 and IPv6 address. A grant's `SLOT` names a
//! listener slot of the server's (`listen*`); every connection through it holds that
//! policy. `#` begins a comment. Anything else, and every value out of range, is
//! refused with its line.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::net::IpAddr;

use netstack_protocol::{Addr, Cidr, NameRule, Policy, Rule, rights};

/// The first line of the configuration.
pub const HEADER: &str = "moruna-netstack 1";
/// The most interfaces the server drives, loopback aside.
pub const INTERFACES_MAX: usize = 8;
/// The most addresses an interface is configured with (its IPv6 link-local address
/// is added to them), within smoltcp's `iface-max-addr-count-8`.
pub const ADDRS_MAX: usize = 6;
/// The most resolver servers, smoltcp's `dns-max-server-count-4`.
pub const DNS_MAX: usize = 4;

/// The tuning of the server (SDD 31, i): every buffer and bound that is not the
/// protocol's. The defaults are this table's; the configuration's `setting` lines
/// change them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Bytes of a TCP socket's receive buffer, which is its largest window.
    pub tcp_receive_buffer: usize,
    /// Bytes of a TCP socket's send buffer.
    pub tcp_send_buffer: usize,
    /// Datagrams and bytes a UDP socket holds received and not yet read.
    pub udp_receive_packets: usize,
    pub udp_receive_bytes: usize,
    /// Datagrams and bytes a UDP socket holds waiting to be sent.
    pub udp_send_packets: usize,
    pub udp_send_bytes: usize,
    /// Sockets the whole server holds, listeners' waiting ones included.
    pub sockets_max: usize,
    /// How long a connect waits to be established.
    pub connect_timeout_ms: u64,
    /// How long a ping waits for its echo.
    pub ping_timeout_ms: u64,
    /// How long a resolve waits for its answers.
    pub resolve_timeout_ms: u64,
    /// A TCP socket with nothing received for this long is reset; 0 is never.
    pub tcp_idle_timeout_ms: u64,
    /// Keep-alive probes on an idle TCP connection this often; 0 is none.
    pub tcp_keepalive_ms: u64,
    /// How long an acknowledgement may wait to ride on data; 0 acknowledges at once.
    pub tcp_ack_delay_ms: u64,
    /// Whether small segments wait for outstanding ones to be acknowledged (Nagle).
    pub tcp_nagle: bool,
    /// The congestion control: `reno` (RFC 5681), `cubic` (RFC 9438) or `none`.
    pub tcp_congestion: Congestion,
    /// How long a socket closed by its client, or left by a connection that went, may
    /// take to finish its close before it is reset.
    pub tcp_linger_ms: u64,
    /// Frames asked of a card at once, and the most frames received and not yet
    /// taken by the stack; more are dropped and counted.
    pub card_receive_frames: usize,
    pub card_queue_frames: usize,
    /// Sends to a card in flight at once, each of at most 64 frames.
    pub card_sends_in_flight: usize,
    /// The loopback interface's MTU.
    pub loopback_mtu: usize,
    /// Queries the resolver holds at once.
    pub dns_queries: usize,
    /// Maximum TLS socket states, failed states included until close.
    pub tls_sockets_max: usize,
    /// Per TLS socket buffer bound and total incoming handshake bytes.
    pub tls_buffer_bytes: usize,
    pub tls_handshake_timeout_ms: u64,
    /// Explicit trusted boot UTC anchor; zero disables TLS.
    pub tls_unix_seconds: u64,
    pub tls_time_valid_ms: u64,
}

/// A TCP congestion control smoltcp provides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Congestion {
    Reno,
    Cubic,
    None,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            tcp_receive_buffer: 256 << 10,
            tcp_send_buffer: 256 << 10,
            udp_receive_packets: 64,
            udp_receive_bytes: 256 << 10,
            udp_send_packets: 64,
            udp_send_bytes: 256 << 10,
            sockets_max: 1024,
            connect_timeout_ms: 30_000,
            ping_timeout_ms: 5_000,
            resolve_timeout_ms: 15_000,
            tcp_idle_timeout_ms: 0,
            tcp_keepalive_ms: 0,
            tcp_ack_delay_ms: 10,
            tcp_nagle: true,
            tcp_congestion: Congestion::Reno,
            tcp_linger_ms: 30_000,
            card_receive_frames: 256,
            card_queue_frames: 2048,
            card_sends_in_flight: 4,
            loopback_mtu: 65535,
            dns_queries: 16,
            tls_sockets_max: 16,
            tls_buffer_bytes: 65536,
            tls_handshake_timeout_ms: 30_000,
            tls_unix_seconds: 0,
            tls_time_valid_ms: 3_600_000,
        }
    }
}

/// One card's interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceConfig {
    pub name: String,
    /// The card's service in the device manager's directory.
    pub card: String,
    pub addrs: Vec<Cidr>,
    pub gateway4: Option<Addr>,
    pub gateway6: Option<Addr>,
    pub dhcp: bool,
}

/// What the configuration says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub settings: Settings,
    pub interfaces: Vec<InterfaceConfig>,
    pub dns: Vec<Addr>,
    /// Each listener slot's policy.
    pub grants: Vec<(String, Policy)>,
}

/// A configuration refused: the line (1-based) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub reason: String,
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

fn err<T>(line: usize, reason: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError {
        line,
        reason: reason.into(),
    })
}

fn parse_addr(s: &str) -> Option<Addr> {
    s.parse::<IpAddr>().ok().map(Addr)
}

/// `ADDR/PREFIX`, the prefix within the family's bits.
fn parse_cidr(s: &str) -> Option<Cidr> {
    let (a, p) = s.split_once('/')?;
    let addr = parse_addr(a)?;
    let prefix: u8 = p.parse().ok()?;
    (prefix <= addr.bits()).then_some(Cidr::new(addr, prefix))
}

/// `PORT` or `FIRST-LAST`.
fn parse_ports(s: &str) -> Option<(u16, u16)> {
    let (first, last) = match s.split_once('-') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => {
            let p = s.parse().ok()?;
            (p, p)
        }
    };
    (first <= last).then_some((first, last))
}

/// A rule list: `any`, or `ADDR/PREFIX[@PORTS]`, comma separated.
fn parse_rules(s: &str) -> Option<Vec<Rule>> {
    let mut rules = Vec::new();
    for item in s.split(',') {
        if item == "any" {
            rules.extend(Policy::everything(0).remote);
            continue;
        }
        let (net, ports) = match item.split_once('@') {
            Some((n, p)) => (n, Some(p)),
            None => (item, None),
        };
        let net = parse_cidr(net)?;
        let (first, last) = match ports {
            Some(p) => parse_ports(p)?,
            None => (0, u16::MAX),
        };
        rules.push(Rule { net, first, last });
    }
    Some(rules)
}

fn parse_names(s: &str) -> Option<Vec<NameRule>> {
    s.split(',')
        .map(|item| {
            let (pattern, ports) = match item.split_once('@') {
                Some((n, p)) => (n, Some(p)),
                None => (item, None),
            };
            let (first, last) = match ports {
                Some(p) => parse_ports(p)?,
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

fn parse_rights(s: &str) -> Option<u32> {
    let mut bits = 0;
    for name in s.split(',') {
        bits |= rights::NAMES.iter().find(|(n, _)| *n == name)?.1;
    }
    Some(bits)
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

fn number(
    line: usize,
    name: &str,
    value: &str,
    range: core::ops::RangeInclusive<u64>,
) -> Result<u64, ConfigError> {
    match value.parse::<u64>() {
        Ok(v) if range.contains(&v) => Ok(v),
        _ => err(
            line,
            format!(
                "setting {name}: `{value}` is not a number from {} to {}",
                range.start(),
                range.end()
            ),
        ),
    }
}

impl Settings {
    fn set(&mut self, line: usize, name: &str, value: &str) -> Result<(), ConfigError> {
        const MIB16: u64 = 16 << 20;
        let bytes = |v| number(line, name, v, 1024..=MIB16).map(|n| n as usize);
        let count = |v, most| number(line, name, v, 1..=most).map(|n| n as usize);
        let ms = |v| number(line, name, v, 0..=3_600_000);
        let positive_ms = |v| number(line, name, v, 1..=3_600_000);
        match name {
            "tcp-receive-buffer" => self.tcp_receive_buffer = bytes(value)?,
            "tcp-send-buffer" => self.tcp_send_buffer = bytes(value)?,
            "udp-receive-packets" => self.udp_receive_packets = count(value, 4096)?,
            "udp-receive-bytes" => self.udp_receive_bytes = bytes(value)?,
            "udp-send-packets" => self.udp_send_packets = count(value, 4096)?,
            "udp-send-bytes" => self.udp_send_bytes = bytes(value)?,
            "sockets-max" => self.sockets_max = count(value, 65536)?,
            "connect-timeout-ms" => self.connect_timeout_ms = positive_ms(value)?,
            "ping-timeout-ms" => self.ping_timeout_ms = positive_ms(value)?,
            "resolve-timeout-ms" => self.resolve_timeout_ms = positive_ms(value)?,
            "tcp-idle-timeout-ms" => self.tcp_idle_timeout_ms = ms(value)?,
            "tcp-keepalive-ms" => self.tcp_keepalive_ms = ms(value)?,
            "tcp-ack-delay-ms" => self.tcp_ack_delay_ms = number(line, name, value, 0..=500)?,
            "tcp-nagle" => {
                self.tcp_nagle = match value {
                    "on" => true,
                    "off" => false,
                    _ => return err(line, format!("setting {name}: `{value}` is not on or off")),
                }
            }
            "tcp-congestion" => {
                self.tcp_congestion = match value {
                    "reno" => Congestion::Reno,
                    "cubic" => Congestion::Cubic,
                    "none" => Congestion::None,
                    _ => {
                        return err(
                            line,
                            format!("setting {name}: `{value}` is not reno, cubic or none"),
                        );
                    }
                }
            }
            "tcp-linger-ms" => self.tcp_linger_ms = positive_ms(value)?,
            "card-receive-frames" => self.card_receive_frames = count(value, 1024)?,
            "card-queue-frames" => self.card_queue_frames = count(value, 65536)?,
            "card-sends-in-flight" => self.card_sends_in_flight = count(value, 32)?,
            "loopback-mtu" => self.loopback_mtu = number(line, name, value, 1280..=65535)? as usize,
            "dns-queries" => self.dns_queries = number(line, name, value, 2..=256)? as usize,
            "tls-sockets-max" => self.tls_sockets_max = count(value, 1024)?,
            "tls-buffer-bytes" => {
                self.tls_buffer_bytes = number(line, name, value, 16640..=1 << 20)? as usize
            }
            "tls-handshake-timeout-ms" => self.tls_handshake_timeout_ms = positive_ms(value)?,
            "tls-unix-seconds" => {
                self.tls_unix_seconds = number(line, name, value, 1..=253_402_300_799)?
            }
            "tls-time-valid-ms" => {
                self.tls_time_valid_ms = number(line, name, value, 1..=86_400_000)?
            }
            _ => return err(line, format!("no setting is called `{name}`")),
        }
        Ok(())
    }

    /// What no single setting says: a UDP socket must hold the largest datagram
    /// loopback carries, and a TCP buffer a whole receive's worth.
    fn check(&self) -> Result<(), String> {
        let largest = self.loopback_mtu - 28;
        if self.udp_receive_bytes < largest || self.udp_send_bytes < largest {
            return Err(format!(
                "a UDP buffer smaller than loopback's largest datagram ({largest} bytes for an MTU of {})",
                self.loopback_mtu
            ));
        }
        Ok(())
    }
}

/// Reads a configuration.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    match lines.next() {
        Some((_, HEADER)) => {}
        _ => return err(1, format!("the first line is not `{HEADER}`")),
    }
    let mut c = Config::default();
    for (n, line) in lines {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(directive) = words.next() else {
            continue;
        };
        match directive {
            "setting" => match (words.next(), words.next(), words.next()) {
                (Some(name), Some(value), None) => c.settings.set(n, name, value)?,
                _ => return err(n, "a setting is `setting NAME VALUE`"),
            },
            "interface" => {
                let Some(name) = words.next().filter(|s| valid_name(s)) else {
                    return err(n, "an interface needs a name of 1 to 32 [a-z0-9_-]");
                };
                if name == "lo" || c.interfaces.iter().any(|i| i.name == name) {
                    return err(
                        n,
                        format!("interface `{name}` is named twice (loopback is `lo`)"),
                    );
                }
                let mut iface = InterfaceConfig {
                    name: name.to_string(),
                    card: String::new(),
                    addrs: Vec::new(),
                    gateway4: None,
                    gateway6: None,
                    dhcp: false,
                };
                for w in words {
                    let (key, value) = w.split_once('=').unwrap_or((w, ""));
                    match key {
                        "card" if valid_name(value) => iface.card = value.to_string(),
                        "ipv4" | "ipv6" => match parse_cidr(value) {
                            Some(cidr)
                                if cidr.addr.is_v4() == (key == "ipv4") && cidr.addr.is_host() =>
                            {
                                iface.addrs.push(cidr)
                            }
                            _ => {
                                return err(
                                    n,
                                    format!("`{w}` is not an {key} host address and prefix"),
                                );
                            }
                        },
                        "gateway4" | "gateway6" => match parse_addr(value) {
                            Some(a) if a.is_v4() == (key == "gateway4") && a.is_host() => {
                                *if key == "gateway4" {
                                    &mut iface.gateway4
                                } else {
                                    &mut iface.gateway6
                                } = Some(a)
                            }
                            _ => return err(n, format!("`{w}` is not an address of its family")),
                        },
                        "dhcp" if value.is_empty() => iface.dhcp = true,
                        _ => return err(n, format!("`{w}` is not an interface's setting")),
                    }
                }
                if iface.card.is_empty() {
                    return err(n, format!("interface `{name}` names no card"));
                }
                if c.interfaces.iter().any(|i| i.card == iface.card) {
                    return err(n, format!("card `{}` serves two interfaces", iface.card));
                }
                if iface.dhcp
                    && (iface.addrs.iter().any(|a| a.addr.is_v4()) || iface.gateway4.is_some())
                {
                    return err(
                        n,
                        "an interface with dhcp takes its IPv4 address and gateway from DHCP",
                    );
                }
                if iface.addrs.iter().filter(|a| a.addr.is_v4()).count() > 1 {
                    return err(n, "an interface has at most one IPv4 address");
                }
                if iface.addrs.len() > ADDRS_MAX {
                    return err(n, format!("an interface has at most {ADDRS_MAX} addresses"));
                }
                for gw in [iface.gateway4, iface.gateway6].into_iter().flatten() {
                    if !iface.addrs.iter().any(|a| a.contains(&gw)) {
                        return err(
                            n,
                            format!("gateway {gw} is on none of the interface's networks"),
                        );
                    }
                }
                if c.interfaces.len() == INTERFACES_MAX {
                    return err(n, format!("at most {INTERFACES_MAX} interfaces"));
                }
                c.interfaces.push(iface);
            }
            "dns" => {
                for w in words {
                    match parse_addr(w) {
                        Some(a) if a.is_host() => c.dns.push(a),
                        _ => return err(n, format!("`{w}` is not a server's address")),
                    }
                }
                if c.dns.len() > DNS_MAX {
                    return err(n, format!("at most {DNS_MAX} resolver servers"));
                }
            }
            "grant" => {
                let Some(slot) = words
                    .next()
                    .filter(|s| s.starts_with("listen") && valid_name(s))
                else {
                    return err(
                        n,
                        "a grant names a listener slot, `listen` and up to 26 more of [a-z0-9_-]",
                    );
                };
                if c.grants.iter().any(|(s, _)| s == slot) {
                    return err(n, format!("slot `{slot}` is granted twice"));
                }
                let mut p = Policy {
                    sockets: 64,
                    ..Policy::default()
                };
                let mut had_rights = false;
                for w in words {
                    let (key, value) = w.split_once('=').unwrap_or((w, ""));
                    let parsed = match key {
                        "rights" => parse_rights(value).map(|r| {
                            p.rights = r;
                            had_rights = true;
                        }),
                        "remote" => parse_rules(value).map(|r| p.remote.extend(r)),
                        "local" => parse_rules(value).map(|r| p.local.extend(r)),
                        "names" => parse_names(value).map(|r| p.names.extend(r)),
                        "sockets" => value
                            .parse()
                            .ok()
                            .filter(|s| (1..=65536).contains(s))
                            .map(|s| p.sockets = s),
                        _ => None,
                    };
                    if parsed.is_none() {
                        return err(
                            n,
                            format!("`{w}` is not a grant's rights, rules, names or socket bound"),
                        );
                    }
                }
                if !had_rights {
                    return err(n, "a grant says its rights (`rights=`)");
                }
                if p.validate().is_err() {
                    return err(
                        n,
                        "the grant's policy is malformed (too many rules, or a name that is not one)",
                    );
                }
                c.grants.push((slot.to_string(), p));
            }
            other => {
                return err(
                    n,
                    format!("`{other}` is not setting, interface, dns or grant"),
                );
            }
        }
    }
    c.settings
        .check()
        .map_err(|reason| ConfigError { line: 0, reason })?;
    if c.grants.is_empty() {
        return err(0, "no grant: nothing could reach the server");
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const PAIR: &str = "moruna-netstack 1
# two cards on one link
setting tcp-receive-buffer 131072
setting tcp-nagle off
setting tcp-congestion cubic
interface a card=net2 ipv4=10.47.0.1/24 ipv6=fd4b:4532::1/64
interface b card=net3 ipv4=10.47.0.2/24 ipv6=fd4b:4532::2/64 gateway4=10.47.0.254
interface wan card=net4 dhcp
dns 10.47.0.2
grant listen-a rights=connect,listen,udp,ping remote=10.47.0.0/24,fd4b:4532::/64@1-65535 local=10.47.0.1/32 names=*.ke2.test@443 sockets=16
grant listen-any rights=connect,listen,udp,ping,resolve,grant remote=any local=any
";

    #[test]
    fn ke2_a_configuration_is_read() {
        let c = parse(PAIR).expect("parses");
        assert_eq!(c.settings.tcp_receive_buffer, 131072);
        assert!(!c.settings.tcp_nagle);
        assert_eq!(c.settings.tcp_congestion, Congestion::Cubic);
        assert_eq!(
            c.settings.tcp_send_buffer,
            Settings::default().tcp_send_buffer
        );
        assert_eq!(c.interfaces.len(), 3);
        assert_eq!(c.interfaces[1].gateway4, Some(Addr::v4(10, 47, 0, 254)));
        assert!(c.interfaces[2].dhcp && c.interfaces[2].addrs.is_empty());
        assert_eq!(c.dns, vec![Addr::v4(10, 47, 0, 2)]);
        let (slot, a) = &c.grants[0];
        assert_eq!(slot, "listen-a");
        assert_eq!(
            a.rights,
            rights::CONNECT | rights::LISTEN | rights::UDP | rights::PING
        );
        assert_eq!(a.remote.len(), 2);
        assert_eq!((a.remote[1].first, a.remote[1].last), (1, 65535));
        assert_eq!(a.local[0].net, Cidr::new(Addr::v4(10, 47, 0, 1), 32));
        assert_eq!(
            (a.names[0].pattern.as_str(), a.names[0].first),
            ("*.ke2.test", 443)
        );
        assert_eq!(a.sockets, 16);
        assert_eq!(c.grants[1].1.remote, Policy::everything(0).remote);
        assert_eq!(c.grants[1].1.sockets, 64);
    }

    #[test]
    fn ke2_every_mistake_is_refused_with_its_line() {
        let cases: &[(&str, usize, &str)] = &[
            ("moruna-netstack 2\n", 1, "first line"),
            (
                "moruna-netstack 1\nsetting tcp-receive-buffer 12\n",
                2,
                "1024",
            ),
            ("moruna-netstack 1\nsetting bogus 1\n", 2, "no setting"),
            ("moruna-netstack 1\nsetting dns-queries 1\n", 2, "2 to 256"),
            (
                "moruna-netstack 1\nsetting tls-buffer-bytes 16639\n",
                2,
                "16640",
            ),
            (
                "moruna-netstack 1\nsetting tls-time-valid-ms 86400001\n",
                2,
                "86400000",
            ),
            ("moruna-netstack 1\nsetting tls-unix-seconds 0\n", 2, "1 to"),
            (
                "moruna-netstack 1\nsetting tcp-nagle maybe\n",
                2,
                "on or off",
            ),
            (
                "moruna-netstack 1\nsetting udp-receive-bytes 2048\ngrant listen rights=connect\n",
                0,
                "UDP buffer",
            ),
            (
                "moruna-netstack 1\ninterface a ipv4=10.0.0.1/24\n",
                2,
                "names no card",
            ),
            ("moruna-netstack 1\ninterface lo card=net0\n", 2, "twice"),
            (
                "moruna-netstack 1\ninterface a card=net0\ninterface a card=net1\n",
                3,
                "twice",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0\ninterface b card=net0\n",
                3,
                "two interfaces",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 ipv4=10.0.0.1/33\n",
                2,
                "host address",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 ipv4=fd00::1/64\n",
                2,
                "host address",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 ipv4=0.0.0.0/8\n",
                2,
                "host address",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 ipv4=10.0.0.1/24 gateway4=10.1.0.1\n",
                2,
                "none of",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 dhcp ipv4=10.0.0.1/24\n",
                2,
                "dhcp",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 ipv4=10.0.0.1/24 ipv4=10.0.0.2/24\n",
                2,
                "one IPv4",
            ),
            (
                "moruna-netstack 1\ninterface a card=net0 mtu=9000\n",
                2,
                "interface's setting",
            ),
            (
                "moruna-netstack 1\ndns 10.0.0.1 nowhere\n",
                2,
                "server's address",
            ),
            (
                "moruna-netstack 1\ndns 1.1.1.1 1.0.0.1 8.8.8.8 8.8.4.4 9.9.9.9\n",
                2,
                "at most",
            ),
            (
                "moruna-netstack 1\ngrant server rights=connect\n",
                2,
                "listener slot",
            ),
            (
                "moruna-netstack 1\ngrant listen rights=connect\ngrant listen rights=udp\n",
                3,
                "twice",
            ),
            ("moruna-netstack 1\ngrant listen rights=fly\n", 2, "rights"),
            (
                "moruna-netstack 1\ngrant listen remote=any\n",
                2,
                "says its rights",
            ),
            (
                "moruna-netstack 1\ngrant listen rights=connect remote=10.0.0.0/8@9-8\n",
                2,
                "rules",
            ),
            (
                "moruna-netstack 1\ngrant listen rights=connect names=a..b\n",
                2,
                "malformed",
            ),
            (
                "moruna-netstack 1\ngrant listen rights=connect sockets=0\n",
                2,
                "socket bound",
            ),
            ("moruna-netstack 1\nroute 10.0.0.0/8\n", 2, "not setting"),
            ("moruna-netstack 1\n", 0, "no grant"),
        ];
        for (text, line, why) in cases {
            match parse(text) {
                Err(e) => {
                    assert_eq!(e.line, *line, "{text:?}: {e}");
                    assert!(e.reason.contains(why), "{text:?}: {e}");
                }
                Ok(c) => panic!("{text:?} parsed: {c:?}"),
            }
        }
    }
}
