//! What a policy permits, and whether one policy covers another (SDD 31, f.6).
//!
//! The address rules: `remote` bounds where a connection reaches out to (a TCP
//! connect's endpoint, a datagram's destination, a ping's address, ports ignored for
//! a ping); `local` bounds what it takes on the server (a listener's or a UDP socket's
//! endpoint, and the source address of anything it sends, ports ignored for a source).
//! The name rules bound which names it may resolve (ports ignored) and connect to by
//! name (that port); a name reached by a name rule needs no address rule for the
//! addresses it resolves to, which is what lets an egress policy name a service whose
//! addresses move. `RESOLVE` resolves any name, and a connect to a name then needs the
//! address rules to permit the address it resolves to.
//!
//! One policy covers another when each right of the other is its own, it allows at
//! least as many sockets, and every rule of the other lies inside one rule of its own.
//! This is deliberately the simple test: a rule that only the union of two rules
//! covers is refused, which errs towards refusing a grant, never towards widening one.

use alloc::string::String;

use crate::{Addr, Endpoint, NAME_MAX, NameRule, Policy, RULES_MAX, Rule, net, rights};

/// `name` lowercased, without one trailing dot, if it is a host name: 1 to 253 bytes of
/// labels of 1 to 63 letters, digits, hyphens and underscores, no label starting or
/// ending with a hyphen.
pub fn normalise_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let lower = name.to_ascii_lowercase();
    name_valid(&lower).then_some(lower)
}

/// Whether `name` is a lowercase host name (see [`normalise_name`]).
pub fn name_valid(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_MAX {
        return false;
    }
    name.split('.').all(|label| {
        let b = label.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
            && b.iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
    })
}

/// Whether `pattern` is a name, or `*.` and a name.
fn pattern_valid(pattern: &str) -> bool {
    name_valid(pattern.strip_prefix("*.").unwrap_or(pattern))
}

/// Whether the lowercase name `name` matches `pattern`: the same name, or for
/// `*.domain` any name below `domain` (not `domain` itself).
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => {
            name.len() > domain.len() + 1 && name.ends_with(domain) && {
                let cut = name.len() - domain.len();
                name.as_bytes()[cut - 1] == b'.'
            }
        }
        None => pattern == name,
    }
}

/// Whether every name `child` matches, `parent` matches.
fn pattern_covers(parent: &str, child: &str) -> bool {
    match (parent.strip_prefix("*."), child.strip_prefix("*.")) {
        (_, None) => name_matches(parent, child),
        (Some(p), Some(c)) => c == p || name_matches(parent, c),
        (None, Some(_)) => false,
    }
}

impl NameRule {
    fn permits(&self, name: &str, port: Option<u16>) -> bool {
        name_matches(&self.pattern, name)
            && port.is_none_or(|p| (self.first..=self.last).contains(&p))
    }

    fn covers(&self, other: &NameRule) -> bool {
        pattern_covers(&self.pattern, &other.pattern)
            && self.first <= other.first
            && other.last <= self.last
    }
}

impl Policy {
    /// A policy with every right over every address, name and port, holding up to
    /// `sockets` sockets: what the configuration's `any` means.
    pub fn everything(sockets: u32) -> Policy {
        use crate::Cidr;
        let any4 = Rule::any_port(Cidr::new(Addr::v4(0, 0, 0, 0), 0));
        let any6 = Rule::any_port(Cidr::new(
            Addr(core::net::IpAddr::V6(core::net::Ipv6Addr::UNSPECIFIED)),
            0,
        ));
        Policy {
            rights: rights::ALL,
            remote: alloc::vec![any4, any6],
            local: alloc::vec![any4, any6],
            names: alloc::vec![],
            sockets,
        }
    }

    /// `BadPolicy` unless every right is known, every rule's port range is in order,
    /// every pattern is a name and no list is longer than [`RULES_MAX`]. (A prefix
    /// past its family's bits does not decode.)
    pub fn validate(&self) -> Result<(), net::Error> {
        let rules_ok = |rules: &[Rule]| {
            rules.len() <= RULES_MAX
                && rules
                    .iter()
                    .all(|r| r.first <= r.last && r.net.prefix <= r.net.addr.bits())
        };
        let names_ok = self.names.len() <= RULES_MAX
            && self.names.iter().all(|n| {
                n.first <= n.last && n.pattern.len() <= NAME_MAX && pattern_valid(&n.pattern)
            });
        if self.rights & !rights::ALL != 0
            || !rules_ok(&self.remote)
            || !rules_ok(&self.local)
            || !names_ok
        {
            return Err(net::Error::BadPolicy);
        }
        Ok(())
    }

