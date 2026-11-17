//! The audit record's ring: newest first, at most 10,000, compact as kept.

use super::*;
use alloc::{string::ToString, vec};

fn grant(key: &str, scope: &[Scope]) -> Grant {
    Grant {
        key: key.to_string(),
        scope: scope.to_vec(),
    }
}

#[test]
fn records_come_back_newest_first_as_2_1_writes_them() {
    let mut log = AuditLog::default();
    log.record(10, None, "health", 200);
    log.record(
        11,
        Some(&grant(
            "0123456789abcdef",
            &[Scope::Admin, Scope::Observe, Scope::Admin],
        )),
        "node_audit",
        200,
    );
    log.record(12, None, "node", 401);
    assert_eq!(
        log.audit(usize::MAX).records,
        vec![
            AuditRecord {
                time: 12,
                key: String::new(),
                scope: vec![],
                operation: "node".to_string(),
                status: 401,
            },
            AuditRecord {
                time: 11,
                key: "0123456789abcdef".to_string(),
                scope: vec![Scope::Observe, Scope::Admin],
                operation: "node_audit".to_string(),
                status: 200,
            },
            AuditRecord {
                time: 10,
                key: String::new(),
                scope: vec![],
                operation: "health".to_string(),
                status: 200,
            },
        ]
    );
}

#[test]
fn the_ring_holds_the_last_10_000() {
    let mut log = AuditLog::default();
    for time in 0..=AUDIT_LIMIT as u64 {
        log.record(time, None, "health", 200);
    }
    assert_eq!(log.len(), AUDIT_LIMIT);
    let records = log.audit(usize::MAX).records;
    assert_eq!(records.len(), AUDIT_LIMIT);
    assert_eq!(
        log.audit(AUDIT_ANSWER_LIMIT).records[..],
        records[..AUDIT_ANSWER_LIMIT]
    );
    assert_eq!(records[0].time, AUDIT_LIMIT as u64);
    assert_eq!(records[AUDIT_LIMIT - 1].time, 1, "the oldest went first");
}
