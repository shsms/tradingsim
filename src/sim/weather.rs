//! Weather state — a registry of per-location atmospheric models
//! that the forward curve consumes (solar / wind / temperature
//! adjustments) and that the WeatherForecastService surfaces over
//! gRPC. Locations are keyed at 0.1° lat/lon granularity; a lookup
//! by arbitrary lat/lon snaps to the nearest registered grid point.
//!
//! Backwards compat: the legacy `(set-weather-*)` setters mutate
//! the registry's default location (index 0); a freshly-constructed
//! registry pre-populates that default with the DE-LU typical
//! values so callers that never register an explicit location keep
//! working.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// 0.1° lat/lon grid key — multiply by 10, round to i32, so 51.5
/// becomes (515, 95) regardless of how the input was spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocationKey {
    pub lat_tenths: i32,
    pub lon_tenths: i32,
}

impl LocationKey {
    pub fn from_latlon(lat: f64, lon: f64) -> Self {
        Self {
            lat_tenths: (lat * 10.0).round() as i32,
            lon_tenths: (lon * 10.0).round() as i32,
        }
    }
    pub fn lat(&self) -> f64 {
        self.lat_tenths as f64 / 10.0
    }
    pub fn lon(&self) -> f64 {
        self.lon_tenths as f64 / 10.0
    }
}

/// Per-location atmospheric parameters + the derived feature
/// curves the bias tick + gRPC service consume.
///
/// Each tunable field carries a paired `baseline_*` value that
/// scenarios don't touch. The bias tick resets the live fields to
/// their baseline at the start of every cycle and then applies any
/// active scenario-stage overrides on top, so stopping a scenario
/// restores the configured weather without explicit cleanup.
#[derive(Clone, Debug)]
pub struct WeatherLocation {
    /// Display label (e.g. "tn" for TenneT's anchor point).
    pub name: String,
    /// Latitude (decimal degrees, +N). Stored as the unrounded
    /// value the caller supplied; the registry key snaps to 0.1°.
    pub lat: f64,
    pub lon: f64,
    /// Cloud cover fraction, 0.0 = clear, 1.0 = overcast.
    pub cloud_cover: f64,
    /// Mean wind speed at 100 m altitude, m/s.
    pub mean_wind: f64,
    /// Wind direction degrees (0 = N, 90 = E).
    pub wind_direction: f64,
    /// Diurnal-cycle midpoint temperature in Kelvin.
    pub temperature_base: f64,
    /// Config-set baselines. Restored by the bias tick each cycle
    /// before scenario overrides apply.
    pub baseline_cloud_cover: f64,
    pub baseline_mean_wind: f64,
    pub baseline_temperature_base: f64,
}

impl WeatherLocation {
    /// Default DE-LU midpoint (~Frankfurt). Light cloud cover,
    /// westerly wind, 290 K (~17 °C) midpoint.
    pub fn de_lu_typical() -> Self {
        Self {
            name: "default".into(),
            lat: 50.1,
            lon: 8.7,
            cloud_cover: 0.30,
            mean_wind: 6.0,
            wind_direction: 270.0,
            temperature_base: 290.0,
            baseline_cloud_cover: 0.30,
            baseline_mean_wind: 6.0,
            baseline_temperature_base: 290.0,
        }
    }

    /// Reset live tunable fields to their baselines. Called by the
    /// bias tick at the start of each cycle so scenario overrides
    /// never compound across stages.
    pub fn reset_to_baseline(&mut self) {
        self.cloud_cover = self.baseline_cloud_cover;
        self.mean_wind = self.baseline_mean_wind;
        self.temperature_base = self.baseline_temperature_base;
    }

    /// Surface solar irradiance in W/m² at fractional UTC `hour`
    /// on `day_of_year` (1..=366), at this location's latitude.
    ///
    /// Uses the standard solar-elevation formula —
    ///   sin α = sin φ · sin δ + cos φ · cos δ · cos H
    /// — with declination δ derived from day-of-year and hour-angle
    /// H from UTC hour. Below the horizon, returns zero. Above it,
    /// applies a simple ASHRAE-style atmospheric transmittance
    /// τ^(1/sin α), τ=0.7, so low-elevation sun attenuates steeply
    /// through the longer air-mass path: at 10° elevation only
    /// ~30 W/m² makes it down even under perfectly clear skies.
    ///
    /// Caller resolves day-of-year from the active scenario's
    /// :date (if set) or today's wallclock; we don't store it on
    /// the location so the same lat/lon can be reused across
    /// dates without re-instantiating.
    pub fn solar_at(&self, hour: f64, day_of_year: u32) -> f64 {
        const SOLAR_CONSTANT: f64 = 1361.0;
        const TAU: f64 = 0.7;
        let lat = self.lat.to_radians();
        let decl_amplitude = 23.45_f64.to_radians();
        let decl = decl_amplitude
            * (2.0 * std::f64::consts::PI * (284.0 + day_of_year as f64) / 365.0).sin();
        let hour_angle = 15.0_f64.to_radians() * (hour - 12.0);
        let sin_a = lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos();
        if sin_a <= 0.0 {
            return 0.0;
        }
        let air_mass = 1.0 / sin_a;
        let clear = SOLAR_CONSTANT * sin_a * TAU.powf(air_mass);
        clear * (1.0 - self.cloud_cover).clamp(0.0, 1.0)
    }

