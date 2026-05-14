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
use std::time::Duration;

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
use crate::sim::weather::{SharedWeather, WeatherLocation, WeatherRegistry};

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
}

impl WeatherForecastServer {
    pub fn new(weather: SharedWeather) -> Self {
        Self {
            weather,
            history: new_history(),
        }
    }
}

type LiveStream = Pin<
    Box<dyn Stream<Item = Result<ReceiveLiveWeatherForecastResponse, Status>> + Send>,
>;
type HistStream = Pin<
    Box<dyn Stream<Item = Result<ReceiveHistoricalWeatherForecastResponse, Status>> + Send>,
>;

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
            .filter_map(|loc| {
                Some((loc.latitude as f64, loc.longitude as f64))
            })
            .collect();
        let (tx, rx) = mpsc::channel(8);
        let weather = self.weather.clone();
        let history = self.history.clone();
        let push_and_emit = move |reg: &WeatherRegistry, now: DateTime<Utc>| {
            let locs: Vec<&WeatherLocation> = if requested.is_empty() {
                reg.locations().iter().collect()
            } else {
                requested
                    .iter()
                    .map(|(la, lo)| reg.at_latlon(*la, *lo))
                    .collect()
            };
            let mut frames = Vec::with_capacity(locs.len());
            for loc in locs {
                let lf = build_forecast(loc, now);
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
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
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
fn build_forecast(state: &WeatherLocation, now: DateTime<Utc>) -> LocationForecast {
    let next_hour_secs = (now.timestamp() / 3600 + 1) * 3600;
    let create_secs = now.timestamp();
    let mut forecasts = Vec::with_capacity(24);
    let wind_dir_rad = state.wind_direction.to_radians();
    for h in 0..24i64 {
        let valid_time_secs = next_hour_secs + h * 3600;
        let valid_time = DateTime::from_timestamp(valid_time_secs, 0).unwrap_or(now);
        let hour_of_day = valid_time.hour() as f64;
        let horizon_h = ((valid_time_secs - create_secs) as f64 / 3600.0).max(0.0);
        let wind_speed = state.wind_at(hour_of_day);
        let u = wind_speed * wind_dir_rad.cos();
        let v = wind_speed * wind_dir_rad.sin();
        let solar = state.solar_at(hour_of_day);
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
        location: None,
        create_time: Some(prost_types::Timestamp {
            seconds: create_secs,
            nanos: 0,
        }),
    }
}

/// Deterministic uniform-in-±sigma noise. Per-feature `sigma_per_h`
/// values are calibrated so a 24-hour-out forecast carries a
/// meaningful uncertainty without dominating the truth:
///   solar  30  W/m² per hour → ±720 W/m² at 24 h out
///   wind   0.5 m/s per hour  → ±12 m/s
///   temp   0.3 K per hour    → ±7.2 K
/// Inputs feed an FNV-style seed so the same (create, valid,
/// feature) tuple always yields the same noise — important so the
/// same forecast appears identical to the history replay.
fn apply_noise(
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
    let mut s: u64 = 0xCBF29CE484222325;
    for v in [create_secs as u64, valid_secs as u64, feature as i32 as u64] {
        s = s.wrapping_mul(0x100000001B3).wrapping_add(v);
    }
    let mut rng = SmallRng::seed_from_u64(s);
    let r: f64 = rng.gen_range(-1.0_f64..=1.0_f64);
    truth + r * sigma
}
