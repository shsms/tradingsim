//! Scenario registry + bias-application tick.
//!
//! Two parts:
//!
//! - The lisp side populates a registry of named time-of-day
//!   scenarios via `(define-scenario …)`. The UI layer reads + mutates
//!   the runtime state (current stage, manual override) through HTTP
//!   endpoints.
//! - A background tick task ([`spawn_bias_tick`]) iterates every
//!   aggressor every few seconds and writes its `side_bias` field.
//!   Default behaviour is the natural duck curve (bias varies by the
//!   contract's hour-of-day); when a scenario is active, the bias for
//!   imminent quarters is pulled toward the scenario's current-stage
//!   bias, fading back to natural over ~3 hours of delivery offset.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use rust_decimal::Decimal;

use crate::sim::counterparty::{SharedAggressorConfig, SharedConfig};
use crate::sim::curve::ForwardCurve;
use crate::sim::weather::{SharedWeather, WeatherLocation, WeatherRegistry};

#[derive(Clone, Debug)]
pub struct Stage {
    /// Display name shown in the UI ("06:00 morning ramp").
    pub name: String,
    /// Wallclock UTC hour window this stage represents.
    pub hour_from: f64,
    pub hour_to: f64,
    /// Aggressor side-bias at the start and end of the window.
    /// The tick interpolates linearly between them as the wallclock
    /// advances through [hour_from, hour_to).
    pub bias_from: f64,
    pub bias_to: f64,
    /// Optional weather overrides for this stage. `None` leaves the
    /// area's existing value alone; `Some(v)` replaces it on the
    /// default weather location while the stage is current. Drives
    /// the realism of canned scenarios: a "rainy summer" stage can
    /// push cloud cover up so solar drops and the MM reference
    /// shifts the same way a real overcast hour would.
    pub cloud_cover: Option<f64>,
    pub mean_wind: Option<f64>,
    pub temperature_base: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    /// Optional calendar date the solar-elevation model treats this
    /// scenario as taking place on. None falls back to wallclock-
    /// today. Setting :date "2026-06-21" on a sunny-summer scenario
    /// pins the day-of-year to summer solstice so peak irradiance
    /// matches the scenario name no matter when it's run.
    pub date: Option<NaiveDate>,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScenarioRuntime {
    /// `None` = not running; `Some(i)` = stage `i` is current.
    pub current_stage: Option<usize>,
    pub started_at: Option<DateTime<Utc>>,
    pub stage_entered_at: Option<DateTime<Utc>>,
    /// True when the operator jumped away from the wallclock-current
    /// stage. The tick task respects this and stops auto-advancing
    /// until the operator returns to the wallclock-matching stage
    /// or restarts the scenario.
    pub manual_override: bool,
}

#[derive(Clone, Debug)]
pub struct ScenarioEntry {
    pub def: ScenarioDef,
    pub runtime: ScenarioRuntime,
}

pub type SharedScenarios = Arc<Mutex<HashMap<String, ScenarioEntry>>>;

pub fn new_registry() -> SharedScenarios {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Bias curves — the natural duck curve, the per-quarter decay, and the
// stage-bias-now interpolation. Plain pure functions so they're trivial
// to unit-test and to call from the tick task.
// ---------------------------------------------------------------------------

/// Aggressor side-bias for a given UTC hour-of-day under the natural
/// (no-scenario) duck curve. Returns values in [0, 1].
pub fn natural_duck_bias(hour: f64) -> f64 {
    let h = hour.rem_euclid(24.0);
    match h {
        h if h < 5.0 => 0.50,                              // overnight
        h if h < 9.0 => lerp(0.50, 0.62, (h - 5.0) / 4.0), // morning ramp
        h if h < 10.0 => 0.55,                             // post-peak
        h if h < 15.0 => 0.35,                             // solar belly
        h if h < 17.0 => 0.50,                             // transition
        h if h < 21.0 => 0.72,                             // evening peak
        h if h < 23.0 => 0.60,
        _ => 0.50,
    }
}

/// How strongly a scenario's bias override applies at quarter-offset
/// `i`. q0 = 1.0, decays as `exp(-i/12)` so q12 (3 h out) ~= 0.37 and
/// q47 (12 h out) ~= 0.02.
pub fn decay_weight(offset: i64) -> f64 {
    (-(offset.max(0) as f64) / 12.0).exp()
}

pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Interpolated bias for a scenario's current stage, given the runtime
/// state and wallclock. In auto mode (`manual_override == false`) the
/// bias tracks wallclock progress through [hour_from, hour_to). In
/// manual mode it tracks elapsed time since the operator clicked,
/// scaled by the stage's duration.
pub fn stage_bias_now(stage: &Stage, runtime: &ScenarioRuntime, now: DateTime<Utc>) -> f64 {
    let duration_hours = (stage.hour_to - stage.hour_from).max(0.001);
    let t = if runtime.manual_override {
        runtime
            .stage_entered_at
            .map(|entered| {
                let elapsed_secs = (now - entered).num_seconds() as f64;
                elapsed_secs / (duration_hours * 3600.0)
            })
            .unwrap_or(0.0)
    } else {
        let h = wallclock_hour(now);
        (h - stage.hour_from) / duration_hours
    };
    lerp(stage.bias_from, stage.bias_to, t.clamp(0.0, 1.0))
}

pub fn wallclock_hour(now: DateTime<Utc>) -> f64 {
    now.hour() as f64 + now.minute() as f64 / 60.0 + now.second() as f64 / 3600.0
}

pub fn wallclock_stage(def: &ScenarioDef, hour: f64) -> Option<usize> {
    def.stages
        .iter()
        .position(|s| s.hour_from <= hour && hour < s.hour_to)
}

/// UTC datetime for today's start of the given fractional hour.
fn today_at(hour: f64, now: DateTime<Utc>) -> DateTime<Utc> {
    let total_seconds = (hour * 3600.0) as i64;
    let date = now.date_naive();
    let h = (total_seconds / 3600) as u32;
    let m = ((total_seconds % 3600) / 60) as u32;
    let s = (total_seconds % 60) as u32;
    Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Bias-application tick.
// ---------------------------------------------------------------------------

/// One aggressor's view for the tick: just the offset and shared
/// config. Cloned cheaply (Arc) so the tick doesn't hold the
/// registry lock while writing biases.
#[derive(Clone)]
pub struct AggressorView {
    pub quarter_offset: i64,
    pub shared_config: SharedAggressorConfig,
}

/// Same shape as [`AggressorView`] but holding a market-maker's
/// SharedConfig. The tick translates the effective bias into MM
/// demand + surplus tilts so the quoted bid/ask shift symmetrically
/// — the mechanism prices use to move quickly under load.
#[derive(Clone)]
pub struct MmView {
    pub quarter_offset: i64,
    pub shared_config: SharedConfig,
}

/// Fallback bias scale used when `config.lisp` doesn't call
/// `(set-mm-bias-scale …)`. EUR per (bias - 0.5) unit added to the
/// MM's demand / surplus tilt. With scale = 25 a deep-belly stage
/// (bias 0.18, imbalance -0.32) becomes an 8-EUR shift on both
/// bid and ask, enough to push prices into negative territory
/// within ~3 minutes.
pub const DEFAULT_MM_BIAS_SCALE: f64 = 25.0;

pub type SharedBiasScale = Arc<RwLock<f64>>;

pub fn new_bias_scale() -> SharedBiasScale {
    Arc::new(RwLock::new(DEFAULT_MM_BIAS_SCALE))
}

/// Shared forward curve. Lisp can mutate via `(set-forward-curve-base …)`;
/// the bias tick reads it each cycle.
pub type SharedCurve = Arc<RwLock<ForwardCurve>>;

pub fn new_curve() -> SharedCurve {
    Arc::new(RwLock::new(ForwardCurve::default()))
}

/// Spawn a tokio task that, every `cadence`, walks `aggressors` and
/// `mms` and writes their effective side-bias / demand / surplus to
/// the SharedConfigs. The scenarios registry is consulted on each
/// tick: a running scenario's current stage gets auto-advanced to
/// the wallclock-matching stage (unless the operator has set
/// `manual_override`), and its stage bias is blended into the
/// per-contract natural-curve bias by the quarter-offset decay
/// weight.
pub fn spawn_bias_tick(
    aggressors: Vec<AggressorView>,
    mms: Vec<MmView>,
    scenarios: SharedScenarios,
    bias_scale: SharedBiasScale,
    curve: SharedCurve,
    weather: SharedWeather,
    cadence: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cadence);
        loop {
            tick.tick().await;
            let now = Utc::now();
            let active = pick_active_stage(&scenarios, now);
            let scale = *bias_scale.read();
            let curve_snap = curve.read().clone();

            // Calendar context for the solar-elevation model. If
            // the active scenario carries a :date, that day-of-
            // year drives both MM pricing here and the gRPC
            // weather forecast — keeping the simulated atmosphere
            // consistent with what a trading app's subscribed
            // weather stream is being told.
            let scenario_date = active.as_ref().and_then(|(_, _, d)| *d);
            let day_of_year = scenario_date
                .unwrap_or_else(|| now.date_naive())
                .ordinal();

            // Reset every registered location to its baseline and
            // apply the active stage's weather overrides on top.
            // Doing this under the same lock both writers
            // (the gRPC weather service + MM tick) read from means
            // a stage transition shows up atomically downstream.
            {
                let mut w = weather.write();
                w.active_day_of_year = Some(day_of_year);
                w.active_hour = active.as_ref().map(|(s, _, _)| {
                    // Midpoint of the stage's hour range. The
                    // weather panel reads this for its "now"
                    // snapshot so picking a midday stage shows
                    // midday solar even at 2 AM wallclock. The
                    // stage timeline keeps using wallclock — only
                    // the displayed weather follows the stage.
                    0.5 * (s.hour_from + s.hour_to)
                });
                for loc in w.locations_mut() {
                    loc.reset_to_baseline();
                }
                if let Some((stage, _, _)) = &active {
                    for loc in w.locations_mut() {
                        if let Some(v) = stage.cloud_cover {
                            loc.cloud_cover = v;
                        }
                        if let Some(v) = stage.mean_wind {
                            loc.mean_wind = v;
                        }
                        if let Some(v) = stage.temperature_base {
                            loc.temperature_base = v;
                        }
                    }
                }
            }

            let weather_snap = weather.read().clone();
            let scenario_bias = active.as_ref().map(|(s, rt, _)| stage_bias_now(s, rt, now));
            apply_biases(
                &aggressors,
                &mms,
                scenario_bias,
                scale,
                &curve_snap,
                &weather_snap,
                day_of_year,
                now,
            );
        }
    })
}

