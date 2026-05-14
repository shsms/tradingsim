//! Headless tradingsim simulator. Loads `config.lisp` if present
//! (registers the gridpool, builds market-makers, ...); otherwise
//! default-shaped market-maker liquidity so `tsctl place` has
//! something to trade against on a fresh checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use simplelog::{ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use parking_lot::RwLock;
use tonic::transport::Server;
use tradingsim::{
    lisp::Config as LispConfig,
    proto::trading::electricity_trading_service_server::ElectricityTradingServiceServer,
    server::ElectricityTradingServer,
    sim::counterparty::{Aggressor, MarketMaker, MarketMakerConfig},
    sim::gridpool::Gridpool,
    sim::market::{Area, DeliveryDuration, DeliveryPeriod, MarketRegistry, MarketRules},
    sim::order::GridpoolId,
    sim::world::World,
    ui as ui_server,
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
/// `MM_REFRESH_INTERVAL` against the shared world. `quarter_offset`
/// rolls the MM's delivery period forward each tick so the MM
/// always quotes the contract starting that many quarter-hours from
/// the next 15-min boundary.
fn spawn_mm_task(world: Arc<RwLock<World>>, mut mm: MarketMaker, quarter_offset: i64) {
    let cfg = mm.shared_config();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MM_REFRESH_INTERVAL);
        loop {
            tick.tick().await;
            let now = Utc::now();
            let new_start = tradingsim::lisp::next_quarter_boundary(now)
                + chrono::Duration::minutes(15 * quarter_offset);
            {
                let mut c = cfg.write();
                if c.period.start != new_start {
                    c.period.start = new_start;
                }
            }
            let mut w = world.write();
            mm.refresh(&mut w, now);
        }
    });
}

