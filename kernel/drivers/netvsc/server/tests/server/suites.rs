//! `net-protocol`'s conformance suites (SDD 29, k; NT-T7 and NT-T8 as `kernel/drivers/net-test`
//! runs them), on the host: the suite for one card against the driver, and the suite for
//! two cards on one link against two drivers whose fake hosts are joined.

use std::thread;

use moruna_svc::conformance::{self, Summary};

use crate::{World, fake::ROOMY};

/// The checks that failed, which must be as many as the summary counts.
fn failures(summary: &Summary, checks: &[(String, Result<(), String>)]) -> Vec<String> {
    let failed: Vec<String> = checks
        .iter()
        .filter(|(_, result)| result.is_err())
        .map(|(name, result)| format!("{name}: {result:?}"))
        .collect();
    assert_eq!(failed.len(), summary.failed as usize);
    failed
}

#[test]
fn the_suite_for_one_card_passes_against_the_driver() {
    let (mut world, _) = World::new(ROOMY, |_| ());
    let listener = world.listener(0);
    let (summary, checks) = world.serve(thread::spawn(move || {
        let mut checks = Vec::new();
        let summary = conformance::run(
            &net_protocol::suite(),
            &mut conformance::Listener(listener),
            &mut |c| checks.push((c.name.to_string(), c.result.clone())),
        );
        (summary, checks)
    }));
    assert!(summary.ok(), "{:?}", failures(&summary, &checks));
    assert!(summary.passed >= 16, "{summary:?}");
    assert!(world.driver(0).borrow().failed().is_none());
}

#[test]
fn the_suite_for_two_cards_on_one_link_passes_against_two_drivers() {
    let (mut world, _) = World::pair(ROOMY);
    let (a, b) = (world.listener(0), world.listener(1));
    let (summary, checks) = world.serve(thread::spawn(move || {
        let mut checks = Vec::new();
        let summary = conformance::run_pair(
            &net_protocol::pair_suite(),
            &mut conformance::Listener(a),
            &mut conformance::Listener(b),
            &mut |c| checks.push((c.name.to_string(), c.result.clone())),
        );
        (summary, checks)
    }));
    assert!(summary.ok(), "{:?}", failures(&summary, &checks));
    assert_eq!(summary.passed, 3);
    for card in 0..2 {
        assert!(world.driver(card).borrow().failed().is_none());
    }
}
