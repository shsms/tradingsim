//! Frequenz Weather API service implementation.
//!
//! Surfaces the sim's internal `WeatherRegistry` as a
//! `frequenz.api.weather.v1.WeatherForecastService`. The live stream
//! emits, every minute, one `LocationForecast` per registered
//! weather location (each carrying 24 hourly forecast points with
//! horizon-scaled noise). Past emissions are kept in a bounded ring
//! buffer that the historical RPC replays from.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use parking_lot::Mutex;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::proto::weather::{
    ForecastFeature, LocationForecast, ReceiveHistoricalWeatherForecastRequest,
    ReceiveHistoricalWeatherForecastResponse, ReceiveLiveWeatherForecastRequest,
    ReceiveLiveWeatherForecastResponse,
    location_forecast::{Forecasts, forecasts::FeatureForecast},
    weather_forecast_service_server::WeatherForecastService,
};
use crate::sim::weather::{
    SharedWeather, SharedWeatherCadence, WeatherLocation, WeatherRegistry, new_cadence,
};

/// Cap on past emissions kept in the ring. 24 h × 60 min = 1440 if
/// we kept everything; 100 is plenty for typical client back-fills.
const HISTORY_CAP: usize = 100;

pub type SharedWeatherHistory = Arc<Mutex<VecDeque<LocationForecast>>>;

pub fn new_history() -> SharedWeatherHistory {
    Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY_CAP)))
}

pub struct WeatherForecastServer {
    weather: SharedWeather,
    history: SharedWeatherHistory,
    cadence: SharedWeatherCadence,
}

impl WeatherForecastServer {
    pub fn new(weather: SharedWeather) -> Self {
        Self {
            weather,
            history: new_history(),
            cadence: new_cadence(),
        }
    }

    /// Replace the default 1 h forecast cadence with a shared
    /// handle the lisp layer can mutate at runtime via
    /// `(set-weather-stream-cadence-seconds N)`.
    pub fn with_cadence(mut self, cadence: SharedWeatherCadence) -> Self {
        self.cadence = cadence;
        self
    }
}

type LiveStream =
    Pin<Box<dyn Stream<Item = Result<ReceiveLiveWeatherForecastResponse, Status>> + Send>>;
type HistStream =
    Pin<Box<dyn Stream<Item = Result<ReceiveHistoricalWeatherForecastResponse, Status>> + Send>>;

#[tonic::async_trait]
impl WeatherForecastService for WeatherForecastServer {
    type ReceiveLiveWeatherForecastStream = LiveStream;
    type ReceiveHistoricalWeatherForecastStream = HistStream;

