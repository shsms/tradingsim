//! End-to-end test for the WeatherForecast gRPC service. Spawns
//! the server on a random TCP port with a synthetic WeatherLocation
//! and confirms the live stream emits a 24-hour forecast with all
//! four features.

use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use tradingsim::proto::weather::{
    ForecastFeature, ReceiveHistoricalWeatherForecastRequest, ReceiveLiveWeatherForecastRequest,
    weather_forecast_service_client::WeatherForecastServiceClient,
    weather_forecast_service_server::WeatherForecastServiceServer,
};
use tradingsim::sim::weather::{WeatherLocation, WeatherRegistry, new_state};
use tradingsim::weather_server::WeatherForecastServer;

async fn spawn_server(state: WeatherLocation) -> String {
    let weather = new_state();
    // Replace the default-slot params so the registry still has
    // exactly one location and the assertions on forecast count
    // remain valid.
    {
        let mut reg: parking_lot::RwLockWriteGuard<'_, WeatherRegistry> = weather.write();
        *reg.default_mut() = state;
    }

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
    let addr = spawn_server(WeatherLocation::de_lu_typical()).await;
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
async fn historical_stream_replays_past_emissions() {
    let addr = spawn_server(WeatherLocation::de_lu_typical()).await;

    // First subscriber triggers the initial emit which gets pushed
    // onto the server-side history ring.
    let mut live = WeatherForecastServiceClient::connect(addr.clone()).await.unwrap();
    let mut live_stream = live
        .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest::default())
        .await
        .unwrap()
        .into_inner();
    let _ = live_stream.next().await.expect("initial live emit");
    drop(live_stream);

    // Now hit the historical RPC; we should see at least one
    // forecast — the one the live stream just produced.
    let mut hist = WeatherForecastServiceClient::connect(addr).await.unwrap();
    let mut hist_stream = hist
        .receive_historical_weather_forecast(ReceiveHistoricalWeatherForecastRequest::default())
        .await
        .unwrap()
        .into_inner();
    let mut seen = 0;
    while let Some(msg) = hist_stream.next().await {
        msg.unwrap();
        seen += 1;
    }
    assert!(seen >= 1, "expected at least one historical forecast, got {seen}");
}

#[tokio::test]
async fn forecast_noise_scales_with_horizon() {
    let addr = spawn_server(WeatherLocation { name: "test".to_string(), lat: 50.0, lon: 10.0,
        cloud_cover: 0.0,
        ..WeatherLocation::de_lu_typical()
    })
    .await;
    let mut client = WeatherForecastServiceClient::connect(addr).await.unwrap();
    let resp = client
        .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest::default())
        .await
        .unwrap()
        .into_inner()
        .next()
        .await
        .unwrap()
        .unwrap();
    let lf = &resp.location_forecasts[0];

    // The "+0h" entry of the forecast carries near-zero horizon
    // (we anchor at the next full hour, so horizon ~30-60 min). The
    // "+24h" entry is a full day out. Pick a feature with non-zero
    // truth at both points — temperature — and confirm the absolute
    // deviation from neighbour hours is larger far out.
    fn temp_at(lf: &tradingsim::proto::weather::LocationForecast, idx: usize) -> f32 {
        lf.forecasts[idx]
            .features
            .iter()
            .find(|f| f.feature == ForecastFeature::Temperature2Metre as i32)
            .unwrap()
            .value
    }
    let near = temp_at(lf, 0);
    let far = temp_at(lf, 23);

    // The underlying diurnal cycle is sinusoidal with ±8 K range;
    // adjacent hours differ by ≤ ~2 K. After noise scaled to ±7.2 K
    // at the 23-h horizon, far should diverge from its neighbour
    // more than near does.
    let near_step = (temp_at(lf, 1) - near).abs();
    let far_step = (temp_at(lf, 22) - far).abs();
    // Allow a wide margin — noise is stochastic but seeded.
    assert!(
        far_step > near_step,
        "expected far-horizon step (got {far_step:.2}) > near-horizon step (got {near_step:.2})"
    );
}

#[tokio::test]
async fn cloud_cover_attenuates_solar_feature() {
    let clear = WeatherLocation { name: "test".to_string(), lat: 50.0, lon: 10.0,
        cloud_cover: 0.0,
        ..WeatherLocation::de_lu_typical()
    };
    let overcast = WeatherLocation { name: "test".to_string(), lat: 50.0, lon: 10.0,
        cloud_cover: 0.9,
        ..WeatherLocation::de_lu_typical()
    };

    // Collect the first frame from each and compare solar at the
    // first daytime hour (the live stream emits 24 hourly forecasts
    // anchored at the next hour boundary).
    async fn peak_solar(state: WeatherLocation) -> f32 {
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