/// Spawn one tokio task that fires `ag.fire(...)` on a horizon-
/// scaled cadence. The base rate (from the lisp fleet config) is
/// the inter-fire delay at ≥2 h to gate; near gate it shortens by
/// up to 3× via `gate_close_scale`, the same curve the MM walk
/// amplitude uses. Combined with the per-fire size ramp inside
/// `Aggressor::fire`, total volume on a contract concentrates in
/// its last hour the way real intraday does (~70% there) instead
/// of being uniform across the contract's life.
fn spawn_aggressor_task(
    world: Arc<RwLock<World>>,
    mut ag: Aggressor,
    rate: Duration,
    quarter_offset: i64,
) {
    let cfg = ag.shared_config();
    let base_secs = rate.as_secs_f64();
    tokio::spawn(async move {
        loop {
            let now = Utc::now();
            let new_start = tradingsim::lisp::next_quarter_boundary(now)
                + chrono::Duration::minutes(15 * quarter_offset);
            {
                let mut c = cfg.write();
                if c.period.start != new_start {
                    c.period.start = new_start;
                }
            }
            // Sleep before the next fire — duration shrinks as the
            // contract's gate approaches. Floor at 50 ms so a
            // mis-set base rate can't busy-spin even with the
            // tightest 3× ramp at gate.
            let scale = tradingsim::sim::counterparty::gate_close_scale(new_start, now);
            let wait =
                Duration::from_secs_f64((base_secs / scale.max(1.0)).max(0.05));
            tokio::time::sleep(wait).await;
            let mut w = world.write();
            ag.fire(&mut w, Utc::now());
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

    // Synthetic liquidity: either driven by lisp's (make-market-maker
    // …) entries, or — when no config.lisp is loaded — the prior
    // four-hour fallback so the demo still has a quoted book.
    let mm_specs = lisp_config.market_makers();

    // Drain tulisp-async timers (every / run-with-timer) on a fixed
    // cadence so scheduled callbacks in config.lisp actually fire.
    // Also spawn the notify-rs file watcher so edits to config.lisp
    // (or any (watch-file …) entry) trigger an automatic reload.
    let lisp_config = Arc::new(lisp_config);
    lisp_config.spawn_timer_loop(Duration::from_millis(100));
    lisp_config.spawn_file_watcher();

    let world = Arc::new(RwLock::new(world));

    let mut mm_views: Vec<tradingsim::scenarios::MmView> = Vec::new();
    if !mm_specs.is_empty() {
        log::info!(
            "Spawning {} market-maker(s) from config.lisp",
            mm_specs.len()
        );
        for spec in mm_specs {
            let offset = spec.quarter_offset;
            mm_views.push(tradingsim::scenarios::MmView {
                quarter_offset: offset,
                shared_config: spec.shared_config.clone(),
            });
            let mm = MarketMaker::with_shared_config(spec.shared_config, spec.seed);
            spawn_mm_task(Arc::clone(&world), mm, offset);
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
                duration: DeliveryDuration::DeliveryDuration15,
            };
            let cfg = MarketMakerConfig::default_for(area.clone(), period);
            log::info!(
                "Market-maker quoting {} hour @ ref {} EUR/MWh (spread {})",
                start.format("%Y-%m-%dT%H:%M:%SZ"),
                cfg.reference_price,
                cfg.spread
            );
            let mm = MarketMaker::new(cfg, (hour_offset as u64).wrapping_mul(0x9E37_79B9));
            // Hardcoded fallback: hour_offset is in HOURS, but the
            // rolling task works in quarter-hours. Multiply by 4.
            spawn_mm_task(Arc::clone(&world), mm, hour_offset * 4);
        }
    }

    // Aggressors — non-gridpool takers that cross the MM's quotes
    // and generate public trades. Drives observable price activity
    // on the public trade tape.
    let aggressor_specs = lisp_config.aggressors();
    let mut bias_views: Vec<tradingsim::scenarios::AggressorView> = Vec::new();
    if !aggressor_specs.is_empty() {
        log::info!(
            "Spawning {} aggressor(s) from config.lisp",
            aggressor_specs.len()
        );
        for spec in aggressor_specs {
            let rate = Duration::from_millis(spec.rate_ms);
            let offset = spec.quarter_offset;
            bias_views.push(tradingsim::scenarios::AggressorView {
                quarter_offset: offset,
                shared_config: spec.shared_config.clone(),
            });
            let ag = Aggressor::with_shared_config(spec.shared_config, spec.seed);
            spawn_aggressor_task(Arc::clone(&world), ag, rate, offset);
        }
    }

    // Bias tick — applies the natural duck curve to every aggressor
    // (sets side_bias) and every MM (sets demand + surplus) every 5
    // s. When a scenario is active, its stage bias blends in on top
    // weighted by quarter-offset decay. The MM tilts give an
    // immediate quote shift on top of the slower follow-last-trade
    // drift, so visible price moves land within seconds rather than
    // minutes.
    // Seed each MM's reference from the curve+weather at boot so
    // the very first quote (within MM_REFRESH_INTERVAL) is
    // already on-curve, ahead of the first bias tick a few
    // seconds later.
    {
        let curve_handle = lisp_config.curve();
        let weather_handle = lisp_config.weather();
        let clock_handle = lisp_config.clock();
        let curve = curve_handle.read();
        let weather = weather_handle.read();
        let clock = clock_handle.read().clone();
        for view in &mm_views {
            let cfg_snap = view.shared_config.read();
            let period_start = cfg_snap.period.start;
            let area_code = cfg_snap.area.code.clone();
            drop(cfg_snap);
            // Period start in the configured *local* zone so the
            // curve lookup (peak/belly hours are local) and the
            // solar-elevation day-of-year both land on the right
            // value at boot — straight UTC was peaking around
            // CEST 14:00 in summer.
            let hour = clock.local_hour(period_start);
            let day = clock.local_day_of_year(period_start);
            let loc = weather.for_area(&area_code);
            // Seed both baseline (the bias tick's target) and
            // the live price so the first refresh quotes
            // on-curve before the first bias tick fires.
            let seeded = tradingsim::scenarios::effective_ref(&curve, loc, hour, day);
            let mut w = view.shared_config.write();
            w.reference_baseline = seeded;
            w.reference_price = seeded;
        }
    }

    if !bias_views.is_empty() || !mm_views.is_empty() {
        tradingsim::scenarios::spawn_bias_tick(
            bias_views,
            mm_views,
            lisp_config.scenarios(),
            lisp_config.bias_scale(),
            lisp_config.curve(),
            lisp_config.weather(),
            lisp_config.clock(),
            Duration::from_secs(5),
        );
    }

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