    pub fn has(&self, right: u32) -> bool {
        self.rights & right == right
    }

    fn remote_permits(&self, to: &Endpoint) -> bool {
        self.remote.iter().any(|r| r.permits(to))
    }

    /// Whether a TCP connect to `to` is permitted.
    pub fn may_connect(&self, to: &Endpoint) -> bool {
        self.has(rights::CONNECT) && self.remote_permits(to)
    }

    /// Whether a TCP connect to `name` at `port` is permitted by a name rule.
    pub fn may_connect_name(&self, name: &str, port: u16) -> bool {
        self.has(rights::CONNECT) && self.names.iter().any(|n| n.permits(name, Some(port)))
    }

    /// Whether a datagram to `to` is permitted.
    pub fn may_send_to(&self, to: &Endpoint) -> bool {
        self.has(rights::UDP) && self.remote_permits(to)
    }

    /// Whether an echo request to `to` is permitted.
    pub fn may_ping(&self, to: &Addr) -> bool {
        self.has(rights::PING) && self.remote.iter().any(|r| r.net.contains(to))
    }

    /// Whether `name` may be resolved.
    pub fn may_resolve(&self, name: &str) -> bool {
        self.has(rights::RESOLVE) || self.names.iter().any(|n| n.permits(name, None))
    }

    /// Whether a TCP listener on `at` is permitted.
    pub fn may_listen(&self, at: &Endpoint) -> bool {
        self.has(rights::LISTEN) && self.local.iter().any(|r| r.permits(at))
    }

    /// Whether a UDP socket bound to `at` is permitted.
    pub fn may_bind(&self, at: &Endpoint) -> bool {
        self.has(rights::UDP) && self.local.iter().any(|r| r.permits(at))
    }

    /// Whether anything may leave from the local address `addr`.
    pub fn may_use_source(&self, addr: &Addr) -> bool {
        self.local.iter().any(|r| r.net.contains(addr))
    }

