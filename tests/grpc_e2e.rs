//! End-to-end gRPC round-trip: spawn the ElectricityTrading service
//! on a random port, drive it through the generated client. Each
//! `#[tokio::test]` builds a fresh World so tests don't share state.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use tradingsim::proto::common::grid::{
    DeliveryArea, DeliveryDuration, DeliveryPeriod, EnergyMarketCodeType,
};
use tradingsim::proto::common::market::{Power, Price, price::Currency as PrCurrency};
use tradingsim::proto::common::types::Decimal as PrDecimal;
use tradingsim::proto::trading::{
    CancelAllGridpoolOrdersRequest, CancelGridpoolOrderRequest, CreateGridpoolOrderRequest,
    GetGridpoolOrderRequest, GridpoolOrderFilter, ListGridpoolOrdersRequest,
    ListGridpoolTradesRequest, MarketSide, Order, OrderExecutionOption, OrderState, OrderType,
    ReceiveGridpoolOrdersStreamRequest, ReceiveGridpoolTradesStreamRequest,
    ReceivePublicTradesStreamRequest, UpdateGridpoolOrderRequest,
    electricity_trading_service_client::ElectricityTradingServiceClient,
    electricity_trading_service_server::ElectricityTradingServiceServer,
    update_gridpool_order_request::UpdateOrder,
};
use tradingsim::server::ElectricityTradingServer;
use tradingsim::sim::gridpool::Gridpool;
use tradingsim::sim::market::{Area, MarketRegistry, MarketRules};
use tradingsim::sim::order::GridpoolId;
use tradingsim::sim::world::World;

async fn spawn_server() -> String {
    let mut markets = MarketRegistry::new();
    markets.insert(MarketRules::de_lu());
    let mut world = World::new(markets);
    // Several tests cross a buy + sell from the same pool to
    // exercise the matcher / trade streams. The runtime default
    // is now Reject; opt back into Allow here so those tests
    // keep their self-cross setup.
    world.register_gridpool(
        Gridpool::new(
            GridpoolId(1),
            "test",
            vec![Area::eic("10Y1001A1001A82H")],
        )
        .with_self_trade_policy(tradingsim::sim::gridpool::SelfTradePolicy::Allow),
    );
    let world = Arc::new(RwLock::new(world));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);

    let svc = ElectricityTradingServiceServer::new(ElectricityTradingServer::new(world));
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    // Give the spawn a tick to start accepting.
    tokio::task::yield_now().await;
    format!("http://{addr}")
}

fn dec(s: &str) -> PrDecimal {
    PrDecimal {
        value: s.to_string(),
    }
}

fn price(amount: &str) -> Price {
    Price {
        amount: Some(dec(amount)),
        currency: PrCurrency::Eur as i32,
    }
}

fn power(amount: &str) -> Power {
    Power {
        mw: Some(dec(amount)),
    }
}

fn de_lu() -> DeliveryArea {
    DeliveryArea {
        code: "10Y1001A1001A82H".into(),
        code_type: EnergyMarketCodeType::EuropeEic as i32,
    }
}

fn hour_at_noon() -> DeliveryPeriod {
    // Far-future timestamp so the gate stays open regardless of when
    // the test binary runs: 2099-01-01T12:00:00Z = 4070908800.
    DeliveryPeriod {
        start: Some(prost_types::Timestamp {
            seconds: 4070908800,
            nanos: 0,
        }),
        duration: DeliveryDuration::DeliveryDuration15 as i32,
    }
}

fn limit_order(side: MarketSide, p: &str, qty: &str) -> Order {
    Order {
        delivery_area: Some(de_lu()),
        delivery_period: Some(hour_at_noon()),
        r#type: OrderType::Limit as i32,
        side: side as i32,
        price: Some(price(p)),
        quantity: Some(power(qty)),
        stop_price: None,
        peak_price_delta: None,
        display_quantity: None,
        execution_option: None,
        valid_until: None,
        payload: None,
        tag: None,
    }
}

