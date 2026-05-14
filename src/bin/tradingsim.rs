//! Headless tradingsim simulator. Loads `config.lisp` if present
//! (registers the gridpool, builds market-makers, ...); otherwise
//! falls back to a single hard-coded DE-LU gridpool + four hours of
//! default-shaped market-maker liquidity so `tsctl place` has
//! something to trade against on a fresh checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use tokio::sync::Mutex;
use tonic::transport::Server;
use tradingsim::{
    lisp::Config as LispConfig,
    proto::trading::electricity_trading_service_server::ElectricityTradingServiceServer,
    server::ElectricityTradingServer,
    sim::counterparty::{MarketMaker, MarketMakerConfig},
    sim::gridpool::Gridpool,
    sim::market::{Area, DeliveryDuration, DeliveryPeriod, MarketRegistry, MarketRules},
    sim::order::GridpoolId,
    sim::world::World,
};

const MM_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Fallback MM coverage when no config.lisp is loaded.
const MM_HOURS_COVERED: i64 = 4;

fn next_hour_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let bucket = (secs / 3600 + 1) * 3600;
    DateTime::from_timestamp(bucket, 0).unwrap()
}

/// Spawn one tokio task that ticks `mm.refresh(...)` every
/// `MM_REFRESH_INTERVAL` against the shared world.
fn spawn_mm_task(world: Arc<Mutex<World>>, mut mm: MarketMaker) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MM_REFRESH_INTERVAL);
        loop {
            tick.tick().await;
            let mut w = world.lock().await;
            mm.refresh(&mut w, Utc::now());
        }
    });
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

    // Load lisp config if present; if absent or invalid, log and fall
    // back to hardcoded defaults.
    let lisp_config = if Path::new(&cfg_path).exists() {
        match LispConfig::new(cfg_path.to_str().unwrap()) {
            Ok(c) => {
                log::info!("Loaded config from {}", cfg_path.display());
                Some(c)
            }
            Err(e) => {
                log::error!("Config load failed:\n{e}");
                std::process::exit(1);
            }
        }
    } else {
        log::warn!(
            "Config file {} not present — using hardcoded defaults",
            cfg_path.display()
        );
        None
    };

    let mut markets = MarketRegistry::new();
    markets.insert(MarketRules::de_lu());
    let area = Area::eic("10Y1001A1001A82H");
    let mut world = World::new(markets);

    let gridpool_specs = lisp_config
        .as_ref()
        .map(|c| c.gridpools())
        .unwrap_or_default();
    if !gridpool_specs.is_empty() {
        for gp in gridpool_specs {
            let areas = gp.area_codes.iter().map(|c| Area::eic(c)).collect();
            log::info!(
                "Registered gridpool {} \"{}\" ({} area(s))",
                gp.id,
                gp.name,
                gp.area_codes.len()
            );
            world.register_gridpool(Gridpool::new(GridpoolId(gp.id), gp.name, areas));
        }
    } else {
        log::info!("No gridpools in lisp config — registering hardcoded gridpool 1 (DE-LU)");
        world.register_gridpool(Gridpool::new(GridpoolId(1), "default", vec![area.clone()]));
    }

    let socket_addr = lisp_config
        .as_ref()
        .map(|c| c.socket_addr())
        .unwrap_or_else(|| "[::1]:8810".to_string());

    // Drain tulisp-async timers (every / run-with-timer) on a fixed
    // cadence so scheduled callbacks in config.lisp actually fire.
    if let Some(c) = lisp_config.as_ref() {
        c.spawn_timer_loop(Duration::from_millis(100));
    }

    let world = Arc::new(Mutex::new(world));

    // Synthetic liquidity: either driven by lisp's (make-market-maker
    // …) entries, or — when no config.lisp is loaded — the prior
    // four-hour fallback so the demo still has a quoted book.
    let mm_specs = lisp_config
        .as_ref()
        .map(|c| c.market_makers())
        .unwrap_or_default();

    if !mm_specs.is_empty() {
        log::info!("Spawning {} market-maker(s) from config.lisp", mm_specs.len());
        for spec in mm_specs {
            let cfg_now = spec.shared_config.read();
            log::info!(
                "  {} @ {}: ref {} spread {} demand {} surplus {}",
                spec.name,
                cfg_now.period.start.format("%Y-%m-%dT%H:%M:%SZ"),
                cfg_now.reference_price,
                cfg_now.spread,
                cfg_now.demand,
                cfg_now.surplus,
            );
            drop(cfg_now);
            let mm = MarketMaker::with_shared_config(spec.shared_config, spec.seed);
            spawn_mm_task(Arc::clone(&world), mm);
        }
    } else {
        log::info!(
            "No market-makers in lisp config — spawning {} hardcoded MMs",
            MM_HOURS_COVERED
        );
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
            let mm = MarketMaker::new(cfg, (hour_offset as u64).wrapping_mul(0x9E37_79B9));
            spawn_mm_task(Arc::clone(&world), mm);
        }
    }

    let service =
        ElectricityTradingServiceServer::new(ElectricityTradingServer::new(Arc::clone(&world)));

    let addr = socket_addr.parse().unwrap_or_else(|e| {
        log::error!("invalid socket_addr {socket_addr:?}: {e}");
        std::process::exit(1);
    });
    log::info!("ElectricityTrading gRPC server listening on {addr}");
    Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .unwrap();
}
