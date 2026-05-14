//! Frequenz Weather API service implementation.
//!
//! Surfaces the sim's internal `WeatherState` as a
//! `frequenz.api.weather.v1.WeatherForecastService`. The live stream
//! emits a fresh `LocationForecast` every minute carrying 24 hourly
//! forecast points; the historical stream is a placeholder for now
//! (step 9 of the realism upgrade in plan.org).

use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
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
use crate::sim::weather::{SharedWeather, WeatherState};

pub struct WeatherForecastServer {
    weather: SharedWeather,
}

impl WeatherForecastServer {
    pub fn new(weather: SharedWeather) -> Self {
        Self { weather }
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
        _request: Request<ReceiveLiveWeatherForecastRequest>,
    ) -> Result<Response<Self::ReceiveLiveWeatherForecastStream>, Status> {
        let (tx, rx) = mpsc::channel(8);
        let weather = self.weather.clone();
        tokio::spawn(async move {
            // Emit the initial forecast immediately so a fresh
            // subscriber doesn't sit silent for a minute.
            let initial = ReceiveLiveWeatherForecastResponse {
                location_forecasts: vec![build_forecast(&weather.read(), Utc::now())],
            };
            if tx.send(Ok(initial)).await.is_err() {
                return;
            }
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await; // already emitted; consume the immediate firing
            loop {
                tick.tick().await;
                let snap = weather.read().clone();
                let resp = ReceiveLiveWeatherForecastResponse {
                    location_forecasts: vec![build_forecast(&snap, Utc::now())],
                };
                if tx.send(Ok(resp)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn receive_historical_weather_forecast(
        &self,
        _request: Request<ReceiveHistoricalWeatherForecastRequest>,
    ) -> Result<Response<Self::ReceiveHistoricalWeatherForecastStream>, Status> {
        // History replay lands in step 9. For now hand back an empty
        // stream so callers see a clean EOF.
        let (_tx, rx) =
            mpsc::channel::<Result<ReceiveHistoricalWeatherForecastResponse, Status>>(1);
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// Build a 24-hour hourly forecast snapshot of the current weather
/// state. The forecast is anchored at the next full UTC hour and
/// extends forward; each entry carries solar irradiance, the
/// 100 m wind u/v components, and air temperature.
fn build_forecast(state: &WeatherState, now: DateTime<Utc>) -> LocationForecast {
    let next_hour_secs = (now.timestamp() / 3600 + 1) * 3600;
    let mut forecasts = Vec::with_capacity(24);
    let wind_dir_rad = state.wind_direction.to_radians();
    for h in 0..24i64 {
        let valid_time_secs = next_hour_secs + h * 3600;
        let valid_time = DateTime::from_timestamp(valid_time_secs, 0).unwrap_or(now);
        let hour_of_day = valid_time.hour() as f64;
        let wind_speed = state.wind_at(hour_of_day);
        let u = wind_speed * wind_dir_rad.cos();
        let v = wind_speed * wind_dir_rad.sin();
        forecasts.push(Forecasts {
            valid_time: Some(prost_types::Timestamp {
                seconds: valid_time_secs,
                nanos: 0,
            }),
            features: vec![
                FeatureForecast {
                    feature: ForecastFeature::SurfaceSolarRadiationDownwards as i32,
                    value: state.solar_at(hour_of_day) as f32,
                },
                FeatureForecast {
                    feature: ForecastFeature::UWindComponent100Metre as i32,
                    value: u as f32,
                },
                FeatureForecast {
                    feature: ForecastFeature::VWindComponent100Metre as i32,
                    value: v as f32,
                },
                FeatureForecast {
                    feature: ForecastFeature::Temperature2Metre as i32,
                    value: state.temperature_at(hour_of_day) as f32,
                },
            ],
        });
    }
    LocationForecast {
        forecasts,
        location: None,
        create_time: Some(prost_types::Timestamp {
            seconds: now.timestamp(),
            nanos: 0,
        }),
    }
}
