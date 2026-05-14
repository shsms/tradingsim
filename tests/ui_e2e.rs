//! In-process tests for the UI server's HTTP surface. Builds the
//! axum Router via `ui::build_router` and drives each endpoint
//! through `tower::ServiceExt::oneshot`. No TCP, no live server —
//! faster + more deterministic than the gRPC integration tests in
//! grpc_e2e.rs which need a real socket.
//!
//! Each test constructs a fresh state tuple so the registry,
//! weather, and clock don't bleed across tests.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use parking_lot::RwLock;
use serde_json::Value;
use tower::ServiceExt;

use tradingsim::scenarios::{
    ScenarioDef, ScenarioEntry, ScenarioRuntime, SharedScenarios, Stage, new_registry,
};
use tradingsim::sim::clock::{Clock, new_clock};
use tradingsim::sim::gridpool::Gridpool;
use tradingsim::sim::market::{Area, MarketRegistry, MarketRules};
use tradingsim::sim::order::GridpoolId;
use tradingsim::sim::weather::{SharedWeather, WeatherLocation, new_state};
use tradingsim::sim::world::World;
use tradingsim::ui::build_router;

const DE_TN: &str = "10YDE-EON------1";

fn empty_world() -> Arc<RwLock<World>> {
    let mut markets = MarketRegistry::new();
    markets.insert(MarketRules::for_area(Area::eic(DE_TN), tradingsim::sim::market::Currency::Eur));
    let mut world = World::new(markets);
    world.register_gridpool(Gridpool::new(GridpoolId(1), "test", vec![Area::eic(DE_TN)]));
    Arc::new(RwLock::new(world))
}

fn populated_weather() -> SharedWeather {
    let w = new_state();
    {
        let mut g = w.write();
        // Use a distinct lat/lon so upsert allocates a new slot
        // rather than overwriting the default one — gives the
        // /api/weather endpoint two rows to return.
        let mut loc = WeatherLocation::default_for_tests();
        loc.name = "tn".into();
        loc.lat = 50.4;
        loc.lon = 11.6;
        let idx = g.upsert(loc);
        g.link_area(DE_TN, idx);
    }
    w
}

fn add_scenario(reg: &SharedScenarios, name: &str, stages: Vec<Stage>) {
    reg.lock().insert(
        name.to_string(),
        ScenarioEntry {
            def: ScenarioDef {
                name: name.to_string(),
                description: "test".to_string(),
                date: None,
                stages,
            },
            runtime: ScenarioRuntime::default(),
        },
    );
}

fn stage(name: &str, hour_from: f64, hour_to: f64, bias_from: f64, bias_to: f64) -> Stage {
    Stage {
        name: name.to_string(),
        hour_from,
        hour_to,
        bias_from,
        bias_to,
        cloud_cover: None,
        mean_wind: None,
        temperature_base: None,
    }
}

fn three_stage_scenario() -> Vec<Stage> {
    vec![
        stage("overnight", 0.0, 6.0, 0.50, 0.55),
        stage("morning", 6.0, 12.0, 0.55, 0.60),
        stage("afternoon", 12.0, 24.0, 0.60, 0.45),
    ]
}

fn build_app() -> (Router, SharedScenarios) {
    let world = empty_world();
    let scenarios = new_registry();
    let weather = populated_weather();
    let clock = new_clock();
    let router = build_router(world, Some(scenarios.clone()), Some(weather), clock);
    (router, scenarios)
}

async fn get_json(app: &Router, path: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn post_json(app: &Router, path: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn index_serves_embedded_html() {
    let (app, _) = build_app();
    let res = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&bytes).unwrap();
    assert!(html.contains("<title>tradingsim</title>"));
}

#[tokio::test]
async fn api_info_returns_version_and_counts() {
    let (app, _) = build_app();
    let (status, j) = get_json(&app, "/api/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(j["gridpools"], 1);
    assert_eq!(j["markets"], 1);
}

#[tokio::test]
async fn api_clock_returns_default_berlin() {
    let (app, _) = build_app();
    let (status, j) = get_json(&app, "/api/clock").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["tz"], "Europe/Berlin");
}

#[tokio::test]
async fn api_gridpools_lists_registered_pool() {
    let (app, _) = build_app();
    let (status, j) = get_json(&app, "/api/gridpools").await;
    assert_eq!(status, StatusCode::OK);
    let arr = j.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], 1);
}

