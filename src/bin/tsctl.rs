//! Client CLI for tradingsim. Phase 4: info / place / get / cancel /
//! cancel-all / orders (with optional --live stream). Talks to the
//! gRPC server defaults on [::1]:8810.

use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rust_decimal::Decimal;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

use tradingsim::proto::common::grid::{DeliveryDuration, DeliveryPeriod};
use tradingsim::proto::common::market::{Power, Price, price::Currency as PrCurrency};
use tradingsim::proto::common::types::Decimal as PrDecimal;
use tradingsim::proto::trading::{
    CancelAllGridpoolOrdersRequest, CancelGridpoolOrderRequest, CreateGridpoolOrderRequest,
    GetGridpoolOrderRequest, GridpoolOrderFilter, GridpoolTradeFilter, ListGridpoolOrdersRequest,
    ListGridpoolTradesRequest, MarketSide, Order, OrderState, OrderType, PublicTradeFilter,
    ReceiveGridpoolOrdersStreamRequest, ReceiveGridpoolTradesStreamRequest,
    ReceivePublicTradesStreamRequest, TradeState, UpdateGridpoolOrderRequest,
    electricity_trading_service_client::ElectricityTradingServiceClient,
    update_gridpool_order_request::UpdateOrder,
};

#[derive(Parser, Debug)]
#[command(name = "tsctl", version, about = "tradingsim client")]
struct Cli {
    /// gRPC endpoint for the tradingsim server.
    #[arg(long, default_value = "http://[::1]:8810")]
    addr: String,

    /// HTTP endpoint for the UI server (scenarios live here, not on
    /// the gRPC channel).
    #[arg(long, default_value = "http://127.0.0.1:8811")]
    ui_addr: String,

