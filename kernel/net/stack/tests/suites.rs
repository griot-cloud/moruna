//! KN-T6 and KN-T7 (SDD 31, k): `moruna.net`'s suites pass against the server's stack
//! on a host, over its loopback and over two cards on one link; and fail where the
//! server cannot meet them: a grant that does not reach loopback, a link that carries
//! nothing.

mod common;

use common::{Flaws, MAC_A, MAC_B, PAIR, link, serve};
use moruna_svc::conformance::{self, Listener, Summary};

fn failures(checks: &[(String, Result<(), String>)]) -> Vec<String> {
    checks
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(n, r)| format!("{n}: {r:?}"))
        .collect()
}

fn run_one_checks(listener: moruna_svc::Channel) -> (Summary, Vec<(String, Result<(), String>)>) {
    let mut checks = Vec::new();
    let summary = conformance::run(
        &netstack_protocol::suite(),
        &mut Listener(listener),
        &mut |c| checks.push((c.name.to_string(), c.result.clone())),
    );
    (summary, checks)
}

fn run_one(listener: moruna_svc::Channel) -> (Summary, Vec<String>) {
    let (summary, checks) = run_one_checks(listener);
    (summary, failures(&checks))
}

fn run_pair(
    a: moruna_svc::Channel,
    b: moruna_svc::Channel,
    timeout: Option<u64>,
) -> (Summary, Vec<String>) {
    let mut suite = netstack_protocol::pair_suite();
    if let Some(t) = timeout {
        suite.timeout = t;
    }
    let mut checks = Vec::new();
    let summary = conformance::run_pair(&suite, &mut Listener(a), &mut Listener(b), &mut |c| {
        checks.push((c.name.to_string(), c.result.clone()))
    });
    (summary, failures(&checks))
}

#[test]
fn kn_t6_the_suite_passes_over_loopback() {
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], link(Flaws::default()));
    let (summary, failed) = run_one(slots.remove("listen-suite").expect("slot"));
    assert!(failed.is_empty(), "{failed:#?}");
    assert!(summary.ok());
    // The shared checks, and every case of the suite's own.
    assert!(summary.passed >= 20, "{summary:?}");
    drop(slots);
    join.join().expect("the server");
}

#[test]
fn kn_t7_the_pair_suite_passes_over_two_cards_on_one_link() {
    let (mut slots, join) = serve(PAIR, &[MAC_A, MAC_B], link(Flaws::default()));
    let a = slots.remove("listen-a").expect("slot");
    let b = slots.remove("listen-b").expect("slot");
    let (summary, failed) = run_pair(a, b, None);
    assert!(failed.is_empty(), "{failed:#?}");
    assert_eq!(summary.passed, 6);
    drop(slots);
    join.join().expect("the server");
}

#[test]
fn kn_t7_the_suites_fail_where_the_server_cannot_meet_them() {
    // A grant that does not reach loopback fails every loopback-dependent case.
    // Argument validation still succeeds: it needs no socket or network grant.
    let narrow = PAIR.replace(
        "grant listen-suite rights=connect,listen,udp,ping,grant remote=127.0.0.0/8,::1/128 local=127.0.0.0/8,::1/128",
        "grant listen-suite rights=connect,listen,udp,ping remote=10.47.0.0/24 local=10.47.0.1/32",
    );
    let (mut slots, join) = serve(&narrow, &[MAC_A, MAC_B], link(Flaws::default()));
    let (summary, checks) = run_one_checks(slots.remove("listen-suite").expect("slot"));
    let failed = failures(&checks);
    for case in netstack_protocol::suite().cases {
        let result = &checks
            .iter()
            .find(|(name, _)| name == case.name)
            .expect("every declared suite case must execute")
            .1;
        if case.name == "tls: ordinal 16 refuses an invalid verification name" {
            assert!(
                result.is_ok(),
                "argument validation must succeed without a loopback grant: {result:?}"
            );
        } else {
            assert!(
                result.is_err(),
                "loopback-dependent case unexpectedly passed: {}",
                case.name
            );
        }
    }
    assert!(
        failed
            .iter()
            .all(|f| f.contains("does not reach") || f.contains("cannot grant")),
        "{failed:#?}"
    );
    assert!(!summary.ok());
    drop(slots);
    join.join().expect("the server");

    // A link that carries nothing fails every pair case.
    let (mut slots, join) = serve(
        PAIR,
        &[MAC_A, MAC_B],
        link(Flaws {
            cut: true,
            ..Flaws::default()
        }),
    );
    let a = slots.remove("listen-a").expect("slot");
    let b = slots.remove("listen-b").expect("slot");
    let (summary, failed) = run_pair(a, b, Some(5_000_000_000));
    assert_eq!(summary.passed, 0, "{failed:#?}");
    assert_eq!(failed.len(), 6);
    drop(slots);
    join.join().expect("the server");
}