    /// The local rules that hold `addr`: where a port for a listener or a UDP socket
    /// asking for port 0 may be chosen.
    pub fn local_rules_for<'a>(&'a self, addr: &'a Addr) -> impl Iterator<Item = &'a Rule> + 'a {
        self.local.iter().filter(move |r| r.net.contains(addr))
    }

    /// Whether everything `child` permits, this policy permits.
    pub fn covers(&self, child: &Policy) -> bool {
        child.rights & !self.rights == 0
            && child.sockets <= self.sockets
            && child
                .remote
                .iter()
                .all(|c| self.remote.iter().any(|p| p.covers(c)))
            && child
                .local
                .iter()
                .all(|c| self.local.iter().any(|p| p.covers(c)))
            && child
                .names
                .iter()
                .all(|c| self.names.iter().any(|p| p.covers(c)))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use super::*;
    use crate::Cidr;

    fn net4(a: [u8; 4], prefix: u8) -> Cidr {
        Cidr::new(Addr::v4(a[0], a[1], a[2], a[3]), prefix)
    }

    fn ep4(a: [u8; 4], port: u16) -> Endpoint {
        Endpoint::new(Addr::v4(a[0], a[1], a[2], a[3]), port)
    }

    fn v6(s: &str) -> Addr {
        Addr(s.parse().expect("address"))
    }

    fn connect_only(net: Cidr, first: u16, last: u16) -> Policy {
        Policy {
            rights: rights::CONNECT,
            remote: vec![Rule { net, first, last }],
            local: vec![Rule::any_port(net4([0, 0, 0, 0], 0))],
            names: vec![],
            sockets: 4,
        }
    }

    #[test]
    fn ke2_networks_hold_their_addresses_and_nothing_else() {
        let n = net4([10, 47, 0, 0], 24);
        assert!(n.contains(&Addr::v4(10, 47, 0, 1)));
        assert!(n.contains(&Addr::v4(10, 47, 0, 255)));
        assert!(!n.contains(&Addr::v4(10, 47, 1, 1)));
        assert!(!n.contains(&v6("::ffff:10.47.0.1")));
        assert!(net4([0, 0, 0, 0], 0).contains(&Addr::v4(255, 255, 255, 255)));
        assert!(net4([10, 47, 0, 9], 32).contains(&Addr::v4(10, 47, 0, 9)));
        assert!(!net4([10, 47, 0, 9], 32).contains(&Addr::v4(10, 47, 0, 8)));
        let six = Cidr::new(v6("fd4b:4532::"), 64);
        assert!(six.contains(&v6("fd4b:4532::2")));
        assert!(six.contains(&v6("fd4b:4532:0:0:ffff::1")));
        assert!(!six.contains(&v6("fd4b:4532:0:1::1")));
        assert!(Cidr::new(v6("::"), 0).contains(&v6("2001:db8::1")));
        assert!(Cidr::new(v6("2001:db8::1"), 128).contains(&v6("2001:db8::1")));
        assert!(!Cidr::new(v6("2001:db8::1"), 128).contains(&v6("2001:db8::2")));
        assert!(n.covers(&net4([10, 47, 0, 128], 25)));
        assert!(!n.covers(&net4([10, 47, 0, 0], 23)));
        assert!(!n.covers(&net4([10, 48, 0, 0], 24)));
    }

    #[test]
    fn ke2_a_connect_only_policy_reaches_its_ports_only() {
        let p = connect_only(net4([10, 47, 0, 2], 32), 443, 443);
        assert!(p.validate().is_ok());
        assert!(p.may_connect(&ep4([10, 47, 0, 2], 443)));
        assert!(!p.may_connect(&ep4([10, 47, 0, 2], 444)));
        assert!(!p.may_connect(&ep4([10, 47, 0, 3], 443)));
        assert!(!p.may_send_to(&ep4([10, 47, 0, 2], 443)));
        assert!(!p.may_ping(&Addr::v4(10, 47, 0, 2)));
        assert!(!p.may_listen(&ep4([10, 47, 0, 1], 443)));
        assert!(!p.may_bind(&ep4([10, 47, 0, 1], 443)));
        assert!(!p.may_resolve("tls.ke2.test"));
        assert!(p.may_use_source(&Addr::v4(10, 47, 0, 1)));
    }

    #[test]
    fn ke2_names_match_exactly_or_below_a_domain() {
        assert!(name_matches("api.example.com", "api.example.com"));
        assert!(!name_matches("api.example.com", "x.api.example.com"));
        assert!(name_matches("*.example.com", "api.example.com"));
        assert!(name_matches("*.example.com", "a.b.example.com"));
        assert!(!name_matches("*.example.com", "example.com"));
        assert!(!name_matches("*.example.com", "badexample.com"));
        assert!(!name_matches("*.example.com", "xexample.com"));
        assert_eq!(
            normalise_name("API.Example.COM."),
            Some("api.example.com".to_string())
        );
        assert_eq!(
            normalise_name("metadata.google.internal"),
            Some("metadata.google.internal".to_string())
        );
        for bad in ["", ".", "a..b", "-a.b", "a-.b", "a b", "a/b", "*.a"] {
            assert_eq!(normalise_name(bad), None, "{bad:?}");
        }
        assert!(!name_valid(&"a".repeat(64)));
        assert!(name_valid(&"a".repeat(63)));
        let long = [&"a".repeat(63)[..]; 4].join(".");
        assert_eq!(long.len(), 255);
        assert!(!name_valid(&long));
    }

    #[test]
    fn ke2_name_rules_permit_resolving_and_connecting_by_name() {
        let p = Policy {
            rights: rights::CONNECT,
            remote: vec![],
            local: vec![Rule::any_port(net4([0, 0, 0, 0], 0))],
            names: vec![NameRule {
                pattern: "*.googleapis.com".to_string(),
                first: 443,
                last: 443,
            }],
            sockets: 8,
        };
        assert!(p.validate().is_ok());
        assert!(p.may_resolve("storage.googleapis.com"));
        assert!(!p.may_resolve("example.com"));
        assert!(p.may_connect_name("storage.googleapis.com", 443));
        assert!(!p.may_connect_name("storage.googleapis.com", 80));
        assert!(!p.may_connect(&ep4([142, 250, 0, 1], 443)));
        let mut any = p.clone();
        any.rights |= rights::RESOLVE;
        assert!(any.may_resolve("example.com"));
        assert!(!any.may_connect_name("example.com", 443));
    }

    #[test]
    fn ke2_malformed_policies_are_refused() {
        let good = connect_only(net4([10, 0, 0, 0], 8), 1, 2);
        let mut bad = good.clone();
        bad.rights |= 1 << 20;
        assert_eq!(bad.validate(), Err(net::Error::BadPolicy));
        let mut bad = good.clone();
        bad.remote[0].first = 3;
        assert_eq!(bad.validate(), Err(net::Error::BadPolicy));
        let mut bad = good.clone();
        bad.remote = vec![good.remote[0]; RULES_MAX + 1];
        assert_eq!(bad.validate(), Err(net::Error::BadPolicy));
        for pattern in ["", "*.", "*", "a..b", "**.a", "a.*.b"] {
            let mut bad = good.clone();
            bad.names = vec![NameRule {
                pattern: pattern.to_string(),
                first: 0,
                last: 1,
            }];
            assert_eq!(bad.validate(), Err(net::Error::BadPolicy), "{pattern:?}");
        }
        assert!(Policy::everything(16).validate().is_ok());
    }

    #[test]
    fn ke2_a_policy_covers_only_what_it_permits_itself() {
        let parent = Policy {
            rights: rights::CONNECT | rights::GRANT | rights::UDP,
            remote: vec![Rule {
                net: net4([10, 47, 0, 0], 24),
                first: 1000,
                last: 2000,
            }],
            local: vec![Rule::any_port(net4([10, 47, 0, 1], 32))],
            names: vec![NameRule {
                pattern: "*.ke2.test".to_string(),
                first: 0,
                last: 65535,
            }],
            sockets: 8,
        };
        let narrow = Policy {
            rights: rights::CONNECT,
            remote: vec![Rule {
                net: net4([10, 47, 0, 2], 32),
                first: 1500,
                last: 1500,
            }],
            local: vec![Rule {
                net: net4([10, 47, 0, 1], 32),
                first: 0,
                last: 0,
            }],
            names: vec![
                NameRule {
                    pattern: "tls.ke2.test".to_string(),
                    first: 443,
                    last: 443,
                },
                NameRule {
                    pattern: "*.a.ke2.test".to_string(),
                    first: 1,
                    last: 2,
                },
            ],
            sockets: 8,
        };
        assert!(parent.covers(&narrow));
        assert!(parent.covers(&parent));
        assert!(!narrow.covers(&parent));
        let widen = |f: &dyn Fn(&mut Policy)| {
            let mut p = narrow.clone();
            f(&mut p);
            parent.covers(&p)
        };
        assert!(!widen(&|p| p.rights |= rights::LISTEN));
        assert!(!widen(&|p| p.sockets = 9));
        assert!(!widen(&|p| p.remote[0].net = net4([10, 47, 0, 0], 23)));
        assert!(!widen(&|p| p.remote[0].last = 2001));
        assert!(!widen(&|p| p.local[0].net = net4([10, 47, 0, 2], 32)));
        assert!(!widen(&|p| p.names[0].pattern = "ke2.test".to_string()));
        assert!(!widen(&|p| p.names[1].pattern = "*.test".to_string()));
        assert!(!widen(&|p| p
            .remote
            .push(Rule::any_port(Cidr::new(v6("::"), 0)))));
        assert!(widen(&|p| p.names[1].pattern = "*.ke2.test".to_string()));
        // An exact name never covers a domain below it.
        let exact = Policy {
            names: vec![NameRule {
                pattern: "a.test".to_string(),
                first: 0,
                last: 65535,
            }],
            ..Policy::default()
        };
        let below = Policy {
            names: vec![NameRule {
                pattern: "*.a.test".to_string(),
                first: 0,
                last: 65535,
            }],
            ..Policy::default()
        };
        assert!(!exact.covers(&below));
    }
}
