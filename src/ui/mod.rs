//! Minimal axum-driven UI. One HTML page (embedded via `include_str!`)
//! + a couple of /api/* JSON endpoints + two WebSockets for the live
//! public-trade and public-book streams. Spawned by the binary in
//! its own task; talks to the shared World handle.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tulisp::{SharedMut, TulispContext};

use crate::proto::trading::{MarketSide, PublicOrderBookRecord};
use crate::scenarios::{ScenarioEntry, ScenarioRuntime, SharedScenarios};
use crate::sim::trade::PublicTrade;
use crate::sim::world::World;

#[derive(Clone)]
struct UiState {
    world: Arc<Mutex<World>>,
    scenarios: Option<SharedScenarios>,
    tulisp_ctx: Option<SharedMut<TulispContext>>,
}

pub async fn serve(
    addr: SocketAddr,
    world: Arc<Mutex<World>>,
    scenarios: Option<SharedScenarios>,
    tulisp_ctx: Option<SharedMut<TulispContext>>,
) -> std::io::Result<()> {
    let state = UiState {
        world,
        scenarios,
        tulisp_ctx,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/info", get(api_info))
        .route("/api/gridpools", get(api_gridpools))
        .route("/api/scenarios", get(api_scenarios))
        .route("/api/scenarios/{name}/start", post(api_scenario_start))
        .route("/api/scenarios/{name}/next", post(api_scenario_next))
        .route("/api/scenarios/{name}/stop", post(api_scenario_stop))
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
    let w = s.world.lock().await;
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
    let w = s.world.lock().await;
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

async fn ws_public_trades(
    ws: WebSocketUpgrade,
    State(s): State<UiState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_trades_ws(socket, s))
}

async fn handle_trades_ws(mut socket: WebSocket, s: UiState) {
    let rx = s.world.lock().await.subscribe_public_trades();
    let mut stream = BroadcastStream::new(rx);
    while let Some(item) = stream.next().await {
        let Ok(t) = item else { continue };
        let payload: PublicTradeJson = (&t).into();
        if let Ok(s) = serde_json::to_string(&payload) {
            if socket.send(Message::Text(s.into())).await.is_err() {
                break;
            }
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

async fn ws_public_book(
    ws: WebSocketUpgrade,
    State(s): State<UiState>,
) -> impl IntoResponse {
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
        let w = s.world.lock().await;
        let snap = w.snapshot_books(chrono::Utc::now());
        let rx = w.subscribe_public_book();
        (snap, rx)
    };
    for rec in snapshot {
        let payload = book_record_to_json(&rec);
        if let Ok(s) = serde_json::to_string(&payload) {
            if socket.send(Message::Text(s.into())).await.is_err() {
                return;
            }
        }
    }

    let mut stream = BroadcastStream::new(rx);
    while let Some(item) = stream.next().await {
        let Ok(r) = item else { continue };
        let payload = book_record_to_json(&r);
        if let Ok(s) = serde_json::to_string(&payload) {
            if socket.send(Message::Text(s.into())).await.is_err() {
                break;
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Scenarios — list, start, advance, stop. The lisp side populates the
// registry via `(define-scenario …)`; these endpoints read it and
// invoke the named stage defun via the shared TulispContext.
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct ScenarioJson {
    name: String,
    description: String,
    stages: Vec<String>,
    current_stage: Option<usize>,
    started_at: Option<String>,
    stage_entered_at: Option<String>,
}

fn scenario_to_json(e: &ScenarioEntry) -> ScenarioJson {
    ScenarioJson {
        name: e.def.name.clone(),
        description: e.def.description.clone(),
        stages: e.def.stages.iter().map(|s| s.name.clone()).collect(),
        current_stage: e.runtime.current_stage,
        started_at: e.runtime.started_at.map(|t| t.to_rfc3339()),
        stage_entered_at: e.runtime.stage_entered_at.map(|t| t.to_rfc3339()),
    }
}

async fn api_scenarios(State(s): State<UiState>) -> Json<Vec<ScenarioJson>> {
    let Some(reg) = s.scenarios.as_ref() else {
        return Json(Vec::new());
    };
    let mut out: Vec<ScenarioJson> = reg.lock().values().map(scenario_to_json).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

async fn api_scenario_start(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    advance_scenario(&s, &name, |rt, n| {
        if n == 0 {
            return None;
        }
        let now = chrono::Utc::now();
        rt.current_stage = Some(0);
        rt.started_at = Some(now);
        rt.stage_entered_at = Some(now);
        Some(0)
    })
}

async fn api_scenario_next(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    advance_scenario(&s, &name, |rt, n| {
        let cur = rt.current_stage?;
        if cur + 1 >= n {
            return None;
        }
        rt.current_stage = Some(cur + 1);
        rt.stage_entered_at = Some(chrono::Utc::now());
        Some(cur + 1)
    })
}

async fn api_scenario_stop(
    State(s): State<UiState>,
    Path(name): Path<String>,
) -> Result<Json<ScenarioJson>, StatusCode> {
    let Some(reg) = s.scenarios.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let mut guard = reg.lock();
    let Some(entry) = guard.get_mut(&name) else {
        return Err(StatusCode::NOT_FOUND);
    };
    entry.runtime = ScenarioRuntime::default();
    Ok(Json(scenario_to_json(entry)))
}

fn advance_scenario<F>(
    s: &UiState,
    name: &str,
    update: F,
) -> Result<Json<ScenarioJson>, StatusCode>
where
    F: FnOnce(&mut ScenarioRuntime, usize) -> Option<usize>,
{
    let (fn_name, payload) = {
        let reg = s.scenarios.as_ref().ok_or(StatusCode::NOT_FOUND)?;
        let mut guard = reg.lock();
        let entry = guard.get_mut(name).ok_or(StatusCode::NOT_FOUND)?;
        let n = entry.def.stages.len();
        let idx = update(&mut entry.runtime, n).ok_or(StatusCode::BAD_REQUEST)?;
        (entry.def.stages[idx].fn_name.clone(), scenario_to_json(entry))
    };
    invoke_stage_fn(s, &fn_name);
    Ok(Json(payload))
}

fn invoke_stage_fn(s: &UiState, fn_name: &str) {
    let Some(ctx) = s.tulisp_ctx.as_ref() else {
        return;
    };
    let expr = format!("({fn_name})");
    let mut guard = ctx.borrow_mut();
    if let Err(e) = guard.eval_string(&expr) {
        log::warn!("scenario stage {fn_name}: {}", e.format(&guard));
    }
}
