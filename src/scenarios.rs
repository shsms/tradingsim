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

use chrono::{DateTime, TimeZone, Timelike, Utc};
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
}

#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
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
            let scenario_bias = pick_active_bias(&scenarios, now);
            let scale = *bias_scale.read();
            let curve_snap = curve.read().clone();
            let weather_snap = weather.read().clone();
            apply_biases(
                &aggressors,
                &mms,
                scenario_bias,
                scale,
                &curve_snap,
                &weather_snap,
                now,
            );
        }
    })
}

/// Look at the registry, auto-advance any running scenario that's
/// still in auto mode, and return the current scenario-stage bias
/// (if any). Today's contract is "one scenario active at a time"; if
/// multiple are running, the first by HashMap iteration wins.
fn pick_active_bias(scenarios: &SharedScenarios, now: DateTime<Utc>) -> Option<f64> {
    let mut guard = scenarios.lock();
    let wallclock_h = wallclock_hour(now);
    let mut active: Option<f64> = None;
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
        if active.is_none()
            && let Some(idx) = entry.runtime.current_stage
            && let Some(stage) = entry.def.stages.get(idx)
        {
            active = Some(stage_bias_now(stage, &entry.runtime, now));
        }
    }
    active
}

fn apply_biases(
    aggressors: &[AggressorView],
    mms: &[MmView],
    scenario_bias: Option<f64>,
    bias_scale: f64,
    curve: &ForwardCurve,
    weather: &WeatherRegistry,
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
        let new_ref = effective_ref(curve, loc, period_hour_for(view.quarter_offset));
        let mut cfg = view.shared_config.write();
        cfg.reference_price = new_ref;
        cfg.demand = Decimal::try_from(shift).unwrap_or(Decimal::ZERO);
        cfg.surplus = Decimal::try_from(-shift).unwrap_or(Decimal::ZERO);
    }
}

/// Forward-curve base price for `hour`, adjusted by the supplied
/// weather location's solar irradiance (drops price), wind speed
/// (drops price), and heating-degree-hours (raises price). Each
/// adjustment uses the per-hour coefficient from the curve.
/// Snapped to the 0.01-EUR tick.
pub fn effective_ref(curve: &ForwardCurve, weather: &WeatherLocation, hour: f64) -> Decimal {
    let p = curve.point_at(hour);
    let solar_drop = p.solar_coef * (weather.solar_at(hour) - 200.0).max(0.0);
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
