//! Headless tradingsim simulator. Builds a World with the DE-LU
//! defaults and one hard-coded gridpool, then serves the
//! ElectricityTrading gRPC API. The lisp-driven config loader lands
//! in Phase 5; for now the `config_path` arg is parsed but ignored.

use std::path::PathBuf;
use std::sync::Arc;

use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use tokio::sync::Mutex;
use tonic::transport::Server;
use tradingsim::{
    proto::trading::electricity_trading_service_server::ElectricityTradingServiceServer,
    server::ElectricityTradingServer,
    sim::gridpool::Gridpool,
    sim::market::{Area, MarketRegistry, MarketRules},
    sim::order::GridpoolId,
    sim::world::World,
};

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
    let mut world = World::new(markets);
    world.register_gridpool(Gridpool::new(
        GridpoolId(1),
        "default",
        vec![Area::eic("10Y1001A1001A82H")],
    ));
    log::info!(
        "Registered gridpool {} (DE-LU) with {} market(s)",
        1,
        world.markets().len()
    );

    let world = Arc::new(Mutex::new(world));
    let service = ElectricityTradingServiceServer::new(ElectricityTradingServer::new(world));

    let addr = "[::1]:8810".parse().unwrap();
    log::info!("ElectricityTrading gRPC server listening on {addr}");
    Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .unwrap();
}
