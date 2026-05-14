//! End-to-end test for the WeatherForecast gRPC service. Spawns
//! the server on a random TCP port with a synthetic WeatherState
//! and confirms the live stream emits a 24-hour forecast with all
//! four features.

use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use tradingsim::proto::weather::{
    ForecastFeature, ReceiveLiveWeatherForecastRequest,
    weather_forecast_service_client::WeatherForecastServiceClient,
    weather_forecast_service_server::WeatherForecastServiceServer,
};
use tradingsim::sim::weather::{WeatherState, new_state};
use tradingsim::weather_server::WeatherForecastServer;

async fn spawn_server(state: WeatherState) -> String {
    let weather = new_state();
    *weather.write() = state;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);
    let svc = WeatherForecastServiceServer::new(WeatherForecastServer::new(weather));
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::task::yield_now().await;
    format!("http://{addr}")
}

#[tokio::test]
async fn live_stream_emits_24_hour_forecast() {
    let addr = spawn_server(WeatherState::default()).await;
    let mut client = WeatherForecastServiceClient::connect(addr).await.unwrap();
    let mut stream = client
        .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest {
            locations: vec![],
            features: vec![],
            forecast_horizon: None,
        })
        .await
        .unwrap()
        .into_inner();
    let resp = stream
        .next()
        .await
        .expect("first message")
        .expect("ok status");
    assert_eq!(resp.location_forecasts.len(), 1);
    let lf = &resp.location_forecasts[0];
    assert_eq!(lf.forecasts.len(), 24, "expected 24 hourly forecasts");
    // First entry should carry all four features.
    let first = &lf.forecasts[0];
    let feature_ids: Vec<i32> = first.features.iter().map(|f| f.feature).collect();
    assert!(feature_ids.contains(&(ForecastFeature::SurfaceSolarRadiationDownwards as i32)));
    assert!(feature_ids.contains(&(ForecastFeature::UWindComponent100Metre as i32)));
    assert!(feature_ids.contains(&(ForecastFeature::VWindComponent100Metre as i32)));
    assert!(feature_ids.contains(&(ForecastFeature::Temperature2Metre as i32)));
}

#[tokio::test]
async fn cloud_cover_attenuates_solar_feature() {
    let clear = WeatherState {
        cloud_cover: 0.0,
        ..WeatherState::default()
    };
    let overcast = WeatherState {
        cloud_cover: 0.9,
        ..WeatherState::default()
    };

    // Collect the first frame from each and compare solar at the
    // first daytime hour (the live stream emits 24 hourly forecasts
    // anchored at the next hour boundary).
    async fn peak_solar(state: WeatherState) -> f32 {
        let addr = spawn_server(state).await;
        let mut client = WeatherForecastServiceClient::connect(addr).await.unwrap();
        let mut stream = client
            .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest {
                locations: vec![],
                features: vec![],
                forecast_horizon: None,
            })
            .await
            .unwrap()
            .into_inner();
        let resp = stream.next().await.unwrap().unwrap();
        resp.location_forecasts[0]
            .forecasts
            .iter()
            .flat_map(|f| &f.features)
            .filter(|f| {
                f.feature == ForecastFeature::SurfaceSolarRadiationDownwards as i32
            })
            .map(|f| f.value)
            .fold(f32::MIN, f32::max)
    }

    let clear_peak = peak_solar(clear).await;
    let overcast_peak = peak_solar(overcast).await;
    assert!(
        clear_peak > overcast_peak,
        "clear sky peak {clear_peak} should exceed overcast {overcast_peak}"
    );
}
