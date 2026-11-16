//! Score's matrix products on a real GEMM: `matrixmultiply`'s `dgemm`, pure Rust, `no_std` with
//! `alloc`.
//!
//! Built without its `std` feature it picks its kernel at compile time from the target's
//! features, with no CPU detection at run time: the gate builds every Rust row with the same
//! features (`images/gate/.cargo/config.toml`), so the native task, the baseline and 0.6.6's
//! extension run the same kernel and compute the same bits. With `fma` the kernel fuses its
//! multiply-adds, as BLAS does; Score's `predicted_tip` is matched within its named tolerance.

/// `c = a @ b` for row-major `a` (`m` x `k`), `b` (`k` x `n`) and `c` (`m` x `n`). Slices too
/// short for their shapes leave `c` untouched and return `false`.
#[allow(unsafe_code)]
pub fn matmul(m: usize, k: usize, n: usize, a: &[f64], b: &[f64], c: &mut [f64]) -> bool {
    let fits = |len: usize, rows: usize, cols: usize| {
        rows.checked_mul(cols).is_some_and(|need| len >= need)
    };
    if !(fits(a.len(), m, k) && fits(b.len(), k, n) && fits(c.len(), m, n)) {
        return false;
    }
    if m == 0 || n == 0 {
        return true;
    }
    if k == 0 {
        c[..m * n].fill(0.0);
        return true;
    }
    let (Ok(sk), Ok(sn)) = (isize::try_from(k), isize::try_from(n)) else {
        return false;
    };
    // SAFETY: `a` holds at least `m * k` values, `b` at least `k * n` and `c` at least `m * n`,
    // checked above, and the strides (`k`, 1), (`n`, 1) and (`n`, 1) address exactly those
    // row-major elements. `c` is borrowed mutably, so it does not alias `a` or `b`. With beta 0,
    // dgemm writes every element of `c` and reads none.
    unsafe {
        matrixmultiply::dgemm(
            m,
            k,
            n,
            1.0,
            a.as_ptr(),
            sk,
            1,
            b.as_ptr(),
            sn,
            1,
            0.0,
            c.as_mut_ptr(),
            sn,
            1,
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn small_products() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]; // 2 x 3
        let b = [7.0, 8.0, 9.0, 10.0, 11.0, 12.0]; // 3 x 2
        let mut c = [0.0; 4];
        assert!(matmul(2, 3, 2, &a, &b, &mut c));
        assert_eq!(c, [58.0, 64.0, 139.0, 154.0]);
        assert!(!matmul(3, 3, 2, &a, &b, &mut c));
        assert!(matmul(0, 3, 2, &a, &b, &mut c));
        let mut z = vec![9.0; 4];
        assert!(matmul(2, 0, 2, &a, &b, &mut z));
        assert_eq!(z, [0.0; 4]);
        assert!(!matmul(usize::MAX, 2, 2, &a, &b, &mut c));
    }
}