    pub fn wind_at(&self, _hour: f64) -> f64 {
        self.mean_wind.max(0.0)
    }

    pub fn temperature_at(&self, hour: f64) -> f64 {
        let h = hour.rem_euclid(24.0);
        let offset = (2.0 * std::f64::consts::PI * (h - 14.0) / 24.0).cos();
        self.temperature_base + 8.0 * offset
    }

    pub fn heating_degree(&self, hour: f64) -> f64 {
        (290.0 - self.temperature_at(hour)).max(0.0)
    }
}

/// Multi-location weather registry. Slot 0 is the "default"
/// location the legacy single-state setters mutate; subsequent
/// slots are user-registered locations keyed at 0.1° lat/lon.
#[derive(Clone, Debug)]
pub struct WeatherRegistry {
    locations: Vec<WeatherLocation>,
    /// 0.1°-grid index → slot in `locations`.
    by_key: HashMap<LocationKey, usize>,
    /// Area EIC → slot. Lets the bias tick look up the weather a
    /// given delivery area is "anchored" to.
    by_area: HashMap<String, usize>,
    /// Day-of-year (1-366) the solar-elevation model uses. The
    /// bias tick writes this each cycle: scenario :date if set,
    /// today's UTC date otherwise. Stored here so the gRPC
    /// weather service reads the same value the MM pricing
    /// pipeline does without an extra shared handle.
    pub active_day_of_year: Option<u32>,
}

impl Default for WeatherRegistry {
    fn default() -> Self {
        let default_loc = WeatherLocation::de_lu_typical();
        let key = LocationKey::from_latlon(default_loc.lat, default_loc.lon);
        let mut by_key = HashMap::new();
        by_key.insert(key, 0);
        Self {
            locations: vec![default_loc],
            by_key,
            by_area: HashMap::new(),
            active_day_of_year: None,
        }
    }
}

impl WeatherRegistry {
    /// Register a location (or update an existing one at the same
    /// 0.1°-grid point). Returns the registry index so callers can
    /// link an area to it via [`link_area`].
    pub fn upsert(&mut self, loc: WeatherLocation) -> usize {
        let key = LocationKey::from_latlon(loc.lat, loc.lon);
        if let Some(&idx) = self.by_key.get(&key) {
            self.locations[idx] = loc;
            idx
        } else {
            let idx = self.locations.len();
            self.locations.push(loc);
            self.by_key.insert(key, idx);
            idx
        }
    }

    /// Associate a delivery area EIC with a registered location.
    pub fn link_area(&mut self, area: impl Into<String>, idx: usize) {
        self.by_area.insert(area.into(), idx);
    }

    pub fn locations(&self) -> &[WeatherLocation] {
        &self.locations
    }

    pub fn locations_mut(&mut self) -> &mut [WeatherLocation] {
        &mut self.locations
    }

    /// Mutable handle on the default location — what legacy
    /// `(set-weather-cloud-cover)` etc. setters update.
    pub fn default_mut(&mut self) -> &mut WeatherLocation {
        &mut self.locations[0]
    }

    pub fn default_location(&self) -> &WeatherLocation {
        &self.locations[0]
    }

    /// Look up the location an area is anchored to. Falls back to
    /// the default when the area isn't explicitly linked.
    pub fn for_area(&self, area: &str) -> &WeatherLocation {
        self.by_area
            .get(area)
            .and_then(|i| self.locations.get(*i))
            .unwrap_or(&self.locations[0])
    }

    /// Look up the location at the given lat/lon, snapping to the
    /// 0.1° grid. Falls back to the default when no entry exists at
    /// that grid point.
    pub fn at_latlon(&self, lat: f64, lon: f64) -> &WeatherLocation {
        self.by_key
            .get(&LocationKey::from_latlon(lat, lon))
            .and_then(|i| self.locations.get(*i))
            .unwrap_or(&self.locations[0])
    }
}

pub type SharedWeather = Arc<RwLock<WeatherRegistry>>;

