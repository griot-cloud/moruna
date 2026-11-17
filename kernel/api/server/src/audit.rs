//! The audit record (system design, section 2C): one record per call, the last 10,000 kept in
//! memory, newest first in `node/audit`. Nothing is written to disk or to the console.

use crate::token::Grant;
use alloc::{collections::VecDeque, string::String, vec::Vec};
use moruna_api::{Audit, AuditRecord, Scope};

/// How many records are kept: the last 10,000 calls.
pub const AUDIT_LIMIT: usize = 10_000;
/// How many records `node/audit` answers: the newest 1,000.
pub const AUDIT_ANSWER_LIMIT: usize = 1_000;

/// The scopes in the order a record lists them.
const SCOPES: [Scope; 3] = [Scope::Observe, Scope::Submit, Scope::Admin];

/// One call, as kept: a few dozen bytes, so that 10,000 of them stay small.
#[derive(Clone, Copy, Debug)]
struct Entry {
    time: u64,
    /// The ID of the key that verified the call's token, when one did.
    key: Option<[u8; 16]>,
    /// The token's scope, one bit per scope in [`SCOPES`]' order.
    scope: u8,
    operation: &'static str,
    status: u16,
}

/// The last [`AUDIT_LIMIT`] calls, newest first.
#[derive(Debug, Default)]
pub(crate) struct AuditLog {
    entries: VecDeque<Entry>,
}

impl AuditLog {
    /// Records a call of `operation` at `time` that was answered `status`, made with the token
    /// `grant` granted, if one did.
    pub(crate) fn record(
        &mut self,
        time: u64,
        grant: Option<&Grant>,
        operation: &'static str,
        status: u16,
    ) {
        let key = grant.and_then(|grant| grant.key.as_bytes().try_into().ok());
        let scope = grant.map_or(0, |grant| {
            SCOPES
                .iter()
                .enumerate()
                .filter(|(_, scope)| grant.scope.contains(scope))
                .fold(0, |bits, (index, _)| bits | 1 << index)
        });
        if self.entries.len() == AUDIT_LIMIT {
            self.entries.pop_back();
        }
        self.entries.push_front(Entry {
            time,
            key,
            scope,
            operation,
            status,
        });
    }

    /// The newest `count` records, newest first, as `node/audit` answers them.
    pub(crate) fn audit(&self, count: usize) -> Audit {
        Audit {
            records: self
                .entries
                .iter()
                .take(count)
                .map(|entry| AuditRecord {
                    time: entry.time,
                    key: entry
                        .key
                        .map(|key| key.iter().map(|b| char::from(*b)).collect())
                        .unwrap_or_else(String::new),
                    scope: SCOPES
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| entry.scope & 1 << index != 0)
                        .map(|(_, scope)| *scope)
                        .collect::<Vec<_>>(),
                    operation: String::from(entry.operation),
                    status: entry.status,
                })
                .collect(),
        }
    }

    /// How many records are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests;
