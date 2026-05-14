//! Client CLI for tradingsim. Phase 4: info / place / get / cancel /
//! cancel-all / orders (with optional --live stream). Talks to the
//! gRPC server defaults on [::1]:8810.

use std::str::FromStr;

use chrono::{DateTime, Utc};
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
        #[arg(long)]
        price: String,
        #[arg(long)]
        qty: String,
        /// Delivery period start, UTC ISO-8601 e.g. 2026-05-13T12:00:00Z.
        #[arg(long)]
        start: String,
        /// Delivery duration in minutes: 5, 15, 30, or 60.
        #[arg(long, default_value_t = 60)]
        duration: u32,
        /// User-defined tag for grouping.
        #[arg(long)]
        tag: Option<String>,
        /// Delivery area code (EIC). Defaults to DE-LU.
        #[arg(long, default_value = "10Y1001A1001A82H")]
        area: String,
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
        #[arg(long)]
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
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SideArg {
    Buy,
    Sell,
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
    let dt = DateTime::parse_from_rfc3339(s)
        .map_err(|e| format!("bad timestamp {s:?}: {e}"))?
        .with_timezone(&Utc);
    Ok(prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    })
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
    format!(
        "trade#{} order#{} {side} {qty} MW @ {price} EUR ({area})",
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
    format!("public#{} {qty} MW @ {price} EUR ({cross})", t.id)
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
    format!("#{id} {side} {qty} MW @ {price} EUR (open {open}) [{state}]")
}

async fn connect(addr: &str) -> Result<ElectricityTradingServiceClient<Channel>, String> {
    ElectricityTradingServiceClient::connect(addr.to_string())
        .await
        .map_err(|e| format!("connect {addr}: {e}"))
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
) -> Result<(), String> {
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
        execution_option: None,
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
        states: state.into_iter().map(|s| OrderState::from(s) as i32).collect(),
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let result: Result<(), String> = async {
        match cli.cmd {
            Cmd::Info => {
                println!("tsctl v{}", env!("CARGO_PKG_VERSION"));
                println!("endpoint: {}", cli.addr);
                Ok(())
            }
            cmd => {
                let mut client = connect(&cli.addr).await?;
                match cmd {
                    Cmd::Info => unreachable!(),
                    Cmd::Place {
                        pool,
                        side,
                        price,
                        qty,
                        start,
                        duration,
                        tag,
                        area,
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