#[tokio::test]
async fn place_then_list_then_cancel() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    // Place a resting buy.
    let created = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap()
        .into_inner();
    let order_id = created.order_detail.unwrap().order_id;

    // List shows the active order.
    let listed = client
        .list_gridpool_orders(ListGridpoolOrdersRequest {
            gridpool_id: 1,
            filter: None,
            pagination_params: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.order_details.len(), 1);
    assert_eq!(listed.order_details[0].order_id, order_id);
    assert_eq!(
        listed.order_details[0].state_detail.as_ref().unwrap().state,
        OrderState::Active as i32
    );

    // Cancel; state flips to Canceled.
    let cancelled = client
        .cancel_gridpool_order(CancelGridpoolOrderRequest {
            gridpool_id: 1,
            order_id,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        cancelled
            .order_detail
            .as_ref()
            .unwrap()
            .state_detail
            .as_ref()
            .unwrap()
            .state,
        OrderState::Canceled as i32
    );

    // Get still finds it (terminal but queryable).
    let got = client
        .get_gridpool_order(GetGridpoolOrderRequest {
            gridpool_id: 1,
            order_id,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        got.order_detail.unwrap().state_detail.unwrap().state,
        OrderState::Canceled as i32
    );
}

#[tokio::test]
async fn place_crossing_orders_produces_fill() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    let _buy = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap()
        .into_inner();
    let sell = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "1.0")),
        })
        .await
        .unwrap()
        .into_inner();
    // Taker fully filled.
    let sell_state = sell.order_detail.unwrap().state_detail.unwrap().state;
    assert_eq!(sell_state, OrderState::Filled as i32);

    // Both orders now visible; both terminal.
    let listed = client
        .list_gridpool_orders(ListGridpoolOrdersRequest {
            gridpool_id: 1,
            filter: None,
            pagination_params: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.order_details.len(), 2);
    for d in &listed.order_details {
        assert_eq!(
            d.state_detail.as_ref().unwrap().state,
            OrderState::Filled as i32
        );
    }
}

#[tokio::test]
async fn stream_receives_live_updates() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr.clone())
        .await
        .unwrap();

    let mut stream = client
        .receive_gridpool_orders_stream(ReceiveGridpoolOrdersStreamRequest {
            gridpool_id: 1,
            filter: Some(GridpoolOrderFilter::default()),
        })
        .await
        .unwrap()
        .into_inner();

    // Send a place from a separate client (the streaming client is
    // committed to the stream's receive task).
    let mut placer = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    let placed = placer
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap()
        .into_inner();
    let placed_id = placed.order_detail.unwrap().order_id;

    let evt = stream
        .next()
        .await
        .expect("expected at least one stream item")
        .expect("stream item should be Ok");
    assert_eq!(evt.order_detail.as_ref().unwrap().order_id, placed_id);
}

#[tokio::test]
async fn validation_rejection_returns_invalid_argument() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    // Off-grid quantity (1.05 isn't a multiple of 0.1 MW — wait, it IS).
    // Use 1.05 EUR price (not a multiple of 0.01... also IS). Real off-grid: 85.005 price.
    let bad = Order {
        price: Some(price("85.005")),
        ..limit_order(MarketSide::Buy, "85.00", "1.0")
    };
    let err = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(bad),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn modify_crosses_spread_after_price_bump() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    // Resting sell @ 85.
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "1.0")),
        })
        .await
        .unwrap();

    // Buy @ 84 (below the ask) — should rest, not fill.
    let placed = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "84.00", "1.0")),
        })
        .await
        .unwrap()
        .into_inner();
    let buy_id = placed.order_detail.unwrap().order_id;

    // Modify to 86 — now crosses the resting sell.
    let after = client
        .update_gridpool_order(UpdateGridpoolOrderRequest {
            gridpool_id: 1,
            order_id: buy_id,
            update_mask: None,
            update_order_fields: Some(UpdateOrder {
                price: Some(price("86.00")),
                ..Default::default()
            }),
        })
        .await
        .unwrap()
        .into_inner();
    let detail = after.order_detail.unwrap();
    assert_eq!(
        detail.state_detail.as_ref().unwrap().state,
        OrderState::Filled as i32
    );
}

