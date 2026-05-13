//! Decimal helpers for price/qty grid math. The proto carries
//! `Decimal { string value }` and the sim uses `rust_decimal::Decimal`
//! internally — both are exact, so price-tick / qty-step alignment is
//! a true equality check, not an epsilon comparison.

use rust_decimal::{Decimal, dec};

/// Default intraday price tick: 0.01 EUR/MWh.
pub const DEFAULT_PRICE_TICK: Decimal = dec!(0.01);

/// Default intraday quantity step: 0.1 MW.
pub const DEFAULT_QTY_STEP: Decimal = dec!(0.1);

/// Snap `value` to the nearest multiple of `tick`, half-away-from-zero.
/// Used by the matcher when a counterparty lisp closure returns a raw
/// price from a continuous price curve — the book only accepts on-grid
/// prices.
pub fn snap_to_tick(value: Decimal, tick: Decimal) -> Decimal {
    debug_assert!(tick > Decimal::ZERO, "tick must be positive");
    let q = (value / tick).round_dp_with_strategy(
        0,
        rust_decimal::RoundingStrategy::MidpointAwayFromZero,
    );
    q * tick
}

/// True iff `value` is an exact non-negative multiple of `step`. The
/// pre-trade validator uses this; rejections cite
/// `STATE_REASON_VALIDATION_FAIL`.
pub fn is_multiple_of(value: Decimal, step: Decimal) -> bool {
    debug_assert!(step > Decimal::ZERO, "step must be positive");
    (value % step).is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_rounds_to_nearest_tick() {
        let tick = dec!(0.01);
        assert_eq!(snap_to_tick(dec!(85.504), tick), dec!(85.50));
        assert_eq!(snap_to_tick(dec!(85.505), tick), dec!(85.51));
        assert_eq!(snap_to_tick(dec!(-85.505), tick), dec!(-85.51));
    }

    #[test]
    fn snap_is_identity_on_grid() {
        let tick = dec!(0.01);
        for v in [dec!(0), dec!(0.01), dec!(85.50), dec!(-12.34)] {
            assert_eq!(snap_to_tick(v, tick), v);
        }
    }

    #[test]
    fn snap_with_coarse_tick() {
        let tick = dec!(0.5);
        assert_eq!(snap_to_tick(dec!(1.24), tick), dec!(1.0));
        assert_eq!(snap_to_tick(dec!(1.26), tick), dec!(1.5));
    }

    #[test]
    fn is_multiple_of_grid_alignment() {
        let step = dec!(0.1);
        assert!(is_multiple_of(dec!(0.1), step));
        assert!(is_multiple_of(dec!(2.5), step));
        assert!(is_multiple_of(dec!(0), step));
        assert!(!is_multiple_of(dec!(0.15), step));
        assert!(!is_multiple_of(dec!(2.55), step));
    }

    #[test]
    fn is_multiple_of_handles_negatives() {
        let step = dec!(0.01);
        assert!(is_multiple_of(dec!(-12.34), step));
        assert!(!is_multiple_of(dec!(-12.345), step));
    }
}