/// Auto-advance any running scenario whose wallclock has moved into
/// a different stage, then return a clone of the currently active
/// stage plus its runtime + the parent scenario's optional :date.
/// Multiple running scenarios collapse to the first one HashMap
/// iteration yields.
fn pick_active_stage(
    scenarios: &SharedScenarios,
    now: DateTime<Utc>,
) -> Option<(Stage, ScenarioRuntime, Option<NaiveDate>)> {
    let mut guard = scenarios.lock();
    let wallclock_h = wallclock_hour(now);
    for entry in guard.values_mut() {
        if entry.runtime.current_stage.is_none() {
            continue;
        }
        if !entry.runtime.manual_override
            && let Some(idx) = wallclock_stage(&entry.def, wallclock_h)
            && entry.runtime.current_stage != Some(idx)
        {
            entry.runtime.current_stage = Some(idx);
            entry.runtime.stage_entered_at = Some(today_at(entry.def.stages[idx].hour_from, now));
        }
        if let Some(idx) = entry.runtime.current_stage
            && let Some(stage) = entry.def.stages.get(idx)
        {
            return Some((stage.clone(), entry.runtime.clone(), entry.def.date));
        }
    }
    None
}

fn apply_biases(
    aggressors: &[AggressorView],
    mms: &[MmView],
    scenario_bias: Option<f64>,
    bias_scale: f64,
    curve: &ForwardCurve,
    weather: &WeatherRegistry,
    day_of_year: u32,
    now: DateTime<Utc>,
) {
    let base_boundary = next_quarter_boundary(now);
    let period_hour_for = |offset: i64| -> f64 {
        let period_start = base_boundary + chrono::Duration::minutes(15 * offset);
        wallclock_hour(period_start)
    };
    let effective_for = |offset: i64| -> f64 {
        let natural = natural_duck_bias(period_hour_for(offset));
        match scenario_bias {
            Some(stage_bias) => lerp(natural, stage_bias, decay_weight(offset)),
            None => natural,
        }
        .clamp(0.0, 1.0)
    };
    for view in aggressors {
        view.shared_config.write().side_bias = effective_for(view.quarter_offset);
    }
    for view in mms {
        let bias = effective_for(view.quarter_offset);
        let imbalance = bias - 0.5;
        let shift = imbalance * bias_scale;
        // Look up the weather location for this MM's area (falls
        // back to the registry's default when the area isn't
        // explicitly linked).
        let area_code = view.shared_config.read().area.code.clone();
        let loc = weather.for_area(&area_code);
        let new_ref = effective_ref(
            curve,
            loc,
            period_hour_for(view.quarter_offset),
            day_of_year,
        );
        let mut cfg = view.shared_config.write();
        cfg.reference_price = new_ref;
        cfg.demand = Decimal::try_from(shift).unwrap_or(Decimal::ZERO);
        cfg.surplus = Decimal::try_from(-shift).unwrap_or(Decimal::ZERO);
    }
}