#[tokio::test]
async fn api_scenarios_empty_by_default() {
    let (app, _) = build_app();
    let (status, j) = get_json(&app, "/api/scenarios").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn api_scenarios_lists_registered_with_stages() {
    let (app, reg) = build_app();
    add_scenario(&reg, "alpha", three_stage_scenario());
    let (status, j) = get_json(&app, "/api/scenarios").await;
    assert_eq!(status, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "alpha");
    assert_eq!(arr[0]["stages"].as_array().unwrap().len(), 3);
    assert_eq!(arr[0]["current_stage"], Value::Null);
    // wallclock_stage is one of 0..3 since the three stages cover
    // the full 24-hour day with no gaps.
    let ws = arr[0]["wallclock_stage"].as_u64().unwrap();
    assert!(ws < 3);
}

#[tokio::test]
async fn api_weather_returns_locations_with_solar_and_wind() {
    let (app, _) = build_app();
    let (status, j) = get_json(&app, "/api/weather").await;
    assert_eq!(status, StatusCode::OK);
    let arr = j.as_array().unwrap();
    // Default state + the one we registered in populated_weather:
    // 2 locations total.
    assert_eq!(arr.len(), 2);
    let entries: Vec<&Value> = arr.iter().collect();
    for entry in entries {
        assert!(entry["lat"].is_number());
        assert!(entry["lon"].is_number());
        assert!(entry["solar_now"].is_number());
        assert!(entry["wind_now"].is_number());
        assert!(entry["temp_c_now"].is_number());
    }
}

#[tokio::test]
async fn scenario_start_sets_current_stage_and_clears_manual() {
    let (app, reg) = build_app();
    add_scenario(&reg, "alpha", three_stage_scenario());
    let (status, j) = post_json(&app, "/api/scenarios/alpha/start").await;
    assert_eq!(status, StatusCode::OK);
    assert!(j["current_stage"].is_number());
    assert_eq!(j["manual_override"], false);
    assert!(j["started_at"].is_string());
}

#[tokio::test]
async fn scenario_jump_sets_manual_when_target_is_not_wallclock() {
    let (app, reg) = build_app();
    // First stage spans 0–6 — jumping there from any wallclock hour
    // outside that window flips manual_override on.
    add_scenario(&reg, "alpha", three_stage_scenario());
    post_json(&app, "/api/scenarios/alpha/start").await;
    let (status, j) = post_json(&app, "/api/scenarios/alpha/jump/0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j["current_stage"], 0);
    // Always-on stage 0 only matches wallclock for the 0–6 local
    // hour window. Outside that, manual flips on. Inside, off. We
    // can't pin wallclock so just assert the field is a bool.
    assert!(j["manual_override"].is_boolean());
}

#[tokio::test]
async fn scenario_jump_out_of_range_returns_400() {
    let (app, reg) = build_app();
    add_scenario(&reg, "alpha", three_stage_scenario());
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/scenarios/alpha/jump/9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn scenario_next_prev_walk_stage_indices() {
    let (app, reg) = build_app();
    add_scenario(&reg, "alpha", three_stage_scenario());
    post_json(&app, "/api/scenarios/alpha/start").await;
    // jump to 0 so we have a known starting point
    post_json(&app, "/api/scenarios/alpha/jump/0").await;
    let (_, j1) = post_json(&app, "/api/scenarios/alpha/next").await;
    assert_eq!(j1["current_stage"], 1);
    let (_, j2) = post_json(&app, "/api/scenarios/alpha/next").await;
    assert_eq!(j2["current_stage"], 2);
    // next at last stage is a no-op (returns 400 since cur + 1 >= len)
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/scenarios/alpha/next")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let (_, j3) = post_json(&app, "/api/scenarios/alpha/prev").await;
    assert_eq!(j3["current_stage"], 1);
}

#[tokio::test]
async fn scenario_stop_clears_runtime() {
    let (app, reg) = build_app();
    add_scenario(&reg, "alpha", three_stage_scenario());
    post_json(&app, "/api/scenarios/alpha/start").await;
    let (_, j) = post_json(&app, "/api/scenarios/alpha/stop").await;
    assert_eq!(j["current_stage"], Value::Null);
    assert_eq!(j["manual_override"], false);
    assert_eq!(j["started_at"], Value::Null);
}

#[tokio::test]
async fn scenario_endpoints_return_404_for_unknown_name() {
    let (app, _) = build_app();
    for path in [
        "/api/scenarios/ghost/start",
        "/api/scenarios/ghost/next",
        "/api/scenarios/ghost/prev",
        "/api/scenarios/ghost/jump/0",
        "/api/scenarios/ghost/stop",
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn api_clock_reflects_runtime_tz_changes() {
    // Build the state by hand so we can mutate the shared clock
    // between requests — the (set-timezone …) defun does the same
    // through the lisp layer.
    let world = empty_world();
    let scenarios = new_registry();
    let weather = populated_weather();
    let clock = new_clock();
    let router = build_router(world, Some(scenarios), Some(weather), clock.clone());

    let (_, j1) = get_json(&router, "/api/clock").await;
    assert_eq!(j1["tz"], "Europe/Berlin");

    *clock.write() = Clock::new(chrono_tz::America::New_York);

    let (_, j2) = get_json(&router, "/api/clock").await;
    assert_eq!(j2["tz"], "America/New_York");
}
