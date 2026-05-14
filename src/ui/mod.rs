//! Minimal axum-driven UI. One HTML page (embedded via `include_str!`)
//! + a couple of /api/* JSON endpoints + two WebSockets for the live
//! public-trade and public-book streams. Spawned by the binary in
//! its own task; talks to the shared World handle.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::proto::trading::{MarketSide, PublicOrderBookRecord};
use crate::scenarios::{ScenarioEntry, ScenarioRuntime, SharedScenarios};
use crate::sim::trade::PublicTrade;
use crate::sim::world::World;
use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde::Serialize;
use parking_lot::RwLock;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone)]
struct UiState {
    world: Arc<RwLock<World>>,
    scenarios: Option<SharedScenarios>,
    weather: Option<crate::sim::weather::SharedWeather>,
    clock: crate::sim::clock::SharedClock,
}

pub async fn serve(
    addr: SocketAddr,
    world: Arc<RwLock<World>>,
    scenarios: Option<SharedScenarios>,
    weather: Option<crate::sim::weather::SharedWeather>,
    clock: crate::sim::clock::SharedClock,
) -> std::io::Result<()> {
    let state = UiState {
        world,
        scenarios,
        weather,
        clock,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/info", get(api_info))
        .route("/api/clock", get(api_clock))
        .route("/api/gridpools", get(api_gridpools))
        .route("/api/scenarios", get(api_scenarios))
        .route("/api/scenarios/{name}/start", post(api_scenario_start))
        .route("/api/scenarios/{name}/next", post(api_scenario_next))
        .route("/api/scenarios/{name}/prev", post(api_scenario_prev))
        .route("/api/scenarios/{name}/jump/{idx}", post(api_scenario_jump))
        .route("/api/scenarios/{name}/stop", post(api_scenario_stop))
        .route("/api/weather", get(api_weather))
        .route("/ws/public-trades", get(ws_public_trades))
        .route("/ws/public-book", get(ws_public_book))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("UI server listening on http://{addr}/");
    axum::serve(listener, app).await
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../../ui-assets/index.html"))
}

#[derive(Serialize)]
struct InfoResp {
    version: &'static str,
    gridpools: usize,
    markets: usize,
    couplings: usize,
}

async fn api_info(State(s): State<UiState>) -> Json<InfoResp> {
    let w = s.world.read();
    Json(InfoResp {
        version: env!("CARGO_PKG_VERSION"),
        gridpools: w.gridpools().len(),
        markets: w.markets().len(),
        couplings: w
            .gridpools()
            .iter()
            .flat_map(|g| g.areas.iter())
            .flat_map(|a| w.coupled_areas(a))
            .count()
            / 2,
    })
}

#[derive(Serialize)]
struct GridpoolResp {
    id: u64,
    name: String,
    areas: Vec<String>,
    orders: usize,
    trades: usize,
}

async fn api_gridpools(State(s): State<UiState>) -> Json<Vec<GridpoolResp>> {
    let w = s.world.read();
    let pools: Vec<GridpoolResp> = w
        .gridpools()
        .iter()
        .map(|g| GridpoolResp {
            id: g.id.0,
            name: g.name.clone(),
            areas: g.areas.iter().map(|a| a.code.clone()).collect(),
            orders: g.orders().count(),
            trades: g.trades().len(),
        })
        .collect();
    Json(pools)
}

#[derive(Serialize)]
struct PublicTradeJson {
    id: u64,
    buy_area: String,
    sell_area: String,
    /// Delivery-period start as RFC-3339 UTC. The UI shows this so
    /// the reader can tell which contract a print belongs to without
    /// inferring it from the price.
    period: String,
    price: String,
    quantity: String,
    execution_time: String,
}

impl From<&PublicTrade> for PublicTradeJson {
    fn from(t: &PublicTrade) -> Self {
        Self {
            id: t.id.0,
            buy_area: t.buy_area.code.clone(),
            sell_area: t.sell_area.code.clone(),
            period: t.period.start.to_rfc3339(),
            price: t.price.normalize().to_string(),
            quantity: t.quantity.normalize().to_string(),
            execution_time: t.execution_time.to_rfc3339(),
        }
    }
}

async fn ws_public_trades(ws: WebSocketUpgrade, State(s): State<UiState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_trades_ws(socket, s))
}

async fn handle_trades_ws(mut socket: WebSocket, s: UiState) {
    let rx = s.world.read().subscribe_public_trades();
    let mut stream = BroadcastStream::new(rx);
    while let Some(item) = stream.next().await {
        let Ok(t) = item else { continue };
        let payload: PublicTradeJson = (&t).into();
        if let Ok(s) = serde_json::to_string(&payload)
            && socket.send(Message::Text(s.into())).await.is_err()
        {
            break;
        }
    }
}

#[derive(Serialize)]
struct PublicBookJson {
    id: u64,
    side: i32,
    area: String,
    /// Delivery-period start as RFC-3339 UTC. The UI uses this plus
    /// `area` to bucket book entries by contract.
    period: String,
    price: String,
    quantity: String,
}