    /// gRPC endpoint for the WeatherForecastService (sibling of the
    /// trading service; binary defaults to [::1]:8820).
    #[arg(long, default_value = "http://[::1]:8820")]
    weather_addr: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show client + endpoint info.
    Info,
    /// Place a LIMIT order on a gridpool.
    Place {
        #[arg(long)]
        pool: u64,
        #[arg(long, value_enum)]
        side: SideArg,
        // Real intraday markets admit negative prices under supply
        // gluts. clap rejects "--price -5" by default because the
        // leading hyphen looks like a flag; allow_hyphen_values
        // lets the matcher see the negative number as a value.
        #[arg(long, allow_hyphen_values = true)]
        price: String,
        #[arg(long)]
        qty: String,
        /// Delivery period start. Accepts:
        ///   - "next"         : next 15-min boundary
        ///   - "+N"           : N quarters from the next boundary
        ///   - RFC-3339 UTC   : e.g. 2026-05-14T12:00:00Z
        #[arg(long, default_value = "next")]
        start: String,
        /// Delivery duration in minutes. Only 15 is admitted today.
        #[arg(long, default_value_t = 15)]
        duration: u32,
        /// User-defined tag for grouping.
        #[arg(long)]
        tag: Option<String>,
        /// Delivery area code (EIC). Defaults to TenneT (the largest
        /// of the four German TSO zones the sample config registers).
        #[arg(long, default_value = "10YDE-EON------1")]
        area: String,
        /// Execution restriction (fok / ioc). Default: none (resting).
        #[arg(long, value_enum)]
        exec: Option<ExecArg>,
    },
    /// Fetch a single order.
    Get {
        #[arg(long)]
        pool: u64,
        order: u64,
    },
    /// Modify a resting order's price / quantity / tag.
    Modify {
        #[arg(long)]
        pool: u64,
        order: u64,
        #[arg(long, allow_hyphen_values = true)]
        price: Option<String>,
        #[arg(long)]
        qty: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Cancel a single order.
    Cancel {
        #[arg(long)]
        pool: u64,
        order: u64,
    },
    /// Cancel every non-terminal order on a gridpool.
    CancelAll {
        #[arg(long)]
        pool: u64,
    },
    /// List orders on a gridpool, or stream updates with --live.
    Orders {
        #[arg(long)]
        pool: u64,
        #[arg(long, value_enum)]
        side: Option<SideArg>,
        #[arg(long, value_enum)]
        state: Vec<StateArg>,
        /// Switch from List (one-shot) to ReceiveGridpoolOrdersStream.
        #[arg(long)]
        live: bool,
    },
    /// List trades on a gridpool, or stream them with --live.
    Trades {
        #[arg(long)]
        pool: u64,
        #[arg(long, value_enum)]
        side: Option<SideArg>,
        #[arg(long)]
        live: bool,
    },
    /// Stream the public trade tape (one event per match, all gridpools).
    PublicTrades {
        /// Optional buy-side delivery area filter (EIC code).
        #[arg(long)]
        buy_area: Option<String>,
        /// Optional sell-side delivery area filter (EIC code).
        #[arg(long)]
        sell_area: Option<String>,
    },
    /// Stream the public order book event tape (one record per
    /// resting-order state change).
    PublicBook {
        /// Optional delivery area filter (EIC code).
        #[arg(long)]
        area: Option<String>,
        #[arg(long, value_enum)]
        side: Option<SideArg>,
    },
    /// Inspect or drive a time-of-day scenario via the UI server.
    Scenarios {
        #[command(subcommand)]
        action: ScenariosAction,
    },
    /// Print the next live frame from the WeatherForecastService —
    /// one row per registered location × forecast horizon. Add
    /// --live to keep the stream open.
    Weather {
        /// One or more "lat,lon" pairs (0.1° grid) to request.
        /// Omit to receive every registered location.
        #[arg(long, value_parser = parse_latlon, num_args = 0..)]
        location: Vec<(f32, f32)>,
        /// Stream live frames instead of exiting after the first.
        #[arg(long)]
        live: bool,
    },
}

fn parse_latlon(s: &str) -> Result<(f32, f32), String> {
    let (lat, lon) = s
        .split_once(',')
        .ok_or_else(|| format!("expected \"lat,lon\"; got {s:?}"))?;
    let lat: f32 = lat.parse().map_err(|e| format!("bad lat {lat:?}: {e}"))?;
    let lon: f32 = lon.parse().map_err(|e| format!("bad lon {lon:?}: {e}"))?;
    Ok((lat, lon))
}

#[derive(Subcommand, Debug)]
enum ScenariosAction {
    /// List every registered scenario and its current state.
    List,
    /// Activate a scenario at the wallclock-matching stage.
    Start { name: String },
    /// Advance one stage forward.
    Next { name: String },
    /// Step one stage backward.
    Prev { name: String },
    /// Jump to a specific stage index (0-based).
    Jump { name: String, idx: usize },
    /// Deactivate a scenario; aggressors fall back to the natural curve.
    Stop { name: String },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SideArg {
    Buy,
    Sell,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ExecArg {
    Fok,
    Ioc,
}

impl From<SideArg> for MarketSide {
    fn from(s: SideArg) -> Self {
        match s {
            SideArg::Buy => MarketSide::Buy,
            SideArg::Sell => MarketSide::Sell,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum StateArg {
    Pending,
    Active,
    Filled,
    Canceled,
    Expired,
    Failed,
    Hibernate,
}

impl From<StateArg> for OrderState {
    fn from(s: StateArg) -> Self {
        match s {
            StateArg::Pending => OrderState::Pending,
            StateArg::Active => OrderState::Active,
            StateArg::Filled => OrderState::Filled,
            StateArg::Canceled => OrderState::Canceled,
            StateArg::Expired => OrderState::Expired,
            StateArg::Failed => OrderState::Failed,
            StateArg::Hibernate => OrderState::Hibernate,
        }
    }
}

fn duration_proto(mins: u32) -> Result<DeliveryDuration, String> {
    match mins {
        5 => Ok(DeliveryDuration::DeliveryDuration5),
        15 => Ok(DeliveryDuration::DeliveryDuration15),
        30 => Ok(DeliveryDuration::DeliveryDuration30),
        60 => Ok(DeliveryDuration::DeliveryDuration60),
        _ => Err(format!("invalid duration {mins} (allowed: 5, 15, 30, 60)")),
    }
}

fn decimal_proto(s: &str) -> Result<PrDecimal, String> {
    let _ = Decimal::from_str(s).map_err(|e| format!("bad decimal {s:?}: {e}"))?;
    Ok(PrDecimal {
        value: s.to_string(),
    })
}

fn timestamp_proto(s: &str) -> Result<prost_types::Timestamp, String> {
    let dt = parse_period_start(s, Utc::now())?;
    Ok(prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    })
}

/// Parse the --start argument. Three shapes:
///   - "next"      → next 15-min boundary
///   - "+N"        → N quarters past the next boundary
///   - RFC-3339    → as written
fn parse_period_start(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let next_boundary = || {
        let bucket = (now.timestamp() / 900 + 1) * 900;
        DateTime::from_timestamp(bucket, 0).unwrap()
    };
    if s == "next" {
        return Ok(next_boundary());
    }
    if let Some(rest) = s.strip_prefix('+') {
        let n: i64 = rest
            .parse()
            .map_err(|e| format!("bad quarter offset {rest:?}: {e}"))?;
        let base = next_boundary();
        return Ok(base + chrono::Duration::minutes(15 * n));
    }
    DateTime::parse_from_rfc3339(s)
        .map_err(|e| format!("bad timestamp {s:?}: {e}"))
        .map(|dt| dt.with_timezone(&Utc))
}

/// Format a proto Timestamp as "HH:MM:SS" UTC for compact line output.
fn fmt_short_time(ts: &Option<prost_types::Timestamp>) -> String {
    ts.as_ref()
        .and_then(|t| Utc.timestamp_opt(t.seconds, t.nanos as u32).single())
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "?".into())
}

fn render_trade_line(t: &tradingsim::proto::trading::Trade) -> String {
    let side = MarketSide::try_from(t.side)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| "?".into());
    let area = t
        .delivery_area
        .as_ref()
        .map(|a| a.code.clone())
        .unwrap_or_else(|| "?".into());
    let price = t
        .price
        .as_ref()
        .and_then(|p| p.amount.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let qty = t
        .quantity
        .as_ref()
        .and_then(|p| p.mw.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let period = fmt_short_time(&t.delivery_period.as_ref().and_then(|p| p.start));
    format!(
        "trade#{} order#{} {side} {qty} MW @ {price} EUR (delivery {period}, {area})",
        t.id, t.order_id
    )
}

fn render_public_trade_line(t: &tradingsim::proto::trading::PublicTrade) -> String {
    let buy = t
        .buy_delivery_area
        .as_ref()
        .map(|a| a.code.clone())
        .unwrap_or_else(|| "?".into());
    let sell = t
        .sell_delivery_area
        .as_ref()
        .map(|a| a.code.clone())
        .unwrap_or_else(|| "?".into());
    let price = t
        .price
        .as_ref()
        .and_then(|p| p.amount.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let qty = t
        .quantity
        .as_ref()
        .and_then(|p| p.mw.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let cross = if buy == sell {
        buy.clone()
    } else {
        format!("{buy} -> {sell}")
    };
    let period = fmt_short_time(&t.delivery_period.as_ref().and_then(|p| p.start));
    format!(
        "public#{} {qty} MW @ {price} EUR (delivery {period}, {cross})",
        t.id
    )
}

fn render_order_line(d: &tradingsim::proto::trading::OrderDetail) -> String {
    let id = d.order_id;
    let state = d
        .state_detail
        .as_ref()
        .and_then(|s| OrderState::try_from(s.state).ok())
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "?".into());
    let side = d
        .order
        .as_ref()
        .and_then(|o| MarketSide::try_from(o.side).ok())
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "?".into());
    let price = d
        .order
        .as_ref()
        .and_then(|o| o.price.as_ref())
        .and_then(|p| p.amount.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let qty = d
        .order
        .as_ref()
        .and_then(|o| o.quantity.as_ref())
        .and_then(|p| p.mw.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let open = d
        .open_quantity
        .as_ref()
        .and_then(|p| p.mw.as_ref())
        .map(|a| a.value.clone())
        .unwrap_or_else(|| "?".into());
    let period = fmt_short_time(
        &d.order
            .as_ref()
            .and_then(|o| o.delivery_period.as_ref())
            .and_then(|p| p.start),
    );
    format!("#{id} {side} {qty} MW @ {price} EUR (open {open}, delivery {period}) [{state}]")
}

async fn connect(addr: &str) -> Result<ElectricityTradingServiceClient<Channel>, String> {
    ElectricityTradingServiceClient::connect(addr.to_string())
        .await
        .map_err(|e| format!("connect {addr}: {e}"))
}

fn render_scenario_brief(s: &serde_json::Value) -> String {
    let name = s["name"].as_str().unwrap_or("?");
    let cur = s["current_stage"].as_u64();
    let stages = s["stages"].as_array().map(|a| a.len()).unwrap_or(0);
    let manual = s["manual_override"].as_bool().unwrap_or(false);
    let wc = s["wallclock_stage"].as_u64();
    let stage_label = cur
        .map(|c| {
            let label = s["stages"]
                .get(c as usize)
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("{}/{} {}", c + 1, stages, label)
        })
        .unwrap_or_else(|| "idle".into());
    let wc_label = wc
        .map(|w| {
            let label = s["stages"]
                .get(w as usize)
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("now={}", label)
        })
        .unwrap_or_default();
    let manual_label = if manual { " [manual]" } else { "" };
    let desc = s["description"].as_str().unwrap_or("");
    format!("{name:24} {stage_label}{manual_label} {wc_label}  — {desc}")
}

async fn cmd_scenarios(ui_addr: &str, action: ScenariosAction) -> Result<(), String> {
    let client = reqwest::Client::new();
    let base = ui_addr.trim_end_matches('/');
    let (method, path) = match &action {
        ScenariosAction::List => ("GET", "/api/scenarios".to_string()),
        ScenariosAction::Start { name } => ("POST", format!("/api/scenarios/{name}/start")),
        ScenariosAction::Next { name } => ("POST", format!("/api/scenarios/{name}/next")),
        ScenariosAction::Prev { name } => ("POST", format!("/api/scenarios/{name}/prev")),
        ScenariosAction::Jump { name, idx } => {
            ("POST", format!("/api/scenarios/{name}/jump/{idx}"))
        }
        ScenariosAction::Stop { name } => ("POST", format!("/api/scenarios/{name}/stop")),
    };
    let url = format!("{base}{path}");
    let req = match method {
        "GET" => client.get(&url),
        _ => client.post(&url),
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{method} {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{method} {url}: HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("parse response: {e}"))?;
    match action {
        ScenariosAction::List => {
            let arr = body.as_array().ok_or("expected JSON array")?;
            if arr.is_empty() {
                println!("no scenarios registered");
            } else {
                for s in arr {
                    println!("{}", render_scenario_brief(s));
                }
            }
        }
        _ => println!("{}", render_scenario_brief(&body)),
    }
    Ok(())
}

async fn cmd_place(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    side: SideArg,
    price: String,
    qty: String,
    start: String,
    duration: u32,
    tag: Option<String>,
    area: String,
    exec: Option<ExecArg>,
) -> Result<(), String> {
    use tradingsim::proto::trading::OrderExecutionOption;
    let exec_i32 = exec.map(|e| match e {
        ExecArg::Fok => OrderExecutionOption::Fok as i32,
        ExecArg::Ioc => OrderExecutionOption::Ioc as i32,
    });
    let order = Order {
        delivery_area: Some(tradingsim::proto::common::grid::DeliveryArea {
            code: area,
            code_type: tradingsim::proto::common::grid::EnergyMarketCodeType::EuropeEic as i32,
        }),
        delivery_period: Some(DeliveryPeriod {
            start: Some(timestamp_proto(&start)?),
            duration: duration_proto(duration)? as i32,
        }),
        r#type: OrderType::Limit as i32,
        side: MarketSide::from(side) as i32,
        price: Some(Price {
            amount: Some(decimal_proto(&price)?),
            currency: PrCurrency::Eur as i32,
        }),
        quantity: Some(Power {
            mw: Some(decimal_proto(&qty)?),
        }),
        stop_price: None,
        peak_price_delta: None,
        display_quantity: None,
        execution_option: exec_i32,
        valid_until: None,
        payload: None,
        tag,
    };
    let resp = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: pool,
            order: Some(order),
        })
        .await
        .map_err(|e| format!("create: {e}"))?
        .into_inner();
    println!("{}", render_order_line(resp.order_detail.as_ref().unwrap()));
    Ok(())
}

async fn cmd_get(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    order: u64,
) -> Result<(), String> {
    let resp = client
        .get_gridpool_order(GetGridpoolOrderRequest {
            gridpool_id: pool,
            order_id: order,
        })
        .await
        .map_err(|e| format!("get: {e}"))?
        .into_inner();
    println!("{}", render_order_line(resp.order_detail.as_ref().unwrap()));
    Ok(())
}

async fn cmd_modify(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    order: u64,
    price: Option<String>,
    qty: Option<String>,
    tag: Option<String>,
) -> Result<(), String> {
    let mut update = UpdateOrder::default();
    if let Some(p) = price {
        update.price = Some(Price {
            amount: Some(decimal_proto(&p)?),
            currency: PrCurrency::Eur as i32,
        });
    }
    if let Some(q) = qty {
        update.quantity = Some(Power {
            mw: Some(decimal_proto(&q)?),
        });
    }
    if let Some(t) = tag {
        update.tag = Some(t);
    }
    let resp = client
        .update_gridpool_order(UpdateGridpoolOrderRequest {
            gridpool_id: pool,
            order_id: order,
            update_mask: None,
            update_order_fields: Some(update),
        })
        .await
        .map_err(|e| format!("modify: {e}"))?
        .into_inner();
    println!("{}", render_order_line(resp.order_detail.as_ref().unwrap()));
    Ok(())
}

async fn cmd_cancel(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    order: u64,
) -> Result<(), String> {
    let resp = client
        .cancel_gridpool_order(CancelGridpoolOrderRequest {
            gridpool_id: pool,
            order_id: order,
        })
        .await
        .map_err(|e| format!("cancel: {e}"))?
        .into_inner();
    println!("{}", render_order_line(resp.order_detail.as_ref().unwrap()));
    Ok(())
}

async fn cmd_cancel_all(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
) -> Result<(), String> {
    let resp = client
        .cancel_all_gridpool_orders(CancelAllGridpoolOrdersRequest { gridpool_id: pool })
        .await
        .map_err(|e| format!("cancel-all: {e}"))?
        .into_inner();
    println!("cancelled all open orders on gridpool {}", resp.gridpool_id);
    Ok(())
}

async fn cmd_trades(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    side: Option<SideArg>,
    live: bool,
) -> Result<(), String> {
    let filter = GridpoolTradeFilter {
        states: vec![TradeState::Active as i32],
        trade_ids: vec![],
        side: side.map(|s| MarketSide::from(s) as i32),
        delivery_time_filter: None,
        delivery_area: None,
        tag: None,
    };
    if live {
        let mut stream = client
            .receive_gridpool_trades_stream(ReceiveGridpoolTradesStreamRequest {
                gridpool_id: pool,
                filter: Some(filter),
            })
            .await
            .map_err(|e| format!("trades stream: {e}"))?
            .into_inner();
        while let Some(item) = stream.next().await {
            let resp = item.map_err(|e| format!("stream: {e}"))?;
            println!("{}", render_trade_line(resp.trade.as_ref().unwrap()));
        }
    } else {
        let resp = client
            .list_gridpool_trades(ListGridpoolTradesRequest {
                gridpool_id: pool,
                filter: Some(filter),
                pagination_params: None,
            })
            .await
            .map_err(|e| format!("trades list: {e}"))?
            .into_inner();
        for t in resp.trades {
            println!("{}", render_trade_line(&t));
        }
    }
    Ok(())
}

async fn cmd_public_book(
    client: &mut ElectricityTradingServiceClient<Channel>,
    area: Option<String>,
    side: Option<SideArg>,
) -> Result<(), String> {
    use tradingsim::proto::trading::{PublicOrderBookFilter, ReceivePublicOrderBookStreamRequest};
    let mk_area = |code: String| tradingsim::proto::common::grid::DeliveryArea {
        code,
        code_type: tradingsim::proto::common::grid::EnergyMarketCodeType::EuropeEic as i32,
    };
    let filter = PublicOrderBookFilter {
        delivery_period: None,
        delivery_area: area.map(mk_area),
        side: side.map(|s| MarketSide::from(s) as i32),
    };
    let mut stream = client
        .receive_public_order_book_stream(ReceivePublicOrderBookStreamRequest {
            filter: Some(filter),
            start_time: None,
            end_time: None,
        })
        .await
        .map_err(|e| format!("public-book stream: {e}"))?
        .into_inner();
    while let Some(item) = stream.next().await {
        let resp = item.map_err(|e| format!("stream: {e}"))?;
        for r in &resp.public_order_book_records {
            let side = MarketSide::try_from(r.side)
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|_| "?".into());
            let area = r
                .delivery_area
                .as_ref()
                .map(|a| a.code.clone())
                .unwrap_or_else(|| "?".into());
            let price = r
                .price
                .as_ref()
                .and_then(|p| p.amount.as_ref())
                .map(|a| a.value.clone())
                .unwrap_or_else(|| "?".into());
            let qty = r
                .quantity
                .as_ref()
                .and_then(|p| p.mw.as_ref())
                .map(|a| a.value.clone())
                .unwrap_or_else(|| "?".into());
            let period = fmt_short_time(&r.delivery_period.as_ref().and_then(|p| p.start));
            println!(
                "book#{} {side} {qty} MW @ {price} EUR (delivery {period}, {area})",
                r.id
            );
        }
    }
    Ok(())
}

async fn cmd_public_trades(
    client: &mut ElectricityTradingServiceClient<Channel>,
    buy_area: Option<String>,
    sell_area: Option<String>,
) -> Result<(), String> {
    let mk_area = |code: String| tradingsim::proto::common::grid::DeliveryArea {
        code,
        code_type: tradingsim::proto::common::grid::EnergyMarketCodeType::EuropeEic as i32,
    };
    let filter = PublicTradeFilter {
        states: vec![TradeState::Active as i32],
        delivery_period: None,
        buy_delivery_area: buy_area.map(mk_area),
        sell_delivery_area: sell_area.map(mk_area),
    };
    let mut stream = client
        .receive_public_trades_stream(ReceivePublicTradesStreamRequest {
            filter: Some(filter),
            start_time: None,
            end_time: None,
        })
        .await
        .map_err(|e| format!("public-trades stream: {e}"))?
        .into_inner();
    while let Some(item) = stream.next().await {
        let resp = item.map_err(|e| format!("stream: {e}"))?;
        println!(
            "{}",
            render_public_trade_line(resp.public_trade.as_ref().unwrap())
        );
    }
    Ok(())
}

async fn cmd_orders(
    client: &mut ElectricityTradingServiceClient<Channel>,
    pool: u64,
    side: Option<SideArg>,
    state: Vec<StateArg>,
    live: bool,
) -> Result<(), String> {
    let filter = GridpoolOrderFilter {
        states: state
            .into_iter()
            .map(|s| OrderState::from(s) as i32)
            .collect(),
        side: side.map(|s| MarketSide::from(s) as i32),
        delivery_time_filter: None,
        delivery_area: None,
        tag: None,
        order_ids: vec![],
    };
    if live {
        let mut stream = client
            .receive_gridpool_orders_stream(ReceiveGridpoolOrdersStreamRequest {
                gridpool_id: pool,
                filter: Some(filter),
            })
            .await
            .map_err(|e| format!("stream: {e}"))?
            .into_inner();
        while let Some(item) = stream.next().await {
            let resp = item.map_err(|e| format!("stream: {e}"))?;
            println!("{}", render_order_line(resp.order_detail.as_ref().unwrap()));
        }
    } else {
        let resp = client
            .list_gridpool_orders(ListGridpoolOrdersRequest {
                gridpool_id: pool,
                filter: Some(filter),
                pagination_params: None,
            })
            .await
            .map_err(|e| format!("list: {e}"))?
            .into_inner();
        for d in resp.order_details {
            println!("{}", render_order_line(&d));
        }
    }
    Ok(())
}

async fn cmd_weather(addr: &str, locations: Vec<(f32, f32)>, live: bool) -> Result<(), String> {
    use tradingsim::proto::common_v1::Location;
    use tradingsim::proto::weather::{
        ForecastFeature, LocationForecast, ReceiveLiveWeatherForecastRequest,
        weather_forecast_service_client::WeatherForecastServiceClient,
    };
    let mut client = WeatherForecastServiceClient::connect(addr.to_string())
        .await
        .map_err(|e| format!("weather connect ({addr}): {e}"))?;
    let req = ReceiveLiveWeatherForecastRequest {
        locations: locations
            .into_iter()
            .map(|(lat, lon)| Location {
                latitude: lat,
                longitude: lon,
                country_code: String::new(),
            })
            .collect(),
        features: vec![],
        forecast_horizon: None,
    };
    let mut stream = client
        .receive_live_weather_forecast(req)
        .await
        .map_err(|e| format!("weather stream: {e}"))?
        .into_inner();
    let feature_label = |f: i32| -> &'static str {
        match ForecastFeature::try_from(f).ok() {
            Some(ForecastFeature::SurfaceSolarRadiationDownwards) => "solar W/m²",
            Some(ForecastFeature::UWindComponent100Metre) => "u100 m/s",
            Some(ForecastFeature::VWindComponent100Metre) => "v100 m/s",
            Some(ForecastFeature::Temperature2Metre) => "t2m K",
            _ => "?",
        }
    };
    let print_frame = |lfs: &[LocationForecast]| {
        for lf in lfs {
            let loc = lf
                .location
                .as_ref()
                .map(|l| format!("{:.1},{:.1}", l.latitude, l.longitude))
                .unwrap_or_else(|| "(unbound)".into());
            for (i, h) in lf.forecasts.iter().enumerate() {
                let ts = fmt_short_time(&h.valid_time);
                let parts: Vec<String> = h
                    .features
                    .iter()
                    .map(|f| format!("{}={:.1}", feature_label(f.feature), f.value))
                    .collect();
                println!("{loc} +{i:02}h {ts}  {}", parts.join("  "));
            }
        }
    };
    let first = stream
        .next()
        .await
        .ok_or_else(|| "no frame received".to_string())?
        .map_err(|e| format!("stream: {e}"))?;
    print_frame(&first.location_forecasts);
    if !live {
        return Ok(());
    }
    while let Some(item) = stream.next().await {
        let frame = item.map_err(|e| format!("stream: {e}"))?;
        println!("---");
        print_frame(&frame.location_forecasts);
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let result: Result<(), String> = async {
        match cli.cmd {
            Cmd::Info => {
                println!("tsctl v{}", env!("CARGO_PKG_VERSION"));
                println!("gRPC endpoint: {}", cli.addr);
                println!("UI   endpoint: {}", cli.ui_addr);
                Ok(())
            }
            Cmd::Scenarios { action } => cmd_scenarios(&cli.ui_addr, action).await,
            Cmd::Weather { location, live } => cmd_weather(&cli.weather_addr, location, live).await,
            cmd => {
                let mut client = connect(&cli.addr).await?;
                match cmd {
                    Cmd::Info | Cmd::Scenarios { .. } | Cmd::Weather { .. } => unreachable!(),
                    Cmd::Place {
                        pool,
                        side,
                        price,
                        qty,
                        start,
                        duration,
                        tag,
                        area,
                        exec,
                    } => {
                        cmd_place(
                            &mut client,
                            pool,
                            side,
                            price,
                            qty,
                            start,
                            duration,
                            tag,
                            area,
                            exec,
                        )
                        .await
                    }
                    Cmd::Get { pool, order } => cmd_get(&mut client, pool, order).await,
                    Cmd::Modify {
                        pool,
                        order,
                        price,
                        qty,
                        tag,
                    } => cmd_modify(&mut client, pool, order, price, qty, tag).await,
                    Cmd::Cancel { pool, order } => cmd_cancel(&mut client, pool, order).await,
                    Cmd::CancelAll { pool } => cmd_cancel_all(&mut client, pool).await,
                    Cmd::Orders {
                        pool,
                        side,
                        state,
                        live,
                    } => cmd_orders(&mut client, pool, side, state, live).await,
                    Cmd::Trades { pool, side, live } => {
                        cmd_trades(&mut client, pool, side, live).await
                    }
                    Cmd::PublicTrades {
                        buy_area,
                        sell_area,
                    } => cmd_public_trades(&mut client, buy_area, sell_area).await,
                    Cmd::PublicBook { area, side } => {
                        cmd_public_book(&mut client, area, side).await
                    }
                }
            }
        }
    }
    .await;
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