#[tokio::test]
async fn fok_insufficient_depth_cancels_no_fills() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    // Resting sell @ 85, 0.5 MW.
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "0.5")),
        })
        .await
        .unwrap();

    // FOK buy @ 85 for 1.0 MW: needs full depth, has 0.5 → kill.
    let fok_order = Order {
        execution_option: Some(OrderExecutionOption::Fok as i32),
        ..limit_order(MarketSide::Buy, "85.00", "1.0")
    };
    let resp = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(fok_order),
        })
        .await
        .unwrap()
        .into_inner();
    let detail = resp.order_detail.unwrap();
    assert_eq!(
        detail.state_detail.as_ref().unwrap().state,
        OrderState::Canceled as i32
    );
    // 0 filled — FOK never partials.
    let filled = detail
        .filled_quantity
        .as_ref()
        .and_then(|p| p.mw.as_ref())
        .map(|d| d.value.clone());
    assert_eq!(filled.unwrap(), "0");
}

#[tokio::test]
async fn cancel_all_terminates_every_active_order_for_the_pool() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    // Place three resting buys at non-crossing prices so they all
    // sit on the book.
    for price_str in ["80.00", "81.00", "82.00"] {
        client
            .create_gridpool_order(CreateGridpoolOrderRequest {
                gridpool_id: 1,
                order: Some(limit_order(MarketSide::Buy, price_str, "1.0")),
            })
            .await
            .unwrap();
    }
    let resp = client
        .cancel_all_gridpool_orders(CancelAllGridpoolOrdersRequest { gridpool_id: 1 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.gridpool_id, 1);
    // Every order ends up in a terminal state — list with no filter
    // should show 3 entries all Canceled.
    let listed = client
        .list_gridpool_orders(ListGridpoolOrdersRequest {
            gridpool_id: 1,
            filter: None,
            pagination_params: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.order_details.len(), 3);
    for d in &listed.order_details {
        assert_eq!(
            d.state_detail.as_ref().unwrap().state,
            OrderState::Canceled as i32
        );
    }
}

#[tokio::test]
async fn cancel_all_unknown_gridpool_returns_not_found() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    let err = client
        .cancel_all_gridpool_orders(CancelAllGridpoolOrdersRequest { gridpool_id: 9999 })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn list_gridpool_trades_returns_completed_fills() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    // Cross two orders so a trade lands on the pool's trade index.
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap();
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "1.0")),
        })
        .await
        .unwrap();
    let listed = client
        .list_gridpool_trades(ListGridpoolTradesRequest {
            gridpool_id: 1,
            filter: None,
            pagination_params: None,
        })
        .await
        .unwrap()
        .into_inner();
    // Single self-cross yields one Trade row (buyer + seller are
    // the same pool, but the trade is recorded once).
    assert!(
        !listed.trades.is_empty(),
        "expected at least one trade after crossing fills"
    );
}

#[tokio::test]
async fn receive_public_trades_stream_emits_each_fill() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    let mut stream = client
        .receive_public_trades_stream(ReceivePublicTradesStreamRequest {
            filter: None,
            start_time: None,
            end_time: None,
        })
        .await
        .unwrap()
        .into_inner();

    // Generate one public trade.
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap();
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "1.0")),
        })
        .await
        .unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("public trade timeout")
        .expect("stream end")
        .expect("status");
    let t = msg.public_trade.expect("public trade present");
    // The price round-trips through rust_decimal which normalises
    // trailing zeroes off — "85.00" comes back as "85".
    assert_eq!(t.price.as_ref().unwrap().amount.as_ref().unwrap().value, "85");
}

#[tokio::test]
async fn receive_gridpool_trades_stream_emits_local_fill() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();
    let mut stream = client
        .receive_gridpool_trades_stream(ReceiveGridpoolTradesStreamRequest {
            gridpool_id: 1,
            filter: None,
        })
        .await
        .unwrap()
        .into_inner();
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap();
    client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 1,
            order: Some(limit_order(MarketSide::Sell, "85.00", "1.0")),
        })
        .await
        .unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("gridpool trade timeout")
        .expect("stream end")
        .expect("status");
    assert!(msg.trade.is_some());
}

#[tokio::test]
async fn unknown_gridpool_returns_not_found() {
    let addr = spawn_server().await;
    let mut client = ElectricityTradingServiceClient::connect(addr)
        .await
        .unwrap();

    let err = client
        .create_gridpool_order(CreateGridpoolOrderRequest {
            gridpool_id: 9999,
            order: Some(limit_order(MarketSide::Buy, "85.00", "1.0")),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}