fn book_record_to_json(r: &PublicOrderBookRecord) -> PublicBookJson {
    let period = r
        .delivery_period
        .as_ref()
        .and_then(|p| p.start.as_ref())
        .map(|ts| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    PublicBookJson {
        id: r.id,
        side: r.side,
        area: r
            .delivery_area
            .as_ref()
            .map(|a| a.code.clone())
            .unwrap_or_default(),
        period,
        price: r
            .price
            .as_ref()
            .and_then(|p| p.amount.as_ref())
            .map(|a| a.value.clone())
            .unwrap_or_default(),
        quantity: r
            .quantity
            .as_ref()
            .and_then(|p| p.mw.as_ref())
            .map(|a| a.value.clone())
            .unwrap_or_default(),
    }
}

async fn ws_public_book(ws: WebSocketUpgrade, State(s): State<UiState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_book_ws(socket, s))
}

async fn handle_book_ws(mut socket: WebSocket, s: UiState) {
    let _ = MarketSide::Buy; // silence "unused MarketSide" if the enum isn't referenced
    let _ = header::CONTENT_TYPE; // silence unused header import on some configs

    // Capture the current book snapshot + subscribe under a single
    // lock so any concurrent event is either reflected in the
    // snapshot (already fired) or queued on the receiver (fires
    // after) — never lost.
    let (snapshot, rx) = {
        let w = s.world.read();
        let snap = w.snapshot_books(chrono::Utc::now());
        let rx = w.subscribe_public_book();
        (snap, rx)
    };
    for rec in snapshot {
        let payload = book_record_to_json(&rec);
        if let Ok(s) = serde_json::to_string(&payload)
            && socket.send(Message::Text(s.into())).await.is_err()
        {
            return;
        }
    }

    let mut stream = BroadcastStream::new(rx);
    while let Some(item) = stream.next().await {
        let r = match item {
            Ok(r) => r,
            // Lagged means we missed events. Close the socket so the
            // browser reconnects and gets a fresh snapshot instead
            // of accumulating ghost orders for the deletes it never
            // saw. The browser's openBookWs() already retries on
            // close.
            Err(_) => break,
        };
        let payload = book_record_to_json(&r);
        if let Ok(s) = serde_json::to_string(&payload)
            && socket.send(Message::Text(s.into())).await.is_err()
        {
            break;
        }
    }
}

// -----------------------------------------------------------------------------
// Scenarios — list, start, advance, stop. The lisp side populates the
// registry via `(define-scenario …)`; these endpoints read it and
// invoke the named stage defun via the shared TulispContext.
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct StageJson {
    name: String,
    hour_from: f64,
    hour_to: f64,
    bias_from: f64,
    bias_to: f64,
    /// Optional weather overrides — same shape as the Stage
    /// struct's fields. None when the stage leaves the area's
    /// baseline weather alone.
    cloud_cover: Option<f64>,
    mean_wind: Option<f64>,
    temperature_base: Option<f64>,
}

#[derive(Serialize)]
struct ScenarioJson {
    name: String,
    description: String,
    stages: Vec<StageJson>,
    current_stage: Option<usize>,
    /// Stage that contains the current UTC wallclock hour, if any.
    /// Lets the UI mark "where auto-advance would land".
    wallclock_stage: Option<usize>,
    manual_override: bool,
    started_at: Option<String>,
    stage_entered_at: Option<String>,
}

fn scenario_to_json(e: &ScenarioEntry, clock: &crate::sim::clock::Clock) -> ScenarioJson {
    let now = chrono::Utc::now();
    let h = clock.local_hour(now);
    ScenarioJson {
        name: e.def.name.clone(),
        description: e.def.description.clone(),
        stages: e
            .def
            .stages
            .iter()
            .map(|s| StageJson {
                name: s.name.clone(),
                hour_from: s.hour_from,
                hour_to: s.hour_to,
                bias_from: s.bias_from,
                bias_to: s.bias_to,
                cloud_cover: s.cloud_cover,
                mean_wind: s.mean_wind,
                temperature_base: s.temperature_base,
            })
            .collect(),
        current_stage: e.runtime.current_stage,
        wallclock_stage: wallclock_stage(&e.def, h),
        manual_override: e.runtime.manual_override,
        started_at: e.runtime.started_at.map(|t| t.to_rfc3339()),
        stage_entered_at: e.runtime.stage_entered_at.map(|t| t.to_rfc3339()),
    }
}

#[derive(Serialize)]
struct WeatherLocJson {
    name: String,
    /// EIC area code this location is linked to via
    /// `(%make-weather-location :area …)`, if any. None for the
    /// fallback default location. Lets the UI filter weather rows
    /// to match the active area chips.
    area_code: Option<String>,
    lat: f64,
    lon: f64,
    cloud_cover: f64,
    mean_wind: f64,
    wind_direction: f64,
    /// Solar irradiance (W/m²) at the current UTC hour.
    solar_now: f64,
    /// Wind speed at 100 m (m/s) at the current UTC hour.
    wind_now: f64,
    /// Air temperature in degrees Celsius at the current UTC hour.
    temp_c_now: f64,
}

