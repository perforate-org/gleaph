/// Returns `floor(log2(x))`, treating `x <= 1` as `0`.
pub fn floor_log2(x: u64) -> u32 {
    if x <= 1 {
        return 0;
    }
    63 - x.leading_zeros()
}

/// Returns `ceil(log2(x))`, treating `x <= 1` as `0`.
pub fn ceil_log2(x: u64) -> u32 {
    if x <= 1 {
        return 0;
    }
    let f = floor_log2(x);
    if x.is_power_of_two() { f } else { f + 1 }
}

/// Returns the greatest power of two less than or equal to `x`.
pub fn hyperfloor(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    1u64 << floor_log2(x)
}

/// Returns `ceil(n / d)` for integer operands.
pub fn ceil_div(n: u64, d: u64) -> u64 {
    assert!(d != 0, "division by zero");
    n.div_ceil(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_edge_cases() {
        assert_eq!(floor_log2(0), 0);
        assert_eq!(floor_log2(1), 0);
        assert_eq!(floor_log2(2), 1);
        assert_eq!(floor_log2(3), 1);
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(1024), 10);
        assert_eq!(ceil_log2(1025), 11);
    }

    #[test]
    fn power_helpers() {
        assert_eq!(hyperfloor(0), 0);
        assert_eq!(hyperfloor(1), 1);
        assert_eq!(hyperfloor(7), 4);
        assert_eq!(hyperfloor(8), 8);
        assert_eq!(hyperfloor(9), 8);
        assert_eq!(ceil_div(0, 3), 0);
        assert_eq!(ceil_div(1, 3), 1);
        assert_eq!(ceil_div(10, 3), 4);
        assert_eq!(ceil_div(u64::MAX - 1, 2), (u64::MAX - 1).div_ceil(2));
    }
}
