//! Forward curve — maps hour-of-day to an expected reference price.
//!
//! Each MM's reference is recomputed from this curve at every rolling
//! tick based on its current delivery period. Scenarios layer
//! per-hour shifts and weather-sensitivity adjustments on top.

use rust_decimal::Decimal;

/// Per-hour pricing parameters. `base_price` is what the curve
/// returns under "moderate weather, weekday"; the three coefficients
/// will pull the effective reference down under high renewables
/// output and up under heating demand once weather is wired in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PricePoint {
    /// Expected reference price for this hour, EUR/MWh.
    pub base_price: f64,
    /// EUR drop per (W/m² above 200 W/m² of solar irradiance) at
    /// this hour. Zero at night, biggest around midday.
    pub solar_coef: f64,
    /// EUR drop per (m/s above 5 m/s of wind speed) at this hour.
    pub wind_coef: f64,
    /// EUR rise per Kelvin of heating-degree-hours at this hour
    /// (max(0, 290 K - temperature)).
    pub load_coef: f64,
}

/// 25 anchors (hours 0..24, with 24 == 0 for cyclic continuity).
/// Linear interpolation between adjacent anchors.
#[derive(Clone, Debug)]
pub struct ForwardCurve {
    anchors: Vec<PricePoint>,
}

impl Default for ForwardCurve {
    fn default() -> Self {
        Self::de_lu_typical()
    }
}

impl ForwardCurve {
    /// The default DE-LU intraday curve. Numbers chosen to match a
    /// median DE-LU weekday: night ~50, morning peak ~110, midday
    /// belly ~25, evening peak ~150 (EUR/MWh). Sensitivity coefs
    /// concentrated in the hours where the respective driver
    /// actually matters (solar 09-16, wind / load overnight).
    pub fn de_lu_typical() -> Self {
        // (base_price, solar_coef, wind_coef, load_coef), one row per hour.
        // Row 24 must equal row 0 (cyclic).
        const ROWS: [(f64, f64, f64, f64); 25] = [
            (55.0, 0.00, 0.50, 0.80),  //  0  night
            (50.0, 0.00, 0.50, 0.90),  //  1
            (48.0, 0.00, 0.50, 0.90),  //  2
            (45.0, 0.00, 0.50, 0.90),  //  3  low
            (50.0, 0.00, 0.50, 0.80),  //  4
            (60.0, 0.00, 0.40, 0.60),  //  5
            (85.0, 0.00, 0.30, 0.40),  //  6  morning ramp
            (100.0, 0.00, 0.30, 0.30), //  7
            (110.0, 0.00, 0.20, 0.20), //  8  morning peak
            (90.0, 0.05, 0.20, 0.10),  //  9
            (60.0, 0.08, 0.20, 0.10),  // 10
            (40.0, 0.10, 0.20, 0.10),  // 11
            (25.0, 0.12, 0.20, 0.10),  // 12  belly
            (25.0, 0.12, 0.20, 0.10),  // 13
            (25.0, 0.10, 0.20, 0.10),  // 14
            (40.0, 0.07, 0.20, 0.10),  // 15
            (65.0, 0.03, 0.30, 0.20),  // 16  transition
            (95.0, 0.00, 0.30, 0.30),  // 17
            (150.0, 0.00, 0.30, 0.50), // 18  evening peak
            (140.0, 0.00, 0.30, 0.50), // 19
            (110.0, 0.00, 0.40, 0.50), // 20
            (95.0, 0.00, 0.40, 0.60),  // 21
            (75.0, 0.00, 0.50, 0.70),  // 22
            (65.0, 0.00, 0.50, 0.80),  // 23
            (55.0, 0.00, 0.50, 0.80),  // 24 = 00 cyclic
        ];
        let anchors = ROWS
            .iter()
            .map(|(b, s, w, l)| PricePoint {
                base_price: *b,
                solar_coef: *s,
                wind_coef: *w,
                load_coef: *l,
            })
            .collect();
        Self { anchors }
    }

    /// Per-hour parameters at fractional hour `hour` in [0, 24).
    /// Linear interpolation between the two flanking anchors; values
    /// outside the day-cycle wrap modulo 24.
    pub fn point_at(&self, hour: f64) -> PricePoint {
        let h = hour.rem_euclid(24.0);
        let lo = (h.floor() as usize).min(23);
        let hi = lo + 1;
        let t = h - lo as f64;
        let a = self.anchors[lo];
        let b = self.anchors[hi];
        PricePoint {
            base_price: lerp(a.base_price, b.base_price, t),
            solar_coef: lerp(a.solar_coef, b.solar_coef, t),
            wind_coef: lerp(a.wind_coef, b.wind_coef, t),
            load_coef: lerp(a.load_coef, b.load_coef, t),
        }
    }

