//! Score's model, as `bench/vm/workloads.py` draws it from `np.random.default_rng(7)`: exported
//! bit for bit by the oracle (`bench/vm/gate_oracle.py`, `data/score_weights.bin`), not drawn
//! again here. `gate_oracle.py --check-constants` compares them with what a machine's NumPy
//! draws.

const WEIGHTS: &[u8] = include_bytes!("../data/score_weights.bin");
const TRIG: &[u8] = include_bytes!("../data/score_trig.bin");

pub const FEATURES: usize = 23;
pub const HIDDEN: usize = 128;
pub const ZONES: usize = 300;
pub const EMBED: usize = 8;

const N_EMB: usize = ZONES * EMBED;
const N_W1: usize = FEATURES * HIDDEN;
const N_W2: usize = HIDDEN * HIDDEN;
const AT_EMB: usize = 0;
const AT_W1: usize = AT_EMB + N_EMB;
const AT_B1: usize = AT_W1 + N_W1;
const AT_W2: usize = AT_B1 + HIDDEN;
const AT_B2: usize = AT_W2 + N_W2;
const AT_W3: usize = AT_B2 + HIDDEN;
const TOTAL: usize = AT_W3 + HIDDEN;

const _: () = assert!(
    WEIGHTS.len() == TOTAL * 8,
    "score_weights.bin is not the model's size"
);
const _: () = assert!(
    TRIG.len() == 48 * 8,
    "score_trig.bin is not two tables of 24"
);

const fn at(bytes: &[u8], i: usize) -> f64 {
    let b = i * 8;
    f64::from_bits(u64::from_le_bytes([
        bytes[b],
        bytes[b + 1],
        bytes[b + 2],
        bytes[b + 3],
        bytes[b + 4],
        bytes[b + 5],
        bytes[b + 6],
        bytes[b + 7],
    ]))
}

const fn table<const N: usize>(bytes: &[u8], from: usize) -> [f64; N] {
    let mut out = [0.0; N];
    let mut i = 0;
    while i < N {
        out[i] = at(bytes, from + i);
        i += 1;
    }
    out
}

/// `_EMB`, 300 x 8, row major.
pub static EMB: [f64; N_EMB] = table(WEIGHTS, AT_EMB);
/// `_W1`, 23 x 128, row major.
pub static W1: [f64; N_W1] = table(WEIGHTS, AT_W1);
pub static B1: [f64; HIDDEN] = table(WEIGHTS, AT_B1);
/// `_W2`, 128 x 128, row major.
pub static W2: [f64; N_W2] = table(WEIGHTS, AT_W2);
pub static B2: [f64; HIDDEN] = table(WEIGHTS, AT_B2);
/// `_W3`, 128 x 1.
pub static W3: [f64; HIDDEN] = table(WEIGHTS, AT_W3);
/// `np.sin(hour * (np.pi / 12))` for hours 0 to 23, as NumPy computed them.
pub static SIN_HOUR: [f64; 24] = table(TRIG, 0);
/// `np.cos(hour * (np.pi / 12))` for hours 0 to 23.
pub static COS_HOUR: [f64; 24] = table(TRIG, 24);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_read_at_run_time_as_at_compile_time() {
        let w1: [f64; N_W1] = table(WEIGHTS, AT_W1);
        assert_eq!(w1, W1);
        assert_eq!(at(TRIG, 24), COS_HOUR[0]);
        assert_eq!(SIN_HOUR[0], 0.0);
        assert_eq!(COS_HOUR[0], 1.0);
        assert_eq!(TOTAL * 8, WEIGHTS.len());
        assert!(
            EMB.iter()
                .chain(&B1)
                .chain(&W2)
                .chain(&B2)
                .chain(&W3)
                .all(|v| v.is_finite())
        );
    }
}