/// Forward-curve base price for `hour` on `day_of_year` (1-366),
/// adjusted by the supplied weather location's solar irradiance
/// (drops price), wind speed (drops price), and heating-degree-
/// hours (raises price). Each adjustment uses the per-hour
/// coefficient from the curve. Snapped to the 0.01-EUR tick.
pub fn effective_ref(
    curve: &ForwardCurve,
    weather: &WeatherLocation,
    hour: f64,
    day_of_year: u32,
) -> Decimal {
    let p = curve.point_at(hour);
    let solar_drop = p.solar_coef * (weather.solar_at(hour, day_of_year) - 200.0).max(0.0);
    let wind_drop = p.wind_coef * (weather.wind_at(hour) - 5.0).max(0.0);
    let load_rise = p.load_coef * weather.heating_degree(hour);
    let net = p.base_price - solar_drop - wind_drop + load_rise;
    let cents = (net * 100.0).round() as i64;
    Decimal::new(cents, 2)
}

fn next_quarter_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let bucket = (secs / 900 + 1) * 900;
    DateTime::from_timestamp(bucket, 0).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::weather::{WeatherLocation, new_state};

    fn stage(name: &str, h_from: f64, h_to: f64, cloud: Option<f64>) -> Stage {
        Stage {
            name: name.into(),
            hour_from: h_from,
            hour_to: h_to,
            bias_from: 0.5,
            bias_to: 0.5,
            cloud_cover: cloud,
            mean_wind: None,
            temperature_base: None,
        }
    }

    #[test]
    fn stage_weather_overrides_apply_then_restore() {
        // Mirror the production registry shape: one default + one
        // per-area location with distinct baselines.
        let weather = new_state();
        {
            let mut reg = weather.write();
            *reg.default_mut() = WeatherLocation::de_lu_typical();
            let idx = reg.upsert(WeatherLocation {
                name: "tn".into(),
                lat: 50.4,
                lon: 11.6,
                cloud_cover: 0.35,
                mean_wind: 5.0,
                wind_direction: 270.0,
                temperature_base: 290.0,
                baseline_cloud_cover: 0.35,
                baseline_mean_wind: 5.0,
                baseline_temperature_base: 290.0,
            });
            reg.link_area("10YDE-EON------1", idx);
        }

        // Apply a stage with cloud_cover override directly (the
        // bias tick does this under a write lock).
        let s = stage("test", 0.0, 24.0, Some(0.95));
        {
            let mut reg = weather.write();
            for loc in reg.locations_mut() {
                loc.reset_to_baseline();
            }
            for loc in reg.locations_mut() {
                if let Some(v) = s.cloud_cover {
                    loc.cloud_cover = v;
                }
            }
            // Both default and TN now reflect the override.
            assert_eq!(reg.default_location().cloud_cover, 0.95);
            assert_eq!(reg.for_area("10YDE-EON------1").cloud_cover, 0.95);
            // Baselines untouched.
            assert_eq!(reg.default_location().baseline_cloud_cover, 0.30);
            assert_eq!(reg.for_area("10YDE-EON------1").baseline_cloud_cover, 0.35);
        }

        // Next tick with no active stage: only the reset runs.
        {
            let mut reg = weather.write();
            for loc in reg.locations_mut() {
                loc.reset_to_baseline();
            }
            assert_eq!(reg.default_location().cloud_cover, 0.30);
            assert_eq!(reg.for_area("10YDE-EON------1").cloud_cover, 0.35);
        }
    }
}