    /// Reference price at fractional hour `hour` under moderate
    /// weather (no solar / wind / load adjustments). Snapped to the
    /// 0.01-EUR tick so MM submissions built from it pass the grid
    /// check in `validate_common`.
    pub fn base_price_at(&self, hour: f64) -> Decimal {
        snap_to_cent(self.point_at(hour).base_price)
    }

    /// Override the base price at integer hour `h` (0..=24).
    /// `h = 0` and `h = 24` are kept in sync so the cyclic boundary
    /// stays continuous. Caller's responsibility to call with `h` in
    /// range; out-of-range silently does nothing.
    pub fn set_base_price_at(&mut self, h: usize, price: f64) {
        if h > 24 {
            return;
        }
        self.anchors[h].base_price = price;
        if h == 0 {
            self.anchors[24].base_price = price;
        } else if h == 24 {
            self.anchors[0].base_price = price;
        }
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn snap_to_cent(v: f64) -> Decimal {
    let cents = (v * 100.0).round() as i64;
    Decimal::new(cents, 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn anchors_match_table() {
        let c = ForwardCurve::default();
        assert_eq!(c.point_at(0.0).base_price, 55.0);
        assert_eq!(c.point_at(8.0).base_price, 110.0);
        assert_eq!(c.point_at(12.0).base_price, 25.0);
        assert_eq!(c.point_at(18.0).base_price, 150.0);
    }

    #[test]
    fn midpoints_interpolate_linearly() {
        let c = ForwardCurve::default();
        // 06:30 — between 85 (06) and 100 (07) = 92.5
        assert!((c.point_at(6.5).base_price - 92.5).abs() < 1e-9);
        // 11:30 — between 40 (11) and 25 (12) = 32.5
        assert!((c.point_at(11.5).base_price - 32.5).abs() < 1e-9);
    }

    #[test]
    fn base_price_snaps_to_cent() {
        let c = ForwardCurve::default();
        assert_eq!(c.base_price_at(11.5), dec!(32.50));
        // 06:00 anchor is 85.0 exactly
        assert_eq!(c.base_price_at(6.0), dec!(85.00));
    }

    #[test]
    fn morning_ramp_strictly_increasing() {
        let c = ForwardCurve::default();
        let prices: Vec<f64> = (5..=8).map(|h| c.point_at(h as f64).base_price).collect();
        for w in prices.windows(2) {
            assert!(w[0] < w[1], "morning ramp not monotonic: {prices:?}");
        }
    }

    #[test]
    fn belly_is_far_below_morning_peak() {
        let c = ForwardCurve::default();
        let morning = c.point_at(8.0).base_price;
        let belly = c.point_at(13.0).base_price;
        // Plan target: belly ~25, morning ~110 — belly should be < 30% of morning.
        assert!(
            belly < morning * 0.30,
            "belly {belly} not far below morning {morning}"
        );
    }

    #[test]
    fn evening_is_the_global_peak() {
        let c = ForwardCurve::default();
        let evening = c.point_at(18.0).base_price;
        let other_max = (0..24)
            .filter(|h| *h != 18)
            .map(|h| c.point_at(h as f64).base_price)
            .fold(f64::MIN, f64::max);
        assert!(
            evening > other_max,
            "evening {evening} not max (next {other_max})"
        );
    }

    #[test]
    fn solar_coef_peaks_at_midday() {
        let c = ForwardCurve::default();
        let mid = c.point_at(12.5).solar_coef;
        assert!(mid > 0.10, "solar coef midday too low: {mid}");
        // Solar coef should be zero overnight (no PV to dump).
        assert_eq!(c.point_at(2.0).solar_coef, 0.0);
        assert_eq!(c.point_at(22.0).solar_coef, 0.0);
    }

    #[test]
    fn curve_wraps_cyclically() {
        let c = ForwardCurve::default();
        // 24:00 should equal 00:00 (the last anchor mirrors the first).
        assert_eq!(c.point_at(24.0).base_price, c.point_at(0.0).base_price);
        // 25.0 should wrap to 1.0
        assert_eq!(c.point_at(25.0).base_price, c.point_at(1.0).base_price);
    }

    #[test]
    fn set_base_overrides_one_anchor() {
        let mut c = ForwardCurve::default();
        c.set_base_price_at(12, -50.0);
        assert_eq!(c.point_at(12.0).base_price, -50.0);
        // Other hours untouched.
        assert_eq!(c.point_at(18.0).base_price, 150.0);
    }

    #[test]
    fn set_base_at_zero_syncs_with_twentyfour() {
        let mut c = ForwardCurve::default();
        c.set_base_price_at(0, 200.0);
        assert_eq!(c.point_at(0.0).base_price, 200.0);
        assert_eq!(c.point_at(24.0).base_price, 200.0);
    }
}
