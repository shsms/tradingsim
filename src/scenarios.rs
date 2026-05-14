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

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use rust_decimal::Decimal;

use crate::sim::clock::{Clock, SharedClock};
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
/// bias tracks local-civil-time progress through [hour_from, hour_to);
/// in manual mode it tracks elapsed wallclock seconds since the
/// operator clicked, scaled by the stage's duration. Stage hours
/// are user-specified in the configured timezone.
pub fn stage_bias_now(
    stage: &Stage,
    runtime: &ScenarioRuntime,
    clock: &Clock,
    now: DateTime<Utc>,
) -> f64 {
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
        let h = clock.local_hour(now);
        (h - stage.hour_from) / duration_hours
    };
    lerp(stage.bias_from, stage.bias_to, t.clamp(0.0, 1.0))
}

pub fn wallclock_stage(def: &ScenarioDef, hour: f64) -> Option<usize> {
    def.stages
        .iter()
        .position(|s| s.hour_from <= hour && hour < s.hour_to)
}

/// UTC datetime for *today's local civil date* at `hour` (a local
/// fractional hour). Used to seed `stage_entered_at` so the manual-
/// override interpolation has a wallclock anchor to measure elapsed
/// time against. DST spring-forward edge: `.earliest()` picks the
/// first valid instant if `hour` falls in the skipped window.
fn today_at(tz: Tz, hour: f64, now: DateTime<Utc>) -> DateTime<Utc> {
    let local_date = now.with_timezone(&tz).date_naive();
    let total_seconds = (hour * 3600.0) as i64;
    let h = ((total_seconds / 3600).rem_euclid(24)) as u32;
    let m = ((total_seconds % 3600) / 60).rem_euclid(60) as u32;
    let s = (total_seconds % 60).rem_euclid(60) as u32;
    let naive = local_date.and_hms_opt(h, m, s).unwrap_or_default();
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now)
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
    clock: SharedClock,
    cadence: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cadence);
        loop {
            tick.tick().await;
            let now = Utc::now();
            // Snapshot the clock once per tick — Tz is Copy and
            // Clock is tiny, so a quick clone is cheaper than
            // holding the read guard across the body.
            let clock_snap = clock.read().clone();
            let active = pick_active_stage(&scenarios, &clock_snap, now);
            let scale = *bias_scale.read();
            let curve_snap = curve.read().clone();

            // Calendar context for the solar-elevation model. If
            // the active scenario carries a :date, that day-of-
            // year drives both MM pricing here and the gRPC
            // weather forecast — keeping the simulated atmosphere
            // consistent with what a trading app's subscribed
            // weather stream is being told. Wallclock-day fallback
            // uses *local* date, so a scenario starting at 23:30
            // local doesn't silently roll into the next ordinal
            // because UTC has already crossed midnight.
            let scenario_date = active.as_ref().and_then(|(_, _, d)| *d);
            let day_of_year = scenario_date
                .unwrap_or_else(|| clock_snap.local_date(now))
                .ordinal();

            // Reset every registered location to its baseline and
            // apply the active stage's weather overrides on top.
            // Doing this under the same lock both writers
            // (the gRPC weather service + MM tick) read from means
            // a stage transition shows up atomically downstream.
            {
                let mut w = weather.write();
                w.active_day_of_year = Some(day_of_year);
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
            let scenario_bias = active
                .as_ref()
                .map(|(s, rt, _)| stage_bias_now(s, rt, &clock_snap, now));
            apply_biases(
                &aggressors,
                &mms,
                scenario_bias,
                scale,
                &curve_snap,
                &weather_snap,
                &clock_snap,
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
    clock: &Clock,
    now: DateTime<Utc>,
) -> Option<(Stage, ScenarioRuntime, Option<NaiveDate>)> {
    let mut guard = scenarios.lock();
    let local_h = clock.local_hour(now);
    for entry in guard.values_mut() {
        if entry.runtime.current_stage.is_none() {
            continue;
        }
        if !entry.runtime.manual_override
            && let Some(idx) = wallclock_stage(&entry.def, local_h)
            && entry.runtime.current_stage != Some(idx)
        {
            entry.runtime.current_stage = Some(idx);
            entry.runtime.stage_entered_at = Some(today_at(
                clock.tz,
                entry.def.stages[idx].hour_from,
                now,
            ));
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
    clock: &Clock,
    day_of_year: u32,
    now: DateTime<Utc>,
) {
    let base_boundary = next_quarter_boundary(now);
    let period_hour_for = |offset: i64| -> f64 {
        let period_start = base_boundary + chrono::Duration::minutes(15 * offset);
        clock.local_hour(period_start)
    };
    let effective_for = |offset: i64| -> f64 {
        let natural = natural_duck_bias(period_hour_for(offset));
        match scenario_bias {
            Some(stage_bias) => lerp(natural, stage_bias, decay_weight(offset)),
            None => natural,
        }
        .clamp(0.0, 1.0)
    };
    // Shared prelude for both loops: per-view area lookup +
    // curve+weather-derived fundamentals reference at this quarter.
    // Weather falls back to the registry's default location when
    // the area isn't explicitly linked.
    let fundamentals = |area_code: &str, offset: i64| -> Decimal {
        let loc = weather.for_area(area_code);
        effective_ref(curve, loc, period_hour_for(offset), day_of_year)
    };
    for view in aggressors {
        let area_code = view.shared_config.read().area.code.clone();
        let new_ref = fundamentals(&area_code, view.quarter_offset);
        let mut cfg = view.shared_config.write();
        cfg.side_bias = effective_for(view.quarter_offset);
        // Same fair-value anchor the MM uses for its baseline.
        // Without this, the aggressor's slippage clamp would
        // still be measuring off the stale 85 EUR boot seed and
        // miss real curve / weather drift.
        cfg.reference_price = new_ref;
    }
    for view in mms {
        let bias = effective_for(view.quarter_offset);
        let shift = (bias - 0.5) * bias_scale;
        let area_code = view.shared_config.read().area.code.clone();
        let new_ref = fundamentals(&area_code, view.quarter_offset);
        let mut cfg = view.shared_config.write();
        // Write the fundamentals target — the MM refresh
        // (step_reference) mean-reverts the live reference_price
        // toward this every 2 s. Bypassing the live field is
        // deliberate: each refresh between bias ticks now gets to
        // accumulate the follow-last-trade pull + random walk
        // instead of being snapped back to the curve value every
        // 5 s. Scenario stage transitions still propagate quickly —
        // the mean-revert pulls visibly within a few refreshes.
        cfg.reference_baseline = new_ref;
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

use crate::lisp::next_quarter_boundary;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::counterparty::{AggressorConfig, MarketMakerConfig};
    use crate::sim::market::{DeliveryDuration, DeliveryPeriod};
    use crate::sim::weather::{WeatherLocation, new_state};
    use chrono::Timelike;
    use rust_decimal::dec;

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

    fn stage_with_bias(h_from: f64, h_to: f64, bias_from: f64, bias_to: f64) -> Stage {
        Stage {
            name: "test".into(),
            hour_from: h_from,
            hour_to: h_to,
            bias_from,
            bias_to,
            cloud_cover: None,
            mean_wind: None,
            temperature_base: None,
        }
    }

    fn def_with_stages(stages: Vec<Stage>) -> ScenarioDef {
        ScenarioDef {
            name: "test".into(),
            description: String::new(),
            date: None,
            stages,
        }
    }

    #[test]
    fn natural_duck_bias_known_hours() {
        // Overnight: balanced. Morning ramp: rising. Solar belly:
        // bid-heavy (cheap power, lots of selling). Evening peak:
        // buy-heavy.
        assert!((natural_duck_bias(2.0) - 0.50).abs() < 1e-9);
        assert!((natural_duck_bias(5.0) - 0.50).abs() < 1e-9);
        assert!((natural_duck_bias(7.0) - 0.56).abs() < 0.01);
        assert!((natural_duck_bias(9.5) - 0.55).abs() < 1e-9);
        assert!((natural_duck_bias(12.0) - 0.35).abs() < 1e-9);
        assert!((natural_duck_bias(18.0) - 0.72).abs() < 1e-9);
        assert!((natural_duck_bias(22.0) - 0.60).abs() < 1e-9);
    }

    #[test]
    fn natural_duck_bias_wraps_negative_hours() {
        // rem_euclid handles negatives — wallclock_hour never goes
        // negative in practice, but be defensive.
        assert_eq!(natural_duck_bias(-23.0), natural_duck_bias(1.0));
        assert_eq!(natural_duck_bias(25.0), natural_duck_bias(1.0));
    }

    #[test]
    fn decay_weight_matches_design_curve() {
        // q0: full weight, q12 (3h): ~37%, q24 (6h): ~14%,
        // q47 (~12h): ~2%, q-N (impossible but guarded): 1.0.
        assert!((decay_weight(0) - 1.0).abs() < 1e-9);
        assert!((decay_weight(12) - 0.367).abs() < 0.01);
        assert!((decay_weight(24) - 0.135).abs() < 0.01);
        assert!((decay_weight(47) - 0.020).abs() < 0.01);
        assert_eq!(decay_weight(-5), 1.0);
    }

    #[test]
    fn lerp_endpoints_and_midpoint() {
        assert_eq!(lerp(0.0, 10.0, 0.0), 0.0);
        assert_eq!(lerp(0.0, 10.0, 1.0), 10.0);
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
    }

    #[test]
    fn wallclock_stage_finds_containing_stage() {
        let def = def_with_stages(vec![
            stage("a", 0.0, 6.0, None),
            stage("b", 6.0, 12.0, None),
            stage("c", 12.0, 24.0, None),
        ]);
        assert_eq!(wallclock_stage(&def, 0.0), Some(0));
        assert_eq!(wallclock_stage(&def, 5.999), Some(0));
        assert_eq!(wallclock_stage(&def, 6.0), Some(1));
        assert_eq!(wallclock_stage(&def, 23.9), Some(2));
    }

    #[test]
    fn wallclock_stage_returns_none_outside_any_stage() {
        let def = def_with_stages(vec![stage("morning", 6.0, 9.0, None)]);
        assert_eq!(wallclock_stage(&def, 5.0), None);
        assert_eq!(wallclock_stage(&def, 9.0), None);
    }

    #[test]
    fn stage_bias_now_auto_interpolates_across_window() {
        let s = stage_with_bias(10.0, 14.0, 0.30, 0.70);
        let rt = ScenarioRuntime {
            current_stage: Some(0),
            manual_override: false,
            ..Default::default()
        };
        let clock = Clock::default();
        // 10:00 utc = 12:00 CEST in May → t = 0.5 → 0.50 bias
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();
        let bias = stage_bias_now(&s, &rt, &clock, now);
        assert!((bias - 0.50).abs() < 1e-6);
    }

    #[test]
    fn stage_bias_now_manual_uses_elapsed_time_against_entered() {
        let s = stage_with_bias(10.0, 14.0, 0.30, 0.70);
        let entered = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();
        let rt = ScenarioRuntime {
            current_stage: Some(0),
            manual_override: true,
            stage_entered_at: Some(entered),
            ..Default::default()
        };
        let clock = Clock::default();
        // Halfway through a 4-hour stage = +2h elapsed → 0.5 → 0.50
        let now = entered + chrono::Duration::hours(2);
        let bias = stage_bias_now(&s, &rt, &clock, now);
        assert!((bias - 0.50).abs() < 1e-6);
    }

    #[test]
    fn stage_bias_now_clamps_outside_window() {
        let s = stage_with_bias(10.0, 14.0, 0.30, 0.70);
        let rt = ScenarioRuntime {
            current_stage: Some(0),
            manual_override: false,
            ..Default::default()
        };
        let clock = Clock::default();
        // 06:00 UTC = 08:00 CEST → before the stage in local. t<0,
        // clamped to 0 → returns bias_from.
        let early = Utc.with_ymd_and_hms(2026, 5, 14, 6, 0, 0).unwrap();
        assert!((stage_bias_now(&s, &rt, &clock, early) - 0.30).abs() < 1e-6);
        // 16:00 UTC = 18:00 CEST → past the stage. Clamped to 1 →
        // bias_to.
        let late = Utc.with_ymd_and_hms(2026, 5, 14, 16, 0, 0).unwrap();
        assert!((stage_bias_now(&s, &rt, &clock, late) - 0.70).abs() < 1e-6);
    }

    #[test]
    fn next_quarter_boundary_rounds_up() {
        let t = Utc.with_ymd_and_hms(2026, 5, 14, 10, 7, 30).unwrap();
        let b = next_quarter_boundary(t);
        assert_eq!(b, Utc.with_ymd_and_hms(2026, 5, 14, 10, 15, 0).unwrap());
        // Boundary itself rolls to the *next* one — never returns
        // the current instant.
        let on = Utc.with_ymd_and_hms(2026, 5, 14, 10, 15, 0).unwrap();
        assert_eq!(
            next_quarter_boundary(on),
            Utc.with_ymd_and_hms(2026, 5, 14, 10, 30, 0).unwrap()
        );
    }

    #[test]
    fn today_at_builds_utc_from_local_hour_in_dst() {
        // May → Europe/Berlin is CEST (UTC+2). Local 13:00 → UTC 11:00.
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let tz = chrono_tz::Europe::Berlin;
        let result = today_at(tz, 13.0, now);
        assert_eq!(result.hour(), 11);
        assert_eq!(result.minute(), 0);
    }

    #[test]
    fn today_at_builds_utc_from_local_hour_in_winter() {
        // Jan → CET (UTC+1). Local 13:00 → UTC 12:00.
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let tz = chrono_tz::Europe::Berlin;
        let result = today_at(tz, 13.0, now);
        assert_eq!(result.hour(), 12);
    }

    #[test]
    fn apply_biases_writes_per_quarter_reference_baseline() {
        // One MM at q0, one MM at q8 (2 h later). Bias tick should
        // write a curve+weather-derived baseline that differs
        // between them (different local hour ⇒ different curve
        // value).
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();
        let make_mm = |quarter_offset: i64| -> MmView {
            let cfg = MarketMakerConfig {
                area: crate::sim::market::Area::eic("10YDE-EON------1"),
                ..MarketMakerConfig::default_for(
                    crate::sim::market::Area::eic("10YDE-EON------1"),
                    DeliveryPeriod {
                        start: next_quarter_boundary(now)
                            + chrono::Duration::minutes(15 * quarter_offset),
                        duration: DeliveryDuration::DeliveryDuration15,
                    },
                )
            };
            MmView {
                quarter_offset,
                shared_config: Arc::new(RwLock::new(cfg)),
            }
        };
        let q0 = make_mm(0);
        let q8 = make_mm(8); // 2 hours later

        let curve = ForwardCurve::default();
        let weather = WeatherRegistry::default();
        let clock = Clock::default();
        apply_biases(
            &[],
            &[q0.clone(), q8.clone()],
            None,
            25.0,
            &curve,
            &weather,
            &clock,
            134, /* day_of_year */
            now,
        );
        let ref_q0 = q0.shared_config.read().reference_baseline;
        let ref_q8 = q8.shared_config.read().reference_baseline;
        // Different quarters → curve hits different forward hours →
        // baselines diverge.
        assert_ne!(ref_q0, ref_q8);
    }

    #[test]
    fn apply_biases_with_lopsided_scenario_writes_demand_or_surplus() {
        // Strong buy-side bias should push the q0 MM's demand
        // positive and surplus negative (mirrored).
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();
        let cfg = MarketMakerConfig::default_for(
            crate::sim::market::Area::eic("10YDE-EON------1"),
            DeliveryPeriod {
                start: next_quarter_boundary(now),
                duration: DeliveryDuration::DeliveryDuration15,
            },
        );
        let view = MmView {
            quarter_offset: 0,
            shared_config: Arc::new(RwLock::new(cfg)),
        };
        apply_biases(
            &[],
            &[view.clone()],
            Some(0.90), // strong buy bias
            25.0,
            &ForwardCurve::default(),
            &WeatherRegistry::default(),
            &Clock::default(),
            134,
            now,
        );
        let cfg = view.shared_config.read();
        assert!(cfg.demand > dec!(0));
        assert!(cfg.surplus < dec!(0));
        // Mirror within rounding.
        assert!((cfg.demand + cfg.surplus).abs() < dec!(0.001));
    }

    #[test]
    fn apply_biases_writes_aggressor_side_bias() {
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 10, 0, 0).unwrap();
        let ag_cfg = AggressorConfig::default_for(
            crate::sim::market::Area::eic("10YDE-EON------1"),
            DeliveryPeriod {
                start: next_quarter_boundary(now),
                duration: DeliveryDuration::DeliveryDuration15,
            },
        );
        let view = AggressorView {
            quarter_offset: 0,
            shared_config: Arc::new(RwLock::new(ag_cfg)),
        };
        apply_biases(
            &[view.clone()],
            &[],
            Some(0.80),
            25.0,
            &ForwardCurve::default(),
            &WeatherRegistry::default(),
            &Clock::default(),
            134,
            now,
        );
        // With q0 and a 0.80 stage bias, decay_weight is 1 so the
        // aggressor's side_bias should land near 0.80 — clamped to
        // [0,1] in any case.
        let sb = view.shared_config.read().side_bias;
        assert!(sb > 0.6 && sb <= 1.0, "side_bias {sb}");
    }

    #[test]
    fn pick_active_stage_returns_running_stage() {
        let scenarios = new_registry();
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        // Local 14:00 CEST → inside the 12–24 stage.
        scenarios.lock().insert(
            "alpha".into(),
            ScenarioEntry {
                def: def_with_stages(vec![
                    stage("a", 0.0, 12.0, None),
                    stage("b", 12.0, 24.0, None),
                ]),
                runtime: ScenarioRuntime {
                    current_stage: Some(1),
                    manual_override: true, // skip auto-advance
                    ..Default::default()
                },
            },
        );
        let active = pick_active_stage(&scenarios, &Clock::default(), now);
        let (stage, _, _) = active.expect("active stage");
        assert_eq!(stage.name, "b");
    }

    #[test]
    fn pick_active_stage_auto_advances_when_wallclock_moves() {
        let scenarios = new_registry();
        // Local 14:00 CEST (= UTC 12) is inside the 12–24 stage,
        // but the runtime still points at stage 0. Auto-advance
        // should bump it to 1 and update stage_entered_at.
        scenarios.lock().insert(
            "alpha".into(),
            ScenarioEntry {
                def: def_with_stages(vec![
                    stage("a", 0.0, 12.0, None),
                    stage("b", 12.0, 24.0, None),
                ]),
                runtime: ScenarioRuntime {
                    current_stage: Some(0),
                    manual_override: false,
                    ..Default::default()
                },
            },
        );
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        pick_active_stage(&scenarios, &Clock::default(), now);
        let g = scenarios.lock();
        let rt = &g.get("alpha").unwrap().runtime;
        assert_eq!(rt.current_stage, Some(1));
        assert!(rt.stage_entered_at.is_some());
    }

    #[test]
    fn pick_active_stage_respects_manual_override() {
        let scenarios = new_registry();
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        scenarios.lock().insert(
            "alpha".into(),
            ScenarioEntry {
                def: def_with_stages(vec![
                    stage("a", 0.0, 12.0, None),
                    stage("b", 12.0, 24.0, None),
                ]),
                runtime: ScenarioRuntime {
                    current_stage: Some(0),
                    manual_override: true,
                    ..Default::default()
                },
            },
        );
        pick_active_stage(&scenarios, &Clock::default(), now);
        let g = scenarios.lock();
        // Manual override keeps the runtime stage even though
        // wallclock would auto-advance to 1.
        assert_eq!(g.get("alpha").unwrap().runtime.current_stage, Some(0));
    }

    #[test]
    fn pick_active_stage_returns_none_when_no_scenario_running() {
        let scenarios = new_registry();
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        scenarios.lock().insert(
            "alpha".into(),
            ScenarioEntry {
                def: def_with_stages(vec![stage("a", 0.0, 24.0, None)]),
                runtime: ScenarioRuntime::default(),
            },
        );
        let active = pick_active_stage(&scenarios, &Clock::default(), now);
        assert!(active.is_none());
    }

    #[test]
    fn stage_weather_overrides_apply_then_restore() {
        // Mirror the production registry shape: one default + one
        // per-area location with distinct baselines.
        let weather = new_state();
        {
            let mut reg = weather.write();
            *reg.default_mut() = WeatherLocation::default_for_tests();
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
