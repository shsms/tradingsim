//! Minimal axum-driven UI. One HTML page (embedded via `include_str!`)
//! + a couple of /api/* JSON endpoints + two WebSockets for the live
//! public-trade and public-book streams. Spawned by the binary in
//! its own task; talks to the shared World handle.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::http::header;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::proto::trading::{MarketSide, PublicOrderBookRecord};
use crate::sim::trade::PublicTrade;
use crate::sim::world::World;

#[derive(Clone)]
struct UiState {
    world: Arc<Mutex<World>>,
}

pub async fn serve(addr: SocketAddr, world: Arc<Mutex<World>>) -> std::io::Result<()> {
    let state = UiState { world };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/info", get(api_info))
        .route("/api/gridpools", get(api_gridpools))
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
    let rx = s.world.lock().await.subscribe_public_book();
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