    async fn receive_live_weather_forecast(
        &self,
        request: Request<ReceiveLiveWeatherForecastRequest>,
    ) -> Result<Response<Self::ReceiveLiveWeatherForecastStream>, Status> {
        let req = request.into_inner();
        // Each requested Location's lat/lon snaps to the registry's
        // 0.1° grid; if the client passes none, emit every
        // registered location.
        let requested: Vec<(f64, f64)> = req
            .locations
            .iter()
            .filter_map(|loc| Some((loc.latitude as f64, loc.longitude as f64)))
            .collect();
        let (tx, rx) = mpsc::channel(8);
        let weather = self.weather.clone();
        let history = self.history.clone();
        let cadence = self.cadence.clone();
        let push_and_emit = move |reg: &WeatherRegistry, now: DateTime<Utc>| {
            // For a "no locations requested" frame, emit one per
            // registered point. For a "locations requested" frame,
            // run each request through at_latlon — that's where
            // bilinear-style IDW interpolation happens for points
            // between registered grid entries.
            let locs: Vec<WeatherLocation> = if requested.is_empty() {
                reg.locations().to_vec()
            } else {
                requested
                    .iter()
                    .map(|(la, lo)| reg.at_latlon(*la, *lo))
                    .collect()
            };
            let day_override = reg.active_day_of_year;
            let mut frames = Vec::with_capacity(locs.len());
            for loc in locs {
                let lf = build_forecast(&loc, now, day_override);
                push_history(&history, lf.clone());
                frames.push(lf);
            }
            ReceiveLiveWeatherForecastResponse {
                location_forecasts: frames,
            }
        };
        tokio::spawn(async move {
            let initial = push_and_emit(&weather.read(), Utc::now());
            if tx.send(Ok(initial)).await.is_err() {
                return;
            }
            loop {
                // Re-read the cadence each cycle so lisp changes
                // via (set-weather-stream-cadence-seconds N) take
                // effect on the next emit without needing a
                // restart or a stream reconnect.
                let wait = *cadence.read();
                tokio::time::sleep(wait).await;
                let snap = weather.read().clone();
                let resp = push_and_emit(&snap, Utc::now());
                if tx.send(Ok(resp)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn receive_historical_weather_forecast(
        &self,
        request: Request<ReceiveHistoricalWeatherForecastRequest>,
    ) -> Result<Response<Self::ReceiveHistoricalWeatherForecastStream>, Status> {
        let req = request.into_inner();
        let start = req
            .start_create_time
            .as_ref()
            .map(|t| t.seconds)
            .unwrap_or(i64::MIN);
        let end = req
            .end_create_time
            .as_ref()
            .map(|t| t.seconds)
            .unwrap_or(i64::MAX);
        let matches: Vec<LocationForecast> = self
            .history
            .lock()
            .iter()
            .filter(|lf| {
                lf.create_time
                    .as_ref()
                    .map(|t| t.seconds >= start && t.seconds <= end)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let (tx, rx) = mpsc::channel(matches.len().max(1));
        tokio::spawn(async move {
            for lf in matches {
                let resp = ReceiveHistoricalWeatherForecastResponse {
                    location_forecasts: vec![lf],
                };
                if tx.send(Ok(resp)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

fn push_history(ring: &SharedWeatherHistory, lf: LocationForecast) {
    let mut g = ring.lock();
    if g.len() == HISTORY_CAP {
        g.pop_front();
    }
    g.push_back(lf);
}

/// Build a 24-hour hourly forecast snapshot of the current weather
/// state. The forecast is anchored at the next full UTC hour and
/// extends forward; each entry carries solar irradiance, the
/// 100 m wind u/v components, and air temperature. Values get
/// horizon-scaled noise on top so successive emits look like real
/// forecast revisions.
///
/// `day_override` pins the solar-elevation model's day-of-year for
/// every entry in the frame. The bias tick writes this from the
/// active scenario's :date so a "summer day" scenario shows summer
/// solar peaks even when run on a December afternoon. When None,
/// each entry derives day-of-year from its own valid_time.
fn build_forecast(
    state: &WeatherLocation,
    now: DateTime<Utc>,
    day_override: Option<u32>,
) -> LocationForecast {
    use chrono::Datelike;
    let next_hour_secs = (now.timestamp() / 3600 + 1) * 3600;
    let create_secs = now.timestamp();
    let mut forecasts = Vec::with_capacity(24);
    let wind_dir_rad = state.wind_direction.to_radians();
    for h in 0..24i64 {
        let valid_time_secs = next_hour_secs + h * 3600;
        let valid_time = DateTime::from_timestamp(valid_time_secs, 0).unwrap_or(now);
        let hour_of_day = valid_time.hour() as f64;
        let day = day_override.unwrap_or_else(|| valid_time.ordinal());
        let horizon_h = ((valid_time_secs - create_secs) as f64 / 3600.0).max(0.0);
        let wind_speed = state.wind_at(hour_of_day);
        let u = wind_speed * wind_dir_rad.cos();
        let v = wind_speed * wind_dir_rad.sin();
        let solar = state.solar_at(hour_of_day, day);
        let temp = state.temperature_at(hour_of_day);
        let nv = |truth: f64, feature: ForecastFeature| -> f32 {
            apply_noise(truth, horizon_h, feature, create_secs, valid_time_secs) as f32
        };
        forecasts.push(Forecasts {
            valid_time: Some(prost_types::Timestamp {
                seconds: valid_time_secs,
                nanos: 0,
            }),
            features: vec![
                FeatureForecast {
                    feature: ForecastFeature::SurfaceSolarRadiationDownwards as i32,
                    value: nv(solar, ForecastFeature::SurfaceSolarRadiationDownwards),
                },
                FeatureForecast {
                    feature: ForecastFeature::UWindComponent100Metre as i32,
                    value: nv(u, ForecastFeature::UWindComponent100Metre),
                },
                FeatureForecast {
                    feature: ForecastFeature::VWindComponent100Metre as i32,
                    value: nv(v, ForecastFeature::VWindComponent100Metre),
                },
                FeatureForecast {
                    feature: ForecastFeature::Temperature2Metre as i32,
                    value: nv(temp, ForecastFeature::Temperature2Metre),
                },
            ],
        });
    }
    LocationForecast {
        forecasts,
        location: Some(crate::proto::common_v1::Location {
            latitude: state.lat as f32,
            longitude: state.lon as f32,
            country_code: String::new(),
        }),
        create_time: Some(prost_types::Timestamp {
            seconds: create_secs,
            nanos: 0,
        }),
    }
}

/// Deterministic uniform-in-±sigma noise. Per-feature `sigma_per_h`
/// values are calibrated so a 24-hour-out forecast carries a
/// meaningful uncertainty without dominating the truth:
///   solar  30  W/m² per hour, scaled by truth / 1361 so night
///          (truth = 0) gets zero noise. Peak summer-noon truth
///          ≈ 900 W/m² → effective σ ≈ 19.8 W/m² per horizon hour,
///          giving ~±475 W/m² at the 24 h horizon — substantial
///          uncertainty for sunny days, zero phantom irradiance
///          at 22:00.
///   wind   0.5 m/s per hour  → ±12 m/s
///   temp   0.3 K per hour    → ±7.2 K
/// Inputs feed an FNV-style seed so the same (create, valid,
/// feature) tuple always yields the same noise — important so the
/// same forecast appears identical to the history replay.
pub(crate) fn apply_noise(
    truth: f64,
    horizon_h: f64,
    feature: ForecastFeature,
    create_secs: i64,
    valid_secs: i64,
) -> f64 {
    let sigma_per_h = match feature {
        ForecastFeature::SurfaceSolarRadiationDownwards => 30.0,
        ForecastFeature::UWindComponent100Metre | ForecastFeature::VWindComponent100Metre => 0.5,
        ForecastFeature::UWindComponent10Metre | ForecastFeature::VWindComponent10Metre => 0.5,
        ForecastFeature::Temperature2Metre => 0.3,
        _ => 0.0,
    };
    let sigma = sigma_per_h * horizon_h;
    if sigma <= 0.0 {
        return truth;
    }
    // Solar irradiance forecast error scales with magnitude in
    // reality — sunny days have large absolute error, night has
    // none. Without this scale, additive ±sigma noise generated
    // phantom 28 W/m² readings on top of zero-truth night hours
    // (clamped from a symmetric [-sigma, sigma] swing).
    let truth_scale = match feature {
        ForecastFeature::SurfaceSolarRadiationDownwards => (truth / 1361.0).clamp(0.0, 1.0),
        _ => 1.0,
    };
    let mut s: u64 = 0xCBF29CE484222325;
    for v in [create_secs as u64, valid_secs as u64, feature as i32 as u64] {
        s = s.wrapping_mul(0x100000001B3).wrapping_add(v);
    }
    let mut rng = SmallRng::seed_from_u64(s);
    let r: f64 = rng.gen_range(-1.0_f64..=1.0_f64);
    let noisy = truth + r * sigma * truth_scale;
    match feature {
        // Solar irradiance at surface is physically bounded:
        // never below zero, never above the top-of-atmosphere
        // solar constant (1361 W/m²).
        ForecastFeature::SurfaceSolarRadiationDownwards => noisy.clamp(0.0, 1361.0),
        _ => noisy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solar_noise_is_zero_when_truth_is_zero() {
        // 24-hour horizon, no daylight (truth = 0). Should stay
        // exactly 0 instead of polluting with phantom irradiance.
        let result = apply_noise(
            0.0,
            24.0,
            ForecastFeature::SurfaceSolarRadiationDownwards,
            1000,
            2000,
        );
        assert_eq!(result, 0.0);
    }

    #[test]
    fn solar_noise_scales_with_truth_magnitude() {
        // Same horizon + seeds, only truth differs. Deviation
        // from truth should grow with truth magnitude because σ
        // scales with truth / 1361.
        let dim = apply_noise(
            50.0,
            24.0,
            ForecastFeature::SurfaceSolarRadiationDownwards,
            1000,
            2000,
        );
        let bright = apply_noise(
            900.0,
            24.0,
            ForecastFeature::SurfaceSolarRadiationDownwards,
            1000,
            2000,
        );
        let dim_dev = (dim - 50.0).abs();
        let bright_dev = (bright - 900.0).abs();
        assert!(
            bright_dev > dim_dev * 5.0,
            "bright-day dev {bright_dev} should be much bigger than dim-day dev {dim_dev}"
        );
    }

    #[test]
    fn non_solar_noise_unchanged_by_truth_scale() {
        // Temperature noise is additive — applies the same sigma
        // regardless of truth magnitude (a forecast 24 h out has
        // similar absolute uncertainty whether it's 270 or 295 K).
        let cold = apply_noise(270.0, 24.0, ForecastFeature::Temperature2Metre, 1000, 2000);
        let warm = apply_noise(295.0, 24.0, ForecastFeature::Temperature2Metre, 1000, 2000);
        let cold_dev = (cold - 270.0).abs();
        let warm_dev = (warm - 295.0).abs();
        // Same seeds → same r → same |dev| within rounding.
        assert!((cold_dev - warm_dev).abs() < 1e-9);
    }

    #[test]
    fn zero_horizon_returns_truth_exactly() {
        let result = apply_noise(
            800.0,
            0.0,
            ForecastFeature::SurfaceSolarRadiationDownwards,
            1000,
            2000,
        );
        assert_eq!(result, 800.0);
    }
}
