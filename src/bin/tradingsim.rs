//! Headless tradingsim simulator. Loads `config.lisp` if present
//! (registers the gridpool, builds market-makers, ...); otherwise
//! default-shaped market-maker liquidity so `tsctl place` has
//! something to trade against on a fresh checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use parking_lot::{Mutex, RwLock};
use tonic::transport::Server;
use tradingsim::{
    lisp::{Config as LispConfig, MmFleetSpec},
    proto::trading::electricity_trading_service_server::ElectricityTradingServiceServer,
    server::ElectricityTradingServer,
    sim::counterparty::MmFleetParams,
    sim::fleet::FleetManager,
    sim::gridpool::Gridpool,
    sim::market::{Area, MarketRegistry, MarketRules},
    sim::order::GridpoolId,
    sim::world::World,
    ui as ui_server,
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

    // Load lisp config if present; otherwise build a defaults-only
    // Config so the rest of the bootstrap doesn't have to branch
    // on Option<LispConfig> everywhere.
    let lisp_config = if Path::new(&cfg_path).exists() {
        match LispConfig::new(cfg_path.to_str().unwrap()) {
            Ok(c) => {
                log::info!("Loaded config from {}", cfg_path.display());
                c
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
        LispConfig::with_defaults()
    };

    let mut markets = MarketRegistry::new();
    let lisp_market_rules = lisp_config.market_rules();
    if !lisp_market_rules.is_empty() {
        log::info!(
            "Registering {} market(s) from config.lisp",
            lisp_market_rules.len()
        );
        for rules in lisp_market_rules {
            markets.insert(rules);
        }
    } else {
        log::info!("No markets in lisp config — registering default fallback market");
        markets.insert(MarketRules::default_for_tests());
    }
    let area = Area::eic("10YDE-EON------1");
    let mut world = World::new(markets);

    // Apply lisp-declared SIDC couplings before any orders flow.
    {
        use rust_decimal::Decimal;
        for cs in lisp_config.couplings() {
            let offset = std::time::Duration::from_secs(cs.gate_offset_seconds.max(0) as u64);
            let capacity = cs
                .capacity_mw
                .filter(|v| *v > 0.0)
                .and_then(|v| Decimal::try_from(v).ok());
            log::info!(
                "Coupling: {} <-> {} (gate offset {} s, capacity {})",
                cs.area_a,
                cs.area_b,
                offset.as_secs(),
                capacity
                    .map(|c| format!("{c} MWh"))
                    .unwrap_or_else(|| "unlimited".into())
            );
            world.add_coupling(Area::eic(cs.area_a), Area::eic(cs.area_b), offset, capacity);
        }
        // Share the lisp-side market-suspended flag so
        // `(suspend-market)` actually rejects future submissions.
        world.set_market_suspended_handle(lisp_config.market_suspended());
    }

    let gridpool_specs = lisp_config.gridpools();
    if !gridpool_specs.is_empty() {
        for gp in gridpool_specs {
            let areas = gp.area_codes.iter().map(|c| Area::eic(c)).collect();
            log::info!(
                "Registered gridpool {} \"{}\" ({} area(s))",
                gp.id,
                gp.name,
                gp.area_codes.len()
            );
            world.register_gridpool(
                Gridpool::new(GridpoolId(gp.id), gp.name, areas)
                    .with_self_trade_policy(gp.self_trade_policy),
            );
        }
    } else {
        log::info!("No gridpools in lisp config — registering hardcoded gridpool 1 (single test area)");
        world.register_gridpool(Gridpool::new(GridpoolId(1), "default", vec![area.clone()]));
    }

    let trading_addr = lisp_config.trading_addr();

    // Drain tulisp-async timers (every / run-with-timer) on a fixed
    // cadence so scheduled callbacks in config.lisp actually fire.
    // Also spawn the notify-rs file watcher so edits to config.lisp
    // (or any (watch-file …) entry) trigger an automatic reload.
    let lisp_config = Arc::new(lisp_config);
    lisp_config.spawn_timer_loop(Duration::from_millis(100));
    lisp_config.spawn_file_watcher();

    let world = Arc::new(RwLock::new(world));

    // Per-contract counterparties. FleetManager spawns one MM (and
    // N aggressors) per delivery contract in each fleet's rolling
    // window, and rotates them every 15 min so contracts gating off
    // are retired and fresh ones at the far edge come online.
    let manager = Arc::new(Mutex::new(FleetManager::new(
        Arc::clone(&world),
        lisp_config.curve(),
        lisp_config.weather(),
        lisp_config.clock(),
    )));
    let mm_fleets = lisp_config.mm_fleets();
    if mm_fleets.is_empty() {
        log::info!("No MM fleets in lisp config — spawning default fallback fleet");
        manager.lock().add_mm_fleet(MmFleetSpec {
            name: "default".into(),
            area: area.code.clone(),
            window_quarters: 16,
            shared_params: Arc::new(RwLock::new(MmFleetParams::default())),
            seed_base: 0,
        });
    } else {
        log::info!("Spawning {} MM fleet(s) from config.lisp", mm_fleets.len());
        for spec in mm_fleets {
            manager.lock().add_mm_fleet(spec);
        }
    }
    let aggressor_fleets = lisp_config.aggressor_fleets();
    if !aggressor_fleets.is_empty() {
        log::info!(
            "Spawning {} aggressor fleet(s) from config.lisp",
            aggressor_fleets.len()
        );
        for spec in aggressor_fleets {
            manager.lock().add_aggressor_fleet(spec);
        }
    }
    let mm_views_handle = manager.lock().mm_views();
    let aggressor_views_handle = manager.lock().aggressor_views();
    FleetManager::start_lifecycle_task(Arc::clone(&manager));

    // Bias tick — applies the natural duck curve to every aggressor
    // (sets side_bias) and every MM (sets demand + surplus) every 5
    // s. When a scenario is active, its stage bias blends in on top
    // weighted by quarter-offset decay. The MM tilts give an
    // immediate quote shift on top of the slower follow-last-trade
    // drift, so visible price moves land within seconds rather than
    // minutes. Views are read from FleetManager's live registries so
    // the tick automatically picks up newly-spawned contracts.
    tradingsim::scenarios::spawn_bias_tick(
        aggressor_views_handle,
        mm_views_handle,
        lisp_config.scenarios(),
        lisp_config.bias_scale(),
        lisp_config.curve(),
        lisp_config.weather(),
        lisp_config.clock(),
        Duration::from_secs(5),
    );

    // Expire orders whose valid_until has lapsed. Once a second is
    // generous given the proto's UTC-timestamp resolution.
    {
        let world_for_expiry = Arc::clone(&world);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let n = world_for_expiry
                    .write()
                    .expire_lapsed_orders(Utc::now());
                if n > 0 {
                    log::info!("Expired {n} order(s) on the valid_until deadline");
                }
            }
        });
    }

    // Drain the lisp-side recall queue. Each entry is an order id
    // the lisp layer asked to force-cancel with actor=System; pop
    // and apply against the World.
    {
        let world_for_recall = Arc::clone(&world);
        let queue = lisp_config.recall_queue();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tick.tick().await;
                let drained: Vec<u64> = {
                    let mut g = queue.lock();
                    g.drain(..).collect()
                };
                if drained.is_empty() {
                    continue;
                }
                let mut w = world_for_recall.write();
                for id in drained {
                    let order_id = tradingsim::sim::order::OrderId(id);
                    match w.recall_order(order_id, Utc::now()) {
                        Ok(_) => log::info!("recall: order#{id} cancelled by system"),
                        Err(e) => log::warn!("recall: order#{id} failed: {e:?}"),
                    }
                }
            }
        });
    }

    // UI server: spawns alongside the gRPC server, sharing the same
    // World handle. Address pulled from config.lisp via
    // (set-ui-addr "…"); defaults to 127.0.0.1:8811.
    {
        let world_for_ui = Arc::clone(&world);
        let scenarios = Some(lisp_config.scenarios());
        let weather = Some(lisp_config.weather());
        let clock = lisp_config.clock();
        let ui_addr_str = lisp_config.ui_addr();
        let ui_addr: std::net::SocketAddr = ui_addr_str.parse().unwrap_or_else(|e| {
            log::error!("Invalid ui-addr {ui_addr_str:?} ({e}); falling back to 127.0.0.1:8811");
            "127.0.0.1:8811".parse().unwrap()
        });
        tokio::spawn(async move {
            if let Err(e) =
                ui_server::serve(ui_addr, world_for_ui, scenarios, weather, clock).await
            {
                log::error!("UI server exited: {e}");
            }
        });
    }

    // Weather forecast service: exposes the sim's internal weather
    // state via the Frequenz weather API. Sibling port to the
    // electricity-trading gRPC, configurable via
    // (set-weather-socket-addr "…").
    {
        use tradingsim::proto::weather::weather_forecast_service_server::WeatherForecastServiceServer;
        use tradingsim::weather_server::WeatherForecastServer;
        let weather_handle = lisp_config.weather();
        let cadence_handle = lisp_config.weather_cadence();
        let weather_addr_str = lisp_config.weather_addr();
        let weather_addr: std::net::SocketAddr = weather_addr_str.parse().unwrap_or_else(|e| {
            log::error!(
                "Invalid weather-socket-addr {weather_addr_str:?} ({e}); falling back to [::1]:8820"
            );
            "[::1]:8820".parse().unwrap()
        });
        tokio::spawn(async move {
            let service = WeatherForecastServiceServer::new(
                WeatherForecastServer::new(weather_handle).with_cadence(cadence_handle),
            );
            log::info!("WeatherForecast gRPC server listening on {weather_addr}");
            if let Err(e) = Server::builder()
                .add_service(service)
                .serve(weather_addr)
                .await
            {
                log::error!("Weather server exited: {e}");
            }
        });
    }

    let service =
        ElectricityTradingServiceServer::new(ElectricityTradingServer::new(Arc::clone(&world)));

    let addr = trading_addr.parse().unwrap_or_else(|e| {
        log::error!("invalid trading-addr {trading_addr:?}: {e}");
        std::process::exit(1);
    });
    log::info!("ElectricityTrading gRPC server listening on {addr}");
    Server::builder()
        .add_service(service)
        .serve(addr)
        .await
        .unwrap();
}