pub fn new_state() -> SharedWeather {
    Arc::new(RwLock::new(WeatherRegistry::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_loc() -> WeatherLocation {
        WeatherLocation::de_lu_typical()
    }

    #[test]
    fn location_key_snaps_to_tenths() {
        let k = LocationKey::from_latlon(51.523, 9.832);
        assert_eq!(k.lat_tenths, 515);
        assert_eq!(k.lon_tenths, 98);
        assert!((k.lat() - 51.5).abs() < 1e-9);
        assert!((k.lon() - 9.8).abs() < 1e-9);
    }

    #[test]
    fn registry_default_has_de_lu_typical() {
        let r = WeatherRegistry::default();
        assert_eq!(r.locations().len(), 1);
        assert_eq!(r.default_location().cloud_cover, 0.30);
    }

    #[test]
    fn upsert_replaces_same_grid_point() {
        let mut r = WeatherRegistry::default();
        let i1 = r.upsert(WeatherLocation {
            name: "alt".into(),
            cloud_cover: 0.9,
            ..default_loc()
        });
        // Same lat/lon as the default → same slot 0.
        assert_eq!(i1, 0);
        assert_eq!(r.default_location().cloud_cover, 0.9);
    }

    #[test]
    fn upsert_adds_distinct_grid_points() {
        let mut r = WeatherRegistry::default();
        let i = r.upsert(WeatherLocation {
            name: "berlin".into(),
            lat: 52.5,
            lon: 13.4,
            ..default_loc()
        });
        assert_eq!(i, 1);
        assert_eq!(r.locations().len(), 2);
    }

    #[test]
    fn area_link_returns_associated_location() {
        let mut r = WeatherRegistry::default();
        let i = r.upsert(WeatherLocation {
            name: "berlin".into(),
            lat: 52.5,
            lon: 13.4,
            cloud_cover: 0.9,
            ..default_loc()
        });
        r.link_area("10YDE-VE-------2", i);
        assert_eq!(r.for_area("10YDE-VE-------2").cloud_cover, 0.9);
        // Unlinked area falls back to the default.
        assert_eq!(r.for_area("10YFR-RTE------C").cloud_cover, 0.30);
    }

    #[test]
    fn at_latlon_snaps_to_nearest_tenth() {
        let mut r = WeatherRegistry::default();
        r.upsert(WeatherLocation {
            name: "munich".into(),
            lat: 48.1,
            lon: 11.6,
            cloud_cover: 0.55,
            ..default_loc()
        });
        // 48.137 / 11.575 round to 48.1 / 11.6 → registered slot.
        assert_eq!(r.at_latlon(48.137, 11.575).cloud_cover, 0.55);
        // Far-away lookup falls back to the default.
        assert_eq!(r.at_latlon(60.0, 25.0).cloud_cover, 0.30);
    }

    // Summer + winter solstices for the solar-elevation tests.
    const SUMMER_SOLSTICE_DOY: u32 = 172; // ~21 June
    const WINTER_SOLSTICE_DOY: u32 = 355; // ~21 December

    #[test]
    fn solar_zero_overnight() {
        let l = default_loc();
        // 0:00 UTC and 22:00 UTC at Frankfurt are both below the
        // horizon on the summer solstice; outside daylight on
        // winter solstice too.
        assert_eq!(l.solar_at(0.0, SUMMER_SOLSTICE_DOY), 0.0);
        assert_eq!(l.solar_at(22.0, SUMMER_SOLSTICE_DOY), 0.0);
        assert_eq!(l.solar_at(0.0, WINTER_SOLSTICE_DOY), 0.0);
    }

    #[test]
    fn solar_peaks_at_noon() {
        // Frankfurt at summer solstice noon, clear sky: elevation
        // ~63°, air mass ~1.12, clear-sky peak ~810 W/m². Wide
        // tolerance — physics-level sanity, not a calibration.
        let l = WeatherLocation {
            cloud_cover: 0.0,
            ..default_loc()
        };
        let peak = l.solar_at(12.0, SUMMER_SOLSTICE_DOY);
        assert!(
            (700.0..900.0).contains(&peak),
            "noon peak {peak:.1} should be ~810 W/m²"
        );
    }

    #[test]
    fn solar_winter_lower_than_summer() {
        // Same location + hour, opposite season: winter elevation
        // is ~16° (vs ~63° in summer) so the air-mass path
        // dominates and the surface irradiance is much smaller.
        let l = WeatherLocation {
            cloud_cover: 0.0,
            ..default_loc()
        };
        let summer = l.solar_at(12.0, SUMMER_SOLSTICE_DOY);
        let winter = l.solar_at(12.0, WINTER_SOLSTICE_DOY);
        assert!(
            winter < 0.3 * summer,
            "winter {winter:.1} should be < 30% of summer {summer:.1}"
        );
    }

    #[test]
    fn temperature_diurnal_cycle() {
        let l = default_loc();
        let warmest = l.temperature_at(14.0);
        let coldest = l.temperature_at(2.0);
        assert!((warmest - 298.0).abs() < 1e-9);
        assert!((coldest - 282.0).abs() < 1e-9);
    }

    #[test]
    fn cloud_attenuates_solar() {
        let clear = WeatherLocation {
            cloud_cover: 0.0,
            ..default_loc()
        };
        let overcast = WeatherLocation {
            cloud_cover: 0.8,
            ..default_loc()
        };
        // Linear (1 - cloud) scaling, so overcast should be 20%
        // of clear at any hour / latitude / date.
        let c = clear.solar_at(12.0, SUMMER_SOLSTICE_DOY);
        let o = overcast.solar_at(12.0, SUMMER_SOLSTICE_DOY);
        assert!((o - 0.2 * c).abs() < 1e-6);
    }
}