async fn api_weather(State(s): State<UiState>) -> Json<Vec<WeatherLocJson>> {
    let Some(handle) = s.weather.as_ref() else {
        return Json(Vec::new());
    };
    let now = chrono::Utc::now();
    let clock = s.clock.read().clone();
    let reg = handle.read();
    // active_hour pins the panel to the active stage's midpoint
    // (e.g. 14.5 for a 13:00–16:00 "deep belly"), so a user
    // picking that stage at 2 AM still sees midday solar /
    // temperature. With no scenario running, fall back to the
    // configured-tz local hour — wallclock would be UTC, which
    // doesn't match the physics that uses local civil time.
    let hour = reg.active_hour.unwrap_or_else(|| clock.local_hour(now));
    let day = reg
        .active_day_of_year
        .unwrap_or_else(|| clock.local_day_of_year(now));
    let out: Vec<WeatherLocJson> = reg
        .locations()
        .iter()
        .enumerate()
        .map(|(idx, l)| WeatherLocJson {
            name: l.name.clone(),
            area_code: reg.area_for_location(idx).map(String::from),
            lat: l.lat,
            lon: l.lon,
            cloud_cover: l.cloud_cover,
            mean_wind: l.mean_wind,
            wind_direction: l.wind_direction,
            solar_now: l.solar_at(hour, day),
            wind_now: l.wind_at(hour),
            temp_c_now: l.temperature_at(hour) - 273.15,
        })
        .collect();
    Json(out)
}

async fn api_scenarios(State(s): State<UiState>) -> Json<Vec<ScenarioJson>> {
    let Some(reg) = s.scenarios.as_ref() else {
        return Json(Vec::new());
    };
    let clock = s.clock.read().clone();
    let mut out: Vec<ScenarioJson> = reg
        .lock()
        .values()
        .map(|e| scenario_to_json(e, &clock))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

#[derive(Serialize)]
struct ClockJson {
    /// IANA timezone name the physics / scenarios / UI all use.
    /// Lets the browser format ISO timestamps in the right zone
    /// instead of falling back to whatever the user's OS reports —
    /// remote operators looking at a Berlin-anchored sim still see
    /// the hours scenarios expect.
    tz: &'static str,
}

async fn api_clock(State(s): State<UiState>) -> Json<ClockJson> {
    Json(ClockJson {
        tz: s.clock.read().tz_name(),
    })
}

async fn api_scenario_start(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    let clock = s.clock.read().clone();
    mutate_scenario(&s, &name, |def, rt| {
        if def.stages.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let now = chrono::Utc::now();
        let hour = clock.local_hour(now);
        let idx = wallclock_stage(def, hour).unwrap_or(0);
        rt.current_stage = Some(idx);
        rt.started_at = Some(now);
        rt.stage_entered_at = Some(now);
        rt.manual_override = false;
        Ok(())
    })
}

async fn api_scenario_next(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    jump_relative(&s, &name, 1)
}

async fn api_scenario_prev(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    jump_relative(&s, &name, -1)
}

async fn api_scenario_jump(
    State(s): State<UiState>,
    Path((name, idx)): Path<(String, usize)>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    let clock = s.clock.read().clone();
    mutate_scenario(&s, &name, |def, rt| {
        if idx >= def.stages.len() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let now = chrono::Utc::now();
        rt.current_stage = Some(idx);
        rt.stage_entered_at = Some(now);
        // Manual unless the operator just clicked the wallclock-matching
        // stage — that's "resume auto", not "freeze here".
        rt.manual_override = wallclock_stage(def, clock.local_hour(now)) != Some(idx);
        Ok(())
    })
}

async fn api_scenario_stop(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    mutate_scenario(&s, &name, |_def, rt| {
        *rt = ScenarioRuntime::default();
        Ok(())
    })
}

fn jump_relative(s: &UiState, name: &str, delta: i64) -> Result<Json<ScenarioJson>, StatusCode> {
    let clock = s.clock.read().clone();
    mutate_scenario(s, name, |def, rt| {
        let cur = rt.current_stage.ok_or(StatusCode::BAD_REQUEST)? as i64;
        let target = cur + delta;
        if target < 0 || target as usize >= def.stages.len() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let idx = target as usize;
        let now = chrono::Utc::now();
        rt.current_stage = Some(idx);
        rt.stage_entered_at = Some(now);
        rt.manual_override = wallclock_stage(def, clock.local_hour(now)) != Some(idx);
        Ok(())
    })
}

fn mutate_scenario<F>(s: &UiState, name: &str, f: F) -> Result<Json<ScenarioJson>, StatusCode>
where
    F: FnOnce(&crate::scenarios::ScenarioDef, &mut ScenarioRuntime) -> Result<(), StatusCode>,
{
    let reg = s.scenarios.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let clock = s.clock.read().clone();
    let mut guard = reg.lock();
    let entry = guard.get_mut(name).ok_or(StatusCode::NOT_FOUND)?;
    f(&entry.def, &mut entry.runtime)?;
    Ok(Json(scenario_to_json(entry, &clock)))
}

use crate::scenarios::wallclock_stage;
