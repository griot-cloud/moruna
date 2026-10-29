//! What the tests of SDD 24 share: a seeded generator, keys, synthetic units, scratch dirs.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use moruna_cost::{CostKey, Digest, Inputs, Origin, Outcome, Target};
use moruna_job::Role;

/// SplitMix64, so every test draws the same numbers on every machine.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u = self.unit().max(1e-300);
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    /// Log-uniform in [lo, hi].
    pub fn log_uniform(&mut self, lo: f64, hi: f64) -> f64 {
        (lo.ln() + (hi.ln() - lo.ln()) * self.unit()).exp()
    }
}

/// A key of `kind` whose function is `n`.
pub fn key(kind: &str, n: u8) -> CostKey {
    CostKey {
        role: Role::Kernel,
        kind: kind.into(),
        fingerprint: Digest([n; 32]),
        settings: Digest([0; 32]),
        schema: Digest([7; 32]),
    }
}

/// A batch of `rows` rows of 64 bytes.
pub fn batch(rows: f64) -> Inputs {
    let rows = rows.round().max(1.0) as u64;
    Inputs::batch(rows, rows.saturating_mul(64))
}

/// An outcome of `origin` measuring `duration`.
pub fn took(origin: Origin, duration: f64) -> Outcome {
    let mut o = Outcome::new(origin);
    o.set(Target::Duration, duration);
    o
}

/// A scratch directory unique to this process and call (CONTRIBUTING: tests write only there).
pub fn scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "moruna-cost-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The median of a list.
pub fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}
