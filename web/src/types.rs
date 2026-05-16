//! Wire types the Leptos shell deserializes from the host server's
//! /api/* endpoints. Shapes mirror `src/ui/mod.rs`'s response structs
//! — kept in sync by hand for now; a shared `tradingsim-api` crate
//! is the natural extraction once the port settles.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct InfoResp {
    pub version: String,
    pub gridpools: usize,
    pub markets: usize,
    pub couplings: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClockResp {
    /// IANA timezone the sim runs in (e.g. `Europe/Berlin`). The
    /// header's "local" / UTC toggle keys off this so a remote
    /// operator still sees the simulator's home zone.
    pub tz: String,
}

/// One weather location row off /api/weather. `area_code` is None
/// for the unlinked fallback location; the panel filters those out
/// because every configured area carries its own location in the
/// shipping config.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WeatherLoc {
    pub name: String,
    pub area_code: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub cloud_cover: f64,
    pub mean_wind: f64,
    pub wind_direction: f64,
    pub solar_now: f64,
    pub wind_now: f64,
    pub temp_c_now: f64,
}

/// One stage of a scenario timeline. Hours are sim-local (the host
/// interprets them through the configured clock); biases drive the
/// MM bias-tick. Weather overrides are None when the stage leaves
/// the area's baseline alone.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Stage {
    pub name: String,
    pub hour_from: f64,
    pub hour_to: f64,
    pub bias_from: f64,
    pub bias_to: f64,
    pub cloud_cover: Option<f64>,
    pub mean_wind: Option<f64>,
    pub temperature_base: Option<f64>,
}

/// One registered scenario, with its current runtime state. `current_stage`
/// is None when idle; the wallclock_stage hint marks where auto-advance
/// would land.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub stages: Vec<Stage>,
    pub current_stage: Option<usize>,
    pub wallclock_stage: Option<usize>,
    pub manual_override: bool,
    pub started_at: Option<String>,
    pub stage_entered_at: Option<String>,
}

/// One print off the /ws/public-trades broadcast. Prices + quantities
/// arrive as the host's `Decimal::to_string()` output; parsed lazily
/// only when a panel needs the numeric value. Fields beyond `id` /
/// `price` are unused until the trades / chart panels port — keep
/// them deserialized so the wire shape stays a single source of
/// truth.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PublicTrade {
    pub id: u64,
    pub buy_area: String,
    pub sell_area: String,
    pub period: String,
    pub price: String,
    pub quantity: String,
    pub execution_time: String,
}
