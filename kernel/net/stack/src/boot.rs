//! The network server's part in a node's boot (system design 1B and 1C): the line its
//! first DHCP lease prints on the console, how long it waits for that lease before it
//! says so and ends, and the line it ends with when it cannot go on.
//!
//! All are the same whatever the server runs in. A configuration with no interface
//! set by DHCP has no lease to wait for, so it has no network line and no deadline.

use alloc::{format, string::String};
use core::fmt::Display;

use netstack_protocol::{Addr, Cidr};

/// How long after its start the server waits for its first DHCP lease: an address
/// within 30 s (system design 1B).
pub const LEASE_WITHIN_NS: u64 = 30_000_000_000;

/// What the server prints when that time has passed with no lease, before it exits
/// with failure.
pub const NO_LEASE: &str = "moruna: fatal network: no DHCP lease within 30 s";

/// The boot's network line:
/// `moruna: network wan <address>/<prefix> gateway <address|none>` for the interface
/// `wan`, `none` when the lease named no router.
pub fn network_line(interface: &str, address: Cidr, gateway: Option<Addr>) -> String {
    match gateway {
        Some(gateway) => format!("moruna: network {interface} {address} gateway {gateway}"),
        None => format!("moruna: network {interface} {address} gateway none"),
    }
}

/// The server's last line when it cannot go on, whether at its start or later
/// (system design 1B): `moruna: fatal network: <reason>`.
pub fn fatal(reason: impl Display) -> String {
    format!("moruna: fatal network: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_line_is_the_boot_s() {
        assert_eq!(
            network_line(
                "wan",
                Cidr::new(Addr::v4(10, 0, 2, 15), 24),
                Some(Addr::v4(10, 0, 2, 2))
            ),
            "moruna: network wan 10.0.2.15/24 gateway 10.0.2.2"
        );
        assert_eq!(
            network_line(
                "wan",
                Cidr::new(Addr::v4(192, 168, 7, 250), 20),
                Some(Addr::v4(192, 168, 0, 1))
            ),
            "moruna: network wan 192.168.7.250/20 gateway 192.168.0.1"
        );
    }

    #[test]
    fn a_lease_with_no_router_says_gateway_none() {
        assert_eq!(
            network_line("wan", Cidr::new(Addr::v4(10, 0, 2, 15), 24), None),
            "moruna: network wan 10.0.2.15/24 gateway none"
        );
    }

    #[test]
    fn the_deadline_is_thirty_seconds_and_its_line_is_the_fatal_line() {
        assert_eq!(LEASE_WITHIN_NS, 30 * 1_000_000_000);
        assert_eq!(NO_LEASE, "moruna: fatal network: no DHCP lease within 30 s");
        assert_eq!(fatal("no DHCP lease within 30 s"), NO_LEASE);
    }

    #[test]
    fn a_server_that_cannot_go_on_says_why() {
        assert_eq!(
            fatal(format_args!("looking up {}: {:?}", "net0", "NotFound")),
            "moruna: fatal network: looking up net0: \"NotFound\""
        );
        assert_eq!(
            fatal("no directory slot, and the configuration names cards"),
            "moruna: fatal network: no directory slot, and the configuration names cards"
        );
    }
}
