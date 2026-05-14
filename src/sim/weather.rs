//! Weather state — internal atmospheric model that the forward
//! curve consumes (solar / wind / temperature adjustments) and that
//! the WeatherForecastService gRPC stream will surface to trading
//! apps (step 8 of the realism upgrade in plan.org).
//!
//! This module is data + math only — no callers wired yet. Step 6
//! adds the lisp setters; step 7 has the forward curve read these
//! values; step 8 is the gRPC surface.

use std::sync::Arc;

use parking_lot::RwLock;

/// Internal "ground truth" weather. A single global state for now;
/// per-area locations land in a follow-up once the gRPC service
/// needs them.
#[derive(Clone, Debug)]
pub struct WeatherState {
    /// Cloud cover fraction, 0.0 = clear sky, 1.0 = fully overcast.
    /// Linearly attenuates the solar irradiance peak.
    pub cloud_cover: f64,
    /// Mean wind speed at 100 m altitude, m/s.
    pub mean_wind: f64,
    /// Wind direction in degrees (0 = N, 90 = E). Stored mostly so
    /// the gRPC surface can emit the u/v components the Frequenz
    /// API expects; the curve only uses |wind|.
    pub wind_direction: f64,
    /// Diurnal-cycle midpoint temperature in Kelvin. Sinusoidal
    /// daily variation of ±8 K around this anchor.
    pub temperature_base: f64,
}

impl Default for WeatherState {
    fn default() -> Self {
        Self::de_lu_typical()
    }
}

impl WeatherState {
    /// Typical-day defaults for the DE-LU zone: light cloud cover,
    /// 6 m/s wind, 290 K (~17 °C) midpoint.
    pub fn de_lu_typical() -> Self {
        Self {
            cloud_cover: 0.30,
            mean_wind: 6.0,
            wind_direction: 270.0, // westerly, the German prevailing wind
            temperature_base: 290.0,
        }
    }

    /// Surface solar irradiance in W/m² at the given fractional
    /// hour. Sinusoidal peak at 12:00 reaching ~1000 W/m² under
    /// clear skies; zero outside the 06:00–18:00 daylight window.
    /// `cloud_cover` attenuates the peak linearly.
    pub fn solar_at(&self, hour: f64) -> f64 {
        let h = hour.rem_euclid(24.0);
        if !(6.0..=18.0).contains(&h) {
            return 0.0;
        }
        let t = (h - 6.0) / 12.0;
        let clear_sky = 1000.0 * (std::f64::consts::PI * t).sin();
        clear_sky * (1.0 - self.cloud_cover).clamp(0.0, 1.0)
    }

    /// Wind speed magnitude (m/s) at the given hour. Currently
    /// time-independent — the underlying state holds the mean and
    /// future tick evolution will add a random walk.
    pub fn wind_at(&self, _hour: f64) -> f64 {
        self.mean_wind.max(0.0)
    }

    /// Air temperature (K) at the given fractional hour. Sinusoidal
    /// diurnal cycle with the warm peak at 14:00 and the cold low
    /// at 02:00, ±8 K around `temperature_base`.
    pub fn temperature_at(&self, hour: f64) -> f64 {
        let h = hour.rem_euclid(24.0);
        // cos(2π·(h-14)/24) peaks at +1 when h = 14, and at -1 when h = 2.
        let offset = (2.0 * std::f64::consts::PI * (h - 14.0) / 24.0).cos();
        self.temperature_base + 8.0 * offset
    }

    /// Heating-degree-hours at the given hour: how far below 290 K
    /// the temperature has fallen, clamped to non-negative. The
    /// curve's `load_coef` multiplies this to bump prices when
    /// people are heating their homes.
    pub fn heating_degree(&self, hour: f64) -> f64 {
        (290.0 - self.temperature_at(hour)).max(0.0)
    }
}

pub type SharedWeather = Arc<RwLock<WeatherState>>;

pub fn new_state() -> SharedWeather {
    Arc::new(RwLock::new(WeatherState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solar_zero_overnight() {
        let w = WeatherState::default();
        assert_eq!(w.solar_at(0.0), 0.0);
        assert_eq!(w.solar_at(3.0), 0.0);
        assert_eq!(w.solar_at(22.0), 0.0);
    }

    #[test]
    fn solar_peaks_at_noon() {
        let w = WeatherState {
            cloud_cover: 0.0,
            ..WeatherState::default()
        };
        let peak = w.solar_at(12.0);
        assert!((peak - 1000.0).abs() < 1e-9, "got {peak}");
        // 09:00 and 15:00 should be symmetric and ~half-peak each.
        let am = w.solar_at(9.0);
        let pm = w.solar_at(15.0);
        assert!((am - pm).abs() < 1e-9);
        assert!(am > 600.0 && am < 800.0);
    }

    #[test]
    fn cloud_cover_attenuates_solar() {
        let clear = WeatherState {
            cloud_cover: 0.0,
            ..WeatherState::default()
        };
        let overcast = WeatherState {
            cloud_cover: 0.80,
            ..WeatherState::default()
        };
        assert!((clear.solar_at(12.0) - 1000.0).abs() < 1e-9);
        assert!((overcast.solar_at(12.0) - 200.0).abs() < 1e-9);
    }

    #[test]
    fn temperature_diurnal_cycle() {
        let w = WeatherState::default();
        let warmest = w.temperature_at(14.0);
        let coldest = w.temperature_at(2.0);
        assert!((warmest - 298.0).abs() < 1e-9, "got {warmest}");
        assert!((coldest - 282.0).abs() < 1e-9, "got {coldest}");
        // Midday should be between the extremes.
        let noon = w.temperature_at(12.0);
        assert!(noon > coldest && noon < warmest);
    }

    #[test]
    fn heating_degree_high_at_night() {
        let w = WeatherState::default();
        let night = w.heating_degree(2.0); // coldest
        let day = w.heating_degree(14.0); // warmest
        assert!(night > 0.0);
        assert_eq!(day, 0.0); // warm enough to need no heating
    }

    #[test]
    fn wind_returns_mean() {
        let w = WeatherState {
            mean_wind: 12.5,
            ..WeatherState::default()
        };
        assert_eq!(w.wind_at(8.0), 12.5);
        assert_eq!(w.wind_at(20.0), 12.5);
    }
}
