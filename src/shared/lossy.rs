//! Numeric conversions std offers only as `as` casts, each named for what it
//! loses: a float rounded or truncated to an integer, and a `u64` widened to
//! the nearest `f64`.

/// `x` rounded to the nearest integer, halves away from zero; negative
/// values and NaN give 0, values past `u64::MAX` give `u64::MAX`.
pub(crate) fn round_u64(x: f64) -> u64 {
    x.round() as u64
}

/// `x` rounded to the nearest integer, halves away from zero; NaN gives 0,
/// values past either end saturate at `i64::MIN` or `i64::MAX`.
pub(crate) fn round_i64(x: f64) -> i64 {
    x.round() as i64
}

/// `x` truncated toward zero; NaN gives 0, values past either end saturate
/// at `i64::MIN` or `i64::MAX`.
pub(crate) fn trunc_i64(x: f64) -> i64 {
    x as i64
}

/// The `f64` nearest `n`, ties to even: exact through 2^53, and above it the
/// nearest multiple of 2^(e-52) for the value's exponent e.
pub(crate) fn to_f64(n: u64) -> f64 {
    n as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_53: u64 = 1 << 53;

    #[test]
    fn round_u64_rounds_halves_away_from_zero() {
        assert_eq!(round_u64(2.5), 3);
        assert_eq!(round_u64(2.4999), 2);
        assert_eq!(round_u64(0.5), 1);
        assert_eq!(round_u64(-0.4), 0);
    }

    #[test]
    fn round_u64_saturates_and_maps_nan_to_zero() {
        assert_eq!(round_u64(f64::NAN), 0);
        assert_eq!(round_u64(-1.0), 0);
        assert_eq!(round_u64(f64::NEG_INFINITY), 0);
        assert_eq!(round_u64(f64::INFINITY), u64::MAX);
        assert_eq!(round_u64(1e20), u64::MAX);
        assert_eq!(round_u64(9_007_199_254_740_992.0), TWO_53);
    }

    #[test]
    fn round_i64_rounds_halves_away_from_zero() {
        assert_eq!(round_i64(-2.5), -3);
        assert_eq!(round_i64(2.5), 3);
        assert_eq!(round_i64(-0.4), 0);
    }

    #[test]
    fn round_i64_saturates_and_maps_nan_to_zero() {
        assert_eq!(round_i64(f64::NAN), 0);
        assert_eq!(round_i64(f64::INFINITY), i64::MAX);
        assert_eq!(round_i64(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(round_i64(1e19), i64::MAX);
        assert_eq!(round_i64(-1e19), i64::MIN);
        assert_eq!(round_i64(-9_223_372_036_854_775_808.0), i64::MIN);
        assert_eq!(round_i64(9_223_372_036_854_774_784.0), 9_223_372_036_854_774_784);
    }

    #[test]
    fn trunc_i64_drops_the_fraction_toward_zero() {
        assert_eq!(trunc_i64(2.9), 2);
        assert_eq!(trunc_i64(-2.9), -2);
        assert_eq!(trunc_i64(-0.5), 0);
    }

    #[test]
    fn trunc_i64_saturates_and_maps_nan_to_zero() {
        assert_eq!(trunc_i64(f64::NAN), 0);
        assert_eq!(trunc_i64(f64::INFINITY), i64::MAX);
        assert_eq!(trunc_i64(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(trunc_i64(-1e19), i64::MIN);
        assert_eq!(trunc_i64(1e19), i64::MAX);
        assert_eq!(trunc_i64(-9_223_372_036_854_775_808.0), i64::MIN);
    }

    #[test]
    fn to_f64_is_exact_through_two_to_the_53() {
        assert_eq!(round_u64(to_f64(0)), 0);
        assert_eq!(round_u64(to_f64(TWO_53 - 1)), TWO_53 - 1);
        assert_eq!(round_u64(to_f64(TWO_53)), TWO_53);
    }

    #[test]
    fn to_f64_rounds_past_two_to_the_53_to_the_nearest_even() {
        assert_eq!(round_u64(to_f64(TWO_53 + 1)), TWO_53);
        assert_eq!(round_u64(to_f64(TWO_53 + 3)), TWO_53 + 4);
        assert_eq!(round_u64(to_f64(u64::MAX)), u64::MAX);
    }
}
