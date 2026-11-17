//! The refused calls of a process (SDD 28, f.6): every call answered `ENOSYS`, counted
//! by number, reported when the process ends so the conformance work knows what real
//! programs ask for.

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use crate::table;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refused {
    counts: BTreeMap<u64, u64>,
}

impl Refused {
    pub fn new() -> Refused {
        Refused::default()
    }

    /// Counts one refusal of call `number`.
    pub fn count(&mut self, number: u64) {
        *self.counts.entry(number).or_insert(0) += 1;
    }

    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }

    /// Number and count, in number order.
    pub fn counts(&self) -> Vec<(u64, u64)> {
        self.counts.iter().map(|(n, c)| (*n, *c)).collect()
    }

    /// `name(number) xcount`, joined by commas; `none` when nothing was refused.
    pub fn render(counts: &[(u64, u64)]) -> String {
        if counts.is_empty() {
            return String::from("none");
        }
        counts
            .iter()
            .map(|(n, c)| match table::name(*n) {
                Some(name) => format!("{name}({n}) x{c}"),
                None => format!("{n} x{c}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_t6_counted_and_rendered() {
        let mut r = Refused::new();
        assert_eq!(Refused::render(&r.counts()), "none");
        r.count(41);
        r.count(334);
        r.count(41);
        r.count(9999);
        assert_eq!(r.total(), 4);
        assert_eq!(r.counts(), alloc::vec![(41, 2), (334, 1), (9999, 1)]);
        assert_eq!(
            Refused::render(&r.counts()),
            "socket(41) x2, rseq(334) x1, 9999 x1"
        );
    }
}
