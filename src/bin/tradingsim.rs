//! Headless tradingsim simulator. Builds a World with the DE-LU
//! defaults and one hard-coded gridpool, spawns synthetic liquidity
//! covering the next few hour-contracts, then serves the
//! ElectricityTrading gRPC API. The lisp-driven config loader lands
//! in Phase 5; for now the `config_path` arg is parsed but ignored.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use tokio::sync::Mutex;
use tonic::transport::Server;
use tradingsim::{
    proto::trading::electricity_trading_service_server::ElectricityTradingServiceServer,
    server::ElectricityTradingServer,
    sim::counterparty::{MarketMaker, MarketMakerConfig},
    sim::gridpool::Gridpool,
    sim::market::{Area, DeliveryDuration, DeliveryPeriod, MarketRegistry, MarketRules},
    sim::order::GridpoolId,
    sim::world::World,
};

const MM_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Number of consecutive hour-contracts a market-maker covers
/// starting from the next hour boundary. tsctl users picking any
/// hour in this window will find a quoted book.
const MM_HOURS_COVERED: i64 = 4;

fn next_hour_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let bucket = (secs / 3600 + 1) * 3600;
    DateTime::from_timestamp(bucket, 0).unwrap()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    TermLogger::init(
        LevelFilter::Info,
        LogConfig::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.lisp".to_string());
    let cfg_path = PathBuf::from(cfg_path);
    log::info!("tradingsim v{} starting", env!("CARGO_PKG_VERSION"));
    log::warn!(
        "Phase 4 demo: ignoring config at {} (lisp loader lands in Phase 5)",
        cfg_path.display()
    );

    let mut markets = MarketRegistry::new();
    markets.insert(MarketRules::de_lu());
    let area = Area::eic("10Y1001A1001A82H");
    let mut world = World::new(markets);
    world.register_gridpool(Gridpool::new(GridpoolId(1), "default", vec![area.clone()]));
    log::info!(
        "Registered gridpool {} (DE-LU) with {} market(s)",
        1,
        world.markets().len()
    );

    let world = Arc::new(Mutex::new(world));

    // Synthetic liquidity: one MarketMaker per hour-contract for the
    // next MM_HOURS_COVERED hours. Each refreshes its bid/ask every
    // MM_REFRESH_INTERVAL.
    let first_hour = next_hour_boundary(Utc::now());
    for hour_offset in 0..MM_HOURS_COVERED {
        let start = first_hour + chrono::Duration::hours(hour_offset);
        let period = DeliveryPeriod {
            start,
            duration: DeliveryDuration::DeliveryDuration60,
        };
        let cfg = MarketMakerConfig::de_lu_default(area.clone(), period);
        log::info!(
            "Market-maker quoting {} hour @ ref {} EUR/MWh (spread {})",
            start.format("%Y-%m-%dT%H:%M:%SZ"),
            cfg.reference_price,
            cfg.spread
        );
        let mut mm = MarketMaker::new(cfg, (hour_offset as u64).wrapping_mul(0x9E37_79B9));
        let world_for_task = Arc::clone(&world);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(MM_REFRESH_INTERVAL);
            loop {
                tick.tick().await;
                let mut w = world_for_task.lock().await;
                mm.refresh(&mut w, Utc::now());
            }
        });
    }

    let service =
        ElectricityTradingServiceServer::new(ElectricityTradingServer::new(Arc::clone(&world)));

    let addr = "[::1]:8810".parse().unwrap();
    log::info!("ElectricityTrading gRPC server listening on {addr}");
    Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .unwrap();
}
