//! Lexicographic compare and equality for byte slices (`[u8]` / `Ord` semantics).
//!
//! Used on hot paths (e.g. [`PropertyIndexKey`](crate::property_index::PropertyIndexKey)
//! `encoded_value`). Wasm builds with `target_feature = "simd128"` use 16-byte vector steps;
//! otherwise a portable big-endian `u64` chunk loop matches byte order correctly.

use core::cmp::Ordering;

/// Byte-wise equality (same as `a == b` for `&[u8]`).
#[inline]
pub(crate) fn eq_u8_slices(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    lex_cmp_u8_slices(a, b) == Ordering::Equal
}

/// Lexicographic order matching `Ord` for `[u8]`: common prefix, then `len().cmp`.
#[inline]
pub(crate) fn lex_cmp_u8_slices(a: &[u8], b: &[u8]) -> Ordering {
    let min_len = a.len().min(b.len());
    let mut i = 0;

    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        use core::arch::wasm32::*;

        while i + 16 <= min_len {
            // Avoid unaligned `v128_load`: build vectors from stack-aligned `[u8; 16]`.
            let ca: [u8; 16] = a[i..i + 16].try_into().expect("len checked");
            let cb: [u8; 16] = b[i..i + 16].try_into().expect("len checked");
            let va = unsafe { core::mem::transmute::<[u8; 16], v128>(ca) };
            let vb = unsafe { core::mem::transmute::<[u8; 16], v128>(cb) };
            let eq_mask = u8x16_eq(va, vb);
            if !u8x16_all_true(eq_mask) {
                return cmp_first_diff_in_block(&a[i..i + 16], &b[i..i + 16]);
            }
            i += 16;
        }
    }

    while i + 8 <= min_len {
        let x = u64::from_be_bytes(a[i..i + 8].try_into().expect("len checked"));
        let y = u64::from_be_bytes(b[i..i + 8].try_into().expect("len checked"));
        if x != y {
            return x.cmp(&y);
        }
        i += 8;
    }

    while i < min_len {
        let c = a[i].cmp(&b[i]);
        if c != Ordering::Equal {
            return c;
        }
        i += 1;
    }

    a.len().cmp(&b.len())
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
fn cmp_first_diff_in_block(a: &[u8], b: &[u8]) -> Ordering {
    debug_assert_eq!(a.len(), b.len());
    for k in 0..a.len() {
        let c = a[k].cmp(&b[k]);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_cmp(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    #[test]
    fn lex_matches_std_ord_exhaustive_small() {
        for len_a in 0..=4 {
            for len_b in 0..=4 {
                let mut buf_a = [0u8; 4];
                let mut buf_b = [0u8; 4];
                for x in 0u16..(1 << (len_a * 2).min(8)) {
                    for y in 0u16..(1 << (len_b * 2).min(8)) {
                        for la in 0..=len_a {
                            for lb in 0..=len_b {
                                for (i, slot) in buf_a.iter_mut().enumerate().take(la) {
                                    *slot = ((x >> (i * 2)) & 3) as u8;
                                }
                                for (j, slot) in buf_b.iter_mut().enumerate().take(lb) {
                                    *slot = ((y >> (j * 2)) & 3) as u8;
                                }
                                let sa = &buf_a[..la];
                                let sb = &buf_b[..lb];
                                assert_eq!(
                                    lex_cmp_u8_slices(sa, sb),
                                    ref_cmp(sa, sb),
                                    "a={sa:?} b={sb:?}"
                                );
                                assert_eq!(eq_u8_slices(sa, sb), sa == sb);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn lex_matches_std_ord_random_long() {
        let mut a = vec![0u8; 300];
        let mut b = vec![0u8; 300];
        for seed in 0u64..2000 {
            let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for v in &mut a {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                *v = s as u8;
            }
            for v in &mut b {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                *v = s as u8;
            }
            for la in (0..=300).step_by(17) {
                for lb in (0..=300).step_by(23) {
                    let sa = &a[..la];
                    let sb = &b[..lb];
                    assert_eq!(lex_cmp_u8_slices(sa, sb), ref_cmp(sa, sb));
                    assert_eq!(eq_u8_slices(sa, sb), sa == sb);
                }
            }
        }
    }

    #[test]
    fn u64_be_chunk_disambiguation() {
        // Lex order compares first byte; must not use naive LE u64 compare.
        let a = [1u8, 0, 0, 0, 0, 0, 0, 0, 0];
        let b = [0u8, 255, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(lex_cmp_u8_slices(&a, &b), Ordering::Greater);
    }
}
