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
    spawn_with(|reg| *reg.default_mut() = state).await
}

/// Spawn a weather-forecast server backed by a registry the caller
/// configured. Lets each test set up locations / active_day_of_year
/// / area overrides before the gRPC client connects.
async fn spawn_with(setup: impl FnOnce(&mut WeatherRegistry)) -> String {
    let weather = new_state();
    {
        let mut reg: parking_lot::RwLockWriteGuard<'_, WeatherRegistry> = weather.write();
        setup(&mut reg);
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
    let mut live = WeatherForecastServiceClient::connect(addr.clone())
        .await
        .unwrap();
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
    assert!(
        seen >= 1,
        "expected at least one historical forecast, got {seen}"
    );
}

#[tokio::test]
async fn forecast_noise_scales_with_horizon() {
    let addr = spawn_server(WeatherLocation {
        name: "test".to_string(),
        lat: 50.0,
        lon: 10.0,
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

    let _ = (near, far);
    // Aggregate absolute hour-to-hour swings over the near third
    // vs the far third of the 24-hour forecast. Individual
    // adjacent steps are noisy enough that a single comparison
    // can land either way; summing seven steps averages over the
    // seed and reliably shows the noise envelope growing with
    // horizon. The underlying diurnal cycle's contribution to
    // hour-to-hour change is roughly symmetric across the 24 h
    // window, so the increase comes from σ scaling with horizon.
    let mut near_total = 0.0;
    for i in 0..7 {
        near_total += (temp_at(lf, i + 1) - temp_at(lf, i)).abs();
    }
    let mut far_total = 0.0;
    for i in 16..23 {
        far_total += (temp_at(lf, i + 1) - temp_at(lf, i)).abs();
    }
    assert!(
        far_total > near_total,
        "expected far-horizon total swing (got {far_total:.2}) > near-horizon total (got {near_total:.2})"
    );
}

#[tokio::test]
async fn cloud_cover_attenuates_solar_feature() {
    let clear = WeatherLocation {
        name: "test".to_string(),
        lat: 50.0,
        lon: 10.0,
        cloud_cover: 0.0,
        ..WeatherLocation::de_lu_typical()
    };
    let overcast = WeatherLocation {
        name: "test".to_string(),
        lat: 50.0,
        lon: 10.0,
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
            .filter(|f| f.feature == ForecastFeature::SurfaceSolarRadiationDownwards as i32)
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

/// Streams the first frame and returns the peak solar over the
/// 24-hour forecast for the first registered location.
async fn first_peak_solar(addr: &str) -> f32 {
    let mut client = WeatherForecastServiceClient::connect(addr.to_string())
        .await
        .unwrap();
    let resp = client
        .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest::default())
        .await
        .unwrap()
        .into_inner()
        .next()
        .await
        .unwrap()
        .unwrap();
    resp.location_forecasts[0]
        .forecasts
        .iter()
        .flat_map(|f| &f.features)
        .filter(|f| f.feature == ForecastFeature::SurfaceSolarRadiationDownwards as i32)
        .map(|f| f.value)
        .fold(f32::MIN, f32::max)
}

#[tokio::test]
async fn scenario_date_pins_day_of_year_in_forecast() {
    // Same physical location, same cloud cover — but different
    // scenario :date should produce different peak solar in the
    // stream because the solar-elevation model honors
    // active_day_of_year. Summer solstice (172) vs winter solstice
    // (355).
    let mk_state = || WeatherLocation {
        name: "berlin".into(),
        lat: 52.5,
        lon: 13.4,
        cloud_cover: 0.0,
        ..WeatherLocation::de_lu_typical()
    };
    let summer_addr = spawn_with(|reg| {
        *reg.default_mut() = mk_state();
        reg.active_day_of_year = Some(172);
    })
    .await;
    let winter_addr = spawn_with(|reg| {
        *reg.default_mut() = mk_state();
        reg.active_day_of_year = Some(355);
    })
    .await;

    let summer = first_peak_solar(&summer_addr).await;
    let winter = first_peak_solar(&winter_addr).await;
    // Berlin sits at lat 52.5 — June solar irradiance maxes around
    // 900 W/m², December around 200 W/m². Generous margin to cover
    // the deterministic per-feature noise the stream layers on top.
    assert!(
        summer > winter + 200.0,
        "summer peak {summer} should clearly exceed winter peak {winter}"
    );
}

#[tokio::test]
async fn stage_overrides_propagate_to_stream() {
    // The bias tick mutates the registry in place when a stage
    // override applies. The stream reads under that lock on each
    // emit, so a second subscriber after the mutation should see
    // the new values. Boot with a clear-sky baseline, take a
    // frame, then patch cloud cover to 0.95 (simulating an
    // "overcast" stage transition), and confirm the next frame's
    // peak solar drops sharply.
    let weather = new_state();
    {
        let mut g = weather.write();
        *g.default_mut() = WeatherLocation {
            name: "berlin".into(),
            lat: 52.5,
            lon: 13.4,
            cloud_cover: 0.0,
            baseline_cloud_cover: 0.0,
            ..WeatherLocation::de_lu_typical()
        };
        g.active_day_of_year = Some(172);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let incoming = TcpListenerStream::new(listener);
    let svc = WeatherForecastServiceServer::new(WeatherForecastServer::new(weather.clone()));
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::task::yield_now().await;

    let clear_peak = first_peak_solar(&addr).await;

    // Simulate the bias tick applying a stage's :cloud-cover
    // override.
    weather.write().default_mut().cloud_cover = 0.95;

    let cloudy_peak = first_peak_solar(&addr).await;
    assert!(
        cloudy_peak < clear_peak * 0.6,
        "post-override cloudy peak {cloudy_peak} should drop well below clear {clear_peak}"
    );
}

#[tokio::test]
async fn forecast_per_registered_location() {
    // Two locations, two LocationForecasts in the first frame
    // (when the client doesn't filter by lat/lon).
    let addr = spawn_with(|reg| {
        *reg.default_mut() = WeatherLocation {
            name: "berlin".into(),
            lat: 52.5,
            lon: 13.4,
            ..WeatherLocation::de_lu_typical()
        };
        reg.upsert(WeatherLocation {
            name: "munich".into(),
            lat: 48.1,
            lon: 11.6,
            ..WeatherLocation::de_lu_typical()
        });
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
    assert_eq!(resp.location_forecasts.len(), 2);
    let lats: Vec<f32> = resp
        .location_forecasts
        .iter()
        .filter_map(|lf| lf.location.as_ref().map(|l| l.latitude))
        .collect();
    assert!(lats.contains(&52.5_f32));
    assert!(lats.contains(&48.1_f32));
}

#[tokio::test]
async fn requested_latlon_returns_one_forecast_at_request_point() {
    // Two anchors registered. Client asks for a single arbitrary
    // point between them. Server should return exactly one
    // LocationForecast whose location echoes the requested
    // lat / lon (proving the at_latlon IDW path ran rather than
    // emit-everything).
    let addr = spawn_with(|reg| {
        *reg.default_mut() = WeatherLocation {
            name: "berlin".into(),
            lat: 52.5,
            lon: 13.4,
            ..WeatherLocation::de_lu_typical()
        };
        reg.upsert(WeatherLocation {
            name: "munich".into(),
            lat: 48.1,
            lon: 11.6,
            ..WeatherLocation::de_lu_typical()
        });
    })
    .await;
    let mut client = WeatherForecastServiceClient::connect(addr).await.unwrap();
    let req_lat = 50.3_f32;
    let req_lon = 12.5_f32;
    let between = tradingsim::proto::common_v1::Location {
        latitude: req_lat,
        longitude: req_lon,
        country_code: String::new(),
    };
    let resp = client
        .receive_live_weather_forecast(ReceiveLiveWeatherForecastRequest {
            locations: vec![between],
            features: vec![],
            forecast_horizon: None,
        })
        .await
        .unwrap()
        .into_inner()
        .next()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.location_forecasts.len(), 1);
    let loc = resp.location_forecasts[0]
        .location
        .as_ref()
        .expect("location echoed");
    // IDW snaps the request to the 0.1° grid before computing the
    // interpolated location, so the echoed lat/lon may differ by up
    // to 0.05 from what the client asked for. Tolerance 0.1.
    assert!(
        (loc.latitude - req_lat).abs() < 0.1,
        "echoed lat {} ≉ {req_lat}",
        loc.latitude
    );
    assert!(
        (loc.longitude - req_lon).abs() < 0.1,
        "echoed lon {} ≉ {req_lon}",
        loc.longitude
    );
}
