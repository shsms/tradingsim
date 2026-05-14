//! Lisp config DSL. The interpreter is the configuration frontend;
//! simulation state lives on `World`. The Config object holds:
//!   - the tulisp context (interior-mutable for hot eval),
//!   - a Metadata block (socket addr, etc.) shared with the runtime,
//!   - per-component config specs (gridpools, market-makers) the
//!     binary reads at startup before constructing the World.
//!
//! Defuns are registered against an `Arc<RwLock<...>>`-fronted shared
//! state, so a `(set-*)` call from inside an `(every …)` callback —
//! which fires under the interpreter lock at tick time — can mutate
//! a value that the gRPC service or the market-maker task is reading
//! concurrently.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, Watcher};
use parking_lot::{Mutex, RwLock};
use rust_decimal::Decimal;
use tulisp::{AsPlist, Error, Plist, Plistable, SharedMut, TulispContext};

use crate::sim::counterparty::{
    AggressorConfig, MarketMakerConfig, SharedAggressorConfig, SharedConfig,
};
use crate::sim::market::{Area, Currency, DeliveryDuration, DeliveryPeriod, MarketRules};

/// Top-level identity + transport settings, set via lisp defuns.
#[derive(Clone, Debug)]
pub struct Metadata {
    pub socket_addr: String,
    pub physics_tick: Duration,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            socket_addr: "[::1]:8810".to_string(),
            physics_tick: Duration::from_millis(100),
        }
    }
}

/// One market-maker as configured by `(make-market-maker …)`. The
/// shared_config is what the running MM task reads each refresh;
/// runtime `(set-mm-* …)` defuns mutate it in place.
#[derive(Clone)]
pub struct MarketMakerSpec {
    pub name: String,
    pub shared_config: SharedConfig,
    /// Seed for the MM's RNG; surfaces in the binary's MM spawner so
    /// each MM is independently deterministic from a fixed lisp
    /// config.
    pub seed: u64,
    /// Quarter-hours from the next 15-min boundary. The spawn task
    /// rolls the SharedConfig's period forward each tick using this
    /// offset, so the MM always quotes the contract starting
    /// `quarter_offset` quarters from now.
    pub quarter_offset: i64,
}

/// One gridpool from `(make-gridpool …)`. Same shape the binary will
/// pass to `World::register_gridpool`.
///
/// `self_trade_policy` accepts the strings "allow" (default) or
/// "reject". The plan's longer-term shape uses an unquoted symbol
/// (`'reject`); for now strings keep the AsPlist plumbing simple
/// and parsing local to one place.
#[derive(Clone, Debug)]
pub struct GridpoolSpec {
    pub id: u64,
    pub name: String,
    pub area_codes: Vec<String>,
    pub self_trade_policy: crate::sim::gridpool::SelfTradePolicy,
}

/// One market from `(%make-market …)`. The binary builds a
/// `MarketRules` from this and registers it.
#[derive(Clone, Debug)]
pub struct MarketSpec {
    pub area_code: String,
    pub currency: Currency,
}

/// One SIDC-style coupling from `(%make-coupling …)`. The binary
/// calls `World::add_coupling` for each entry. `gate_offset_seconds`
/// is the cross-border-gate lead time before delivery; 0 for
/// intra-zone couplings (close at delivery), e.g. 3600 for SIDC
/// cross-border (close 60 min before delivery). `capacity_mw`
/// caps per-contract MWh that can flow across; `None` (negative
/// or omitted in lisp) means unlimited.
#[derive(Clone, Debug)]
pub struct CouplingSpec {
    pub area_a: String,
    pub area_b: String,
    pub gate_offset_seconds: i64,
    pub capacity_mw: Option<f64>,
}

/// One aggressor from `(%make-aggressor …)`. The binary spawns one
/// tokio task per spec that fires `Aggressor::fire(&mut world)` on
/// the configured cadence.
#[derive(Clone)]
pub struct AggressorSpec {
    pub name: String,
    pub shared_config: SharedAggressorConfig,
    pub seed: u64,
    pub rate_ms: u64,
    /// Quarter-hours from the next 15-min boundary; same semantics
    /// as [`MarketMakerSpec::quarter_offset`].
    pub quarter_offset: i64,
}

#[derive(Clone)]
pub struct Config {
    #[allow(dead_code)]
    filename: String,
    pub(crate) ctx: SharedMut<TulispContext>,
    metadata: Arc<RwLock<Metadata>>,
    market_makers: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
    gridpools: Arc<Mutex<Vec<GridpoolSpec>>>,
    markets: Arc<Mutex<Vec<MarketSpec>>>,
    couplings: Arc<Mutex<Vec<CouplingSpec>>>,
    aggressors: Arc<Mutex<HashMap<String, AggressorSpec>>>,
    scenarios: crate::scenarios::SharedScenarios,
    bias_scale: crate::scenarios::SharedBiasScale,
    curve: crate::scenarios::SharedCurve,
    weather: crate::sim::weather::SharedWeather,
    weather_cadence: crate::sim::weather::SharedWeatherCadence,
    market_suspended: Arc<RwLock<bool>>,
    recall_queue: Arc<Mutex<std::collections::VecDeque<u64>>>,
    /// Extra paths registered via `(watch-file PATH)`; the notify
    /// watcher reloads on any of them changing in addition to the
    /// top-level config file.
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    /// tulisp-async timer queue. The binary calls `spawn_timer_loop`
    /// to drain it on a tokio interval; without that, `(every …)` /
    /// `(run-with-timer …)` registrations from config.lisp would
    /// stay pending forever.
    timer_handle: tulisp_async::Handle,
    /// Anchor time for relative period offsets. Set at Config::new
    /// so that `(make-market-maker :quarter-offset N …)` always
    /// builds the same absolute period within one config-load.
    anchor: DateTime<Utc>,
}

impl Config {
    /// Build a config from `filename`. Returns the formatted lisp
    /// error on parse/eval failure — the caller (binary boot)
    /// decides whether to panic or fall back to defaults.
    pub fn new(filename: &str) -> Result<Self, String> {
        let mut ctx = TulispContext::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        let market_makers = Arc::new(Mutex::new(HashMap::new()));
        let gridpools = Arc::new(Mutex::new(Vec::new()));
        let markets = Arc::new(Mutex::new(Vec::new()));
        let couplings = Arc::new(Mutex::new(Vec::new()));
        let aggressors = Arc::new(Mutex::new(HashMap::new()));
        let scenarios = crate::scenarios::new_registry();
        let bias_scale = crate::scenarios::new_bias_scale();
        let curve = crate::scenarios::new_curve();
        let weather = crate::sim::weather::new_state();
        let weather_cadence = crate::sim::weather::new_cadence();
        let market_suspended = Arc::new(RwLock::new(false));
        let recall_queue = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let extra_watches = Arc::new(Mutex::new(HashSet::new()));
        let anchor = Utc::now();

        let load_dir: PathBuf = match Path::new(filename).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| format!("set_load_path({}): {e}", load_dir.display()))?;

        register_runtime(
            &mut ctx,
            metadata.clone(),
            market_makers.clone(),
            gridpools.clone(),
            markets.clone(),
            couplings.clone(),
            aggressors.clone(),
            scenarios.clone(),
            bias_scale.clone(),
            curve.clone(),
            weather.clone(),
            weather_cadence.clone(),
            market_suspended.clone(),
            recall_queue.clone(),
            extra_watches.clone(),
            load_dir.clone(),
            anchor,
        );

        // (every …), (run-with-timer …), (cancel-timer) — must be
        // registered before eval_file because the config may use
        // them at top level. TokioExecutor::new captures
        // Handle::current(), so Config::new must run inside a
        // tokio runtime (tests use #[tokio::test]).
        let timer_handle =
            tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        if let Err(e) = ctx.eval_file(filename) {
            return Err(e.format(&ctx));
        }

        Ok(Self {
            filename: filename.to_string(),
            ctx: SharedMut::new(ctx),
            metadata,
            market_makers,
            gridpools,
            markets,
            couplings,
            aggressors,
            scenarios,
            bias_scale,
            curve,
            weather,
            weather_cadence,
            market_suspended,
            recall_queue,
            extra_watches,
            timer_handle,
            anchor,
        })
    }

    /// Re-evaluate the config file against the existing context. The
    /// file is expected to call `(reset-state)` at the top so timers
    /// from the previous load are cancelled before new ones are
    /// installed. (%make-market-maker) calls update existing
    /// SharedConfigs by name rather than replacing them, so the
    /// running MM tasks pick up new knobs on their next refresh.
    pub fn reload(&self) -> Result<(), String> {
        let start = std::time::Instant::now();
        let mut ctx = self.ctx.borrow_mut();
        if let Err(e) = ctx.eval_file(&self.filename) {
            let formatted = e.format(&ctx);
            log::error!("Reload failed:\n{formatted}");
            return Err(formatted);
        }
        log::info!(
            "Reloaded config in {:.1}ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    /// Spawn a notify-rs watcher on the config file plus any path
    /// registered via `(watch-file …)`. On any modify event (with
    /// 150ms debounce), call `reload()`.
    pub fn spawn_file_watcher(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.watch().await;
        });
    }

    async fn watch(self: Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<notify::Event, notify::Error>>(8);
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    let _ = tx.send(res).await;
                });
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("notify watcher: {e}");
                return;
            }
        };
        if let Err(e) = watcher.watch(
            Path::new(&self.filename),
            notify::RecursiveMode::NonRecursive,
        ) {
            log::error!("watch {}: {e}", self.filename);
            return;
        }
        for path in self.extra_watches.lock().iter() {
            if let Err(e) = watcher.watch(path, notify::RecursiveMode::NonRecursive) {
                log::warn!("watch-file {}: {e}", path.display());
            }
        }

        const DEBOUNCE: Duration = Duration::from_millis(150);
        while let Some(res) = rx.recv().await {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    log::error!("watch error: {:?}", e);
                    return;
                }
            };
            if !matches!(event.kind, notify::EventKind::Modify(_)) {
                continue;
            }
            // Drain follow-up events arriving within DEBOUNCE.
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => {
                        log::error!("watch error: {:?}", e);
                        return;
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            let _ = self.reload();
        }
    }

    /// Spawn the drain task that ticks pending `(every …)` /
    /// `(run-with-timer …)` firings on a fixed cadence. Must be
    /// called from inside a tokio runtime; do this once after
    /// `Config::new`. Returns the JoinHandle so the binary can keep
    /// the task alive (drop it to stop firing).
    pub fn spawn_timer_loop(&self, cadence: Duration) -> tokio::task::JoinHandle<()> {
        let ctx = self.ctx.clone();
        let handle = self.timer_handle.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(cadence);
            loop {
                tick.tick().await;
                let mut guard = ctx.borrow_mut();
                handle.tick(&mut guard);
            }
        })
    }

    pub fn metadata(&self) -> Metadata {
        self.metadata.read().clone()
    }

    pub fn socket_addr(&self) -> String {
        self.metadata.read().socket_addr.clone()
    }

    /// Snapshot of all market-makers built by the lisp config. Each
    /// spec carries a SharedConfig handle the binary can hand to
    /// MarketMaker::with_shared_config.
    pub fn market_makers(&self) -> Vec<MarketMakerSpec> {
        self.market_makers.lock().values().cloned().collect()
    }

    pub fn gridpools(&self) -> Vec<GridpoolSpec> {
        self.gridpools.lock().clone()
    }

    pub fn markets(&self) -> Vec<MarketSpec> {
        self.markets.lock().clone()
    }

    pub fn couplings(&self) -> Vec<CouplingSpec> {
        self.couplings.lock().clone()
    }

    pub fn aggressors(&self) -> Vec<AggressorSpec> {
        self.aggressors.lock().values().cloned().collect()
    }

    /// Hand out the scenarios registry the lisp side populates via
    /// `(define-scenario …)`. The UI layer mutates the runtime state
    /// in this map when the browser hits the start/next/stop endpoints.
    pub fn scenarios(&self) -> crate::scenarios::SharedScenarios {
        self.scenarios.clone()
    }

    /// Hand out a clone of the shared TulispContext. Lets the UI
    /// layer invoke named stage defuns by evaluating `(fn-name)`.
    pub fn tulisp_ctx(&self) -> SharedMut<TulispContext> {
        self.ctx.clone()
    }

    /// Shared MM_BIAS_SCALE handle. `config.lisp` sets it via
    /// `(set-mm-bias-scale FLOAT)`; the bias tick reads it.
    pub fn bias_scale(&self) -> crate::scenarios::SharedBiasScale {
        self.bias_scale.clone()
    }

    /// Shared forward-curve handle. Lisp overrides per-hour base
    /// prices via `(set-forward-curve-base HOUR PRICE)`.
    pub fn curve(&self) -> crate::scenarios::SharedCurve {
        self.curve.clone()
    }

    /// Shared weather-state handle. Lisp tunes the parameters via
    /// `(set-weather-cloud-cover)` / `-mean-wind` / `-direction` /
    /// `-temperature-base`. Bias tick + future gRPC service read it.
    pub fn weather(&self) -> crate::sim::weather::SharedWeather {
        self.weather.clone()
    }

    /// Shared forecast-emit cadence for the gRPC weather service.
    /// Mutated by `(set-weather-stream-cadence-seconds N)`. The
    /// bin hands the clone to WeatherForecastServer::with_cadence
    /// so the next sleep cycle picks up the new value.
    pub fn weather_cadence(&self) -> crate::sim::weather::SharedWeatherCadence {
        self.weather_cadence.clone()
    }

    /// Shared market-suspended flag. Set via `(suspend-market)`,
    /// cleared via `(resume-market)`. The bin hands the clone to
    /// the World so validate_common can short-circuit submissions.
    pub fn market_suspended(&self) -> Arc<RwLock<bool>> {
        self.market_suspended.clone()
    }

    /// Queue of order ids the lisp side wants force-cancelled with
    /// actor=System (TSO recall). The bin drains this on a tokio
    /// task and calls World::recall_order for each.
    pub fn recall_queue(&self) -> Arc<Mutex<std::collections::VecDeque<u64>>> {
        self.recall_queue.clone()
    }

    /// Resolve `markets()` into proper MarketRules. Currencies default
    /// to EUR for any area that wasn't explicitly configured.
    pub fn market_rules(&self) -> Vec<MarketRules> {
        self.markets()
            .into_iter()
            .map(|m| MarketRules::for_area(Area::eic(&m.area_code), m.currency))
            .collect()
    }

    pub fn anchor(&self) -> DateTime<Utc> {
        self.anchor
    }
}

fn register_runtime(
    ctx: &mut TulispContext,
    metadata: Arc<RwLock<Metadata>>,
    market_makers: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
    gridpools: Arc<Mutex<Vec<GridpoolSpec>>>,
    markets: Arc<Mutex<Vec<MarketSpec>>>,
    couplings: Arc<Mutex<Vec<CouplingSpec>>>,
    aggressors: Arc<Mutex<HashMap<String, AggressorSpec>>>,
    scenarios: crate::scenarios::SharedScenarios,
    bias_scale: crate::scenarios::SharedBiasScale,
    curve: crate::scenarios::SharedCurve,
    weather: crate::sim::weather::SharedWeather,
    weather_cadence: crate::sim::weather::SharedWeatherCadence,
    market_suspended: Arc<RwLock<bool>>,
    recall_queue: Arc<Mutex<std::collections::VecDeque<u64>>>,
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    load_dir: PathBuf,
    anchor: DateTime<Utc>,
) {
    add_log_functions(ctx);
    register_time_helpers(ctx);
    register_metadata(ctx, metadata);
    register_markets(ctx, markets);
    register_couplings(ctx, couplings);
    register_gridpools(ctx, gridpools);
    register_market_makers(ctx, market_makers, anchor);
    register_aggressors(ctx, aggressors, anchor);
    register_scenarios(ctx, scenarios);
    register_bias_scale(ctx, bias_scale);
    register_curve(ctx, curve);
    register_weather(ctx, weather, weather_cadence);
    register_market_controls(ctx, market_suspended, recall_queue);
    register_watches(ctx, extra_watches, load_dir);
}

fn register_market_controls(
    ctx: &mut TulispContext,
    market_suspended: Arc<RwLock<bool>>,
    recall_queue: Arc<Mutex<std::collections::VecDeque<u64>>>,
) {
    let suspended = market_suspended.clone();
    ctx.defun("suspend-market", move || -> Result<bool, Error> {
        *suspended.write() = true;
        Ok(true)
    });
    ctx.defun("resume-market", move || -> Result<bool, Error> {
        *market_suspended.write() = false;
        Ok(false)
    });
    ctx.defun("recall-order", move |id: i64| -> Result<i64, Error> {
        if id <= 0 {
            return Err(Error::os_error(format!(
                "recall-order: ID must be positive, got {id}"
            )));
        }
        recall_queue.lock().push_back(id as u64);
        Ok(id)
    });
}

fn register_weather(
    ctx: &mut TulispContext,
    weather: crate::sim::weather::SharedWeather,
    cadence: crate::sim::weather::SharedWeatherCadence,
) {
    use crate::sim::weather::WeatherLocation;
    let cad = cadence.clone();
    ctx.defun(
        "set-weather-stream-cadence-seconds",
        move |value: f64| -> Result<f64, Error> {
            let secs = value.max(1.0);
            *cad.write() = std::time::Duration::from_secs_f64(secs);
            Ok(secs)
        },
    );
    // Keep the original handle alive for downstream code paths
    // even though the defun owns its clone.
    let _ = cadence;
    let w = weather.clone();
    ctx.defun(
        "set-weather-cloud-cover",
        move |value: f64| -> Result<f64, Error> {
            let mut g = w.write();
            let v = value.clamp(0.0, 1.0);
            let d = g.default_mut();
            d.cloud_cover = v;
            d.baseline_cloud_cover = v;
            Ok(value)
        },
    );
    let w = weather.clone();
    ctx.defun(
        "set-weather-mean-wind",
        move |value: f64| -> Result<f64, Error> {
            let mut g = w.write();
            let v = value.max(0.0);
            let d = g.default_mut();
            d.mean_wind = v;
            d.baseline_mean_wind = v;
            Ok(value)
        },
    );
    let w = weather.clone();
    ctx.defun(
        "set-weather-direction",
        move |value: f64| -> Result<f64, Error> {
            w.write().default_mut().wind_direction = value.rem_euclid(360.0);
            Ok(value)
        },
    );
    let w = weather.clone();
    ctx.defun(
        "set-weather-temperature-base",
        move |value: f64| -> Result<f64, Error> {
            let mut g = w.write();
            let d = g.default_mut();
            d.temperature_base = value;
            d.baseline_temperature_base = value;
            Ok(value)
        },
    );
    // (%make-weather-location :name … :area … :lat … :lon …
    //   :cloud-cover … :mean-wind … :wind-direction …
    //   :temperature-base …)
    ctx.defun(
        "%make-weather-location",
        move |args: Plist<MakeWeatherLocationArgs>| -> Result<bool, Error> {
            let a = args.into_inner();
            let cloud = a.cloud_cover.unwrap_or(0.30).clamp(0.0, 1.0);
            let wind = a.mean_wind.unwrap_or(6.0).max(0.0);
            let temp = a.temperature_base.unwrap_or(290.0);
            let loc = WeatherLocation {
                name: a.name.unwrap_or_else(|| {
                    format!(
                        "loc-{:.1}-{:.1}",
                        a.lat.unwrap_or(0.0),
                        a.lon.unwrap_or(0.0)
                    )
                }),
                lat: a.lat.unwrap_or(50.0),
                lon: a.lon.unwrap_or(10.0),
                cloud_cover: cloud,
                mean_wind: wind,
                wind_direction: a.wind_direction.unwrap_or(270.0).rem_euclid(360.0),
                temperature_base: temp,
                baseline_cloud_cover: cloud,
                baseline_mean_wind: wind,
                baseline_temperature_base: temp,
            };
            let mut reg = weather.write();
            let idx = reg.upsert(loc);
            if let Some(area) = a.area {
                reg.link_area(area, idx);
            }
            Ok(true)
        },
    );
}

AsPlist! {
    pub struct MakeWeatherLocationArgs {
        name<":name">: Option<String> {= None},
        area<":area">: Option<String> {= None},
        lat<":lat">: Option<f64> {= None},
        lon<":lon">: Option<f64> {= None},
        cloud_cover<":cloud-cover">: Option<f64> {= None},
        mean_wind<":mean-wind">: Option<f64> {= None},
        wind_direction<":wind-direction">: Option<f64> {= None},
        temperature_base<":temperature-base">: Option<f64> {= None},
    }
}

fn register_curve(ctx: &mut TulispContext, curve: crate::scenarios::SharedCurve) {
    ctx.defun(
        "set-forward-curve-base",
        move |hour: i64, price: f64| -> Result<f64, Error> {
            if !(0..=24).contains(&hour) {
                return Err(Error::os_error(format!(
                    "set-forward-curve-base: HOUR must be 0..=24, got {hour}"
                )));
            }
            curve.write().set_base_price_at(hour as usize, price);
            Ok(price)
        },
    );
}

fn register_bias_scale(ctx: &mut TulispContext, bias_scale: crate::scenarios::SharedBiasScale) {
    ctx.defun(
        "set-mm-bias-scale",
        move |value: f64| -> Result<f64, Error> {
            *bias_scale.write() = value;
            Ok(value)
        },
    );
}

/// Newtype around `TulispObject` so `Vec<RawStage>` satisfies the
/// AsPlist field bound (which needs `TryFrom<TulispObject, Error =
/// Error>`; the blanket impl on TulispObject is `Error = Infallible`).
pub struct RawStage(tulisp::TulispObject);

impl TryFrom<tulisp::TulispObject> for RawStage {
    type Error = Error;
    fn try_from(v: tulisp::TulispObject) -> Result<Self, Error> {
        Ok(RawStage(v))
    }
}

impl From<RawStage> for tulisp::TulispObject {
    fn from(v: RawStage) -> tulisp::TulispObject {
        v.0
    }
}

AsPlist! {
    pub struct DefineScenarioArgs {
        name: String,
        description: Option<String> {= None},
        /// Calendar date the solar-elevation model treats this
        /// scenario as taking place on, in ISO `YYYY-MM-DD`.
        /// Optional — if omitted, the bias tick uses wallclock-
        /// today. Setting "2026-06-21" on sunny-summer-day pins
        /// the day-of-year to summer solstice so peak irradiance
        /// matches the scenario name year-round.
        date: Option<String> {= None},
        stages: Vec<RawStage>,
    }
}

AsPlist! {
    pub struct StageArgs {
        name: String,
        hour_from<":hour-from">: f64,
        hour_to<":hour-to">: f64,
        bias_from<":bias-from">: f64,
        bias_to<":bias-to">: f64,
        /// Optional absolute overrides applied to every registered
        /// weather location while this stage is current. Omit to
        /// leave the configured baseline alone; supply a value to
        /// stamp the desired sky / wind / temperature for the stage
        /// (cloud_cover clamped to [0, 1], mean_wind ≥ 0,
        /// temperature_base in Kelvin).
        cloud_cover<":cloud-cover">: Option<f64> {= None},
        mean_wind<":mean-wind">: Option<f64> {= None},
        temperature_base<":temperature-base">: Option<f64> {= None},
    }
}

fn register_scenarios(ctx: &mut TulispContext, scenarios: crate::scenarios::SharedScenarios) {
    use crate::scenarios::{ScenarioDef, ScenarioEntry, ScenarioRuntime, Stage};
    ctx.defun(
        "define-scenario",
        move |ctx: &mut TulispContext,
              args: Plist<DefineScenarioArgs>|
              -> Result<String, Error> {
            let a = args.into_inner();
            let mut stages = Vec::new();
            for raw in a.stages {
                let s = StageArgs::from_plist(ctx, &raw.0)?;
                stages.push(Stage {
                    name: s.name,
                    hour_from: s.hour_from,
                    hour_to: s.hour_to,
                    bias_from: s.bias_from,
                    bias_to: s.bias_to,
                    cloud_cover: s.cloud_cover.map(|v| v.clamp(0.0, 1.0)),
                    mean_wind: s.mean_wind.map(|v| v.max(0.0)),
                    temperature_base: s.temperature_base,
                });
            }
            let date = match a.date.as_deref() {
                None => None,
                Some(s) => Some(chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(
                    |e| {
                        Error::os_error(format!(
                            "define-scenario: :date must be YYYY-MM-DD; got {s:?} ({e})"
                        ))
                    },
                )?),
            };
            let def = ScenarioDef {
                name: a.name.clone(),
                description: a.description.unwrap_or_default(),
                date,
                stages,
            };
            scenarios.lock().insert(
                a.name.clone(),
                ScenarioEntry {
                    def,
                    runtime: ScenarioRuntime::default(),
                },
            );
            Ok(a.name)
        },
    );
}

AsPlist! {
    pub struct MakeAggressorArgs {
        name: String,
        area<":area">: String,
        quarter_offset<":quarter-offset">: Option<i64> {= None},
        rate_ms<":rate-ms">: Option<i64> {= None},
        size<":size">: Option<f64> {= None},
        side_bias<":side-bias">: Option<f64> {= None},
        seed<":seed">: Option<i64> {= None},
    }
}

fn register_aggressors(
    ctx: &mut TulispContext,
    aggressors: Arc<Mutex<HashMap<String, AggressorSpec>>>,
    anchor: DateTime<Utc>,
) {
    let ag = aggressors.clone();
    ctx.defun(
        "%make-aggressor",
        move |args: Plist<MakeAggressorArgs>| -> Result<String, Error> {
            let a = args.into_inner();
            let area = Area::eic(&a.area);
            let period = DeliveryPeriod {
                start: next_quarter_boundary(anchor)
                    + chrono::Duration::minutes(15 * a.quarter_offset.unwrap_or(0)),
                duration: DeliveryDuration::DeliveryDuration15,
            };
            let cfg = AggressorConfig {
                area,
                period,
                currency: Currency::Eur,
                size: a.size.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(0.2)),
                side_bias: a.side_bias.unwrap_or(0.5).clamp(0.0, 1.0),
            };
            let seed = a.seed.unwrap_or(1) as u64;
            let rate_ms = a.rate_ms.unwrap_or(1000).max(50) as u64;
            let mut map = ag.lock();
            if let Some(existing) = map.get(&a.name) {
                // Hot-reload: keep the SharedConfig handle alive so
                // the running task picks up new size / side-bias on
                // its next fire. rate_ms is task-time only — needs
                // a process restart to change.
                let mut w = existing.shared_config.write();
                *w = cfg;
            } else {
                map.insert(
                    a.name.clone(),
                    AggressorSpec {
                        name: a.name.clone(),
                        shared_config: Arc::new(RwLock::new(cfg)),
                        seed,
                        rate_ms,
                        quarter_offset: a.quarter_offset.unwrap_or(0),
                    },
                );
            }
            Ok(a.name)
        },
    );

    // Runtime setters for the size + side-bias knobs.
    let ag2 = aggressors.clone();
    ctx.defun(
        "set-aggressor-size",
        move |name: String, value: f64| -> Result<bool, Error> {
            match ag2.lock().get(&name) {
                Some(spec) => {
                    spec.shared_config.write().size = f64_to_dec(value);
                    Ok(true)
                }
                None => Err(Error::os_error(format!("unknown aggressor {name:?}"))),
            }
        },
    );
    let ag3 = aggressors;
    ctx.defun(
        "set-aggressor-side-bias",
        move |name: String, value: f64| -> Result<bool, Error> {
            match ag3.lock().get(&name) {
                Some(spec) => {
                    spec.shared_config.write().side_bias = value.clamp(0.0, 1.0);
                    Ok(true)
                }
                None => Err(Error::os_error(format!("unknown aggressor {name:?}"))),
            }
        },
    );
}

fn register_watches(
    ctx: &mut TulispContext,
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    load_dir: PathBuf,
) {
    ctx.defun("watch-file", move |path: String| -> Result<bool, Error> {
        let p = Path::new(&path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            load_dir.join(p)
        };
        extra_watches.lock().insert(abs);
        Ok(true)
    });
}

AsPlist! {
    pub struct MakeCouplingArgs {
        areas<":areas">: Vec<String>,
        gate_offset_seconds<":gate-offset-seconds">: Option<i64> {= None},
        capacity_mw<":capacity">: Option<f64> {= None},
    }
}

fn register_couplings(ctx: &mut TulispContext, couplings: Arc<Mutex<Vec<CouplingSpec>>>) {
    ctx.defun(
        "%make-coupling",
        move |args: Plist<MakeCouplingArgs>| -> Result<bool, Error> {
            let a = args.into_inner();
            if a.areas.len() != 2 {
                return Err(Error::os_error(
                    "make-coupling: :areas must list exactly two area codes",
                ));
            }
            couplings.lock().push(CouplingSpec {
                area_a: a.areas[0].clone(),
                area_b: a.areas[1].clone(),
                gate_offset_seconds: a.gate_offset_seconds.unwrap_or(0),
                capacity_mw: a.capacity_mw,
            });
            Ok(true)
        },
    );
}

AsPlist! {
    pub struct MakeMarketArgs {
        area<":area">: String,
        /// Currency code: "eur" (default), "usd", "gbp", "chf".
        currency<":currency">: Option<String> {= None},
    }
}

fn register_markets(ctx: &mut TulispContext, markets: Arc<Mutex<Vec<MarketSpec>>>) {
    ctx.defun(
        "%make-market",
        move |args: Plist<MakeMarketArgs>| -> Result<String, Error> {
            let a = args.into_inner();
            let currency = match a
                .currency
                .as_deref()
                .unwrap_or("eur")
                .to_lowercase()
                .as_str()
            {
                "eur" => Currency::Eur,
                "usd" => Currency::Usd,
                "gbp" => Currency::Gbp,
                "chf" => Currency::Chf,
                other => {
                    return Err(Error::os_error(format!(
                        "make-market: unsupported currency {other:?}"
                    )));
                }
            };
            let spec = MarketSpec {
                area_code: a.area.clone(),
                currency,
            };
            markets.lock().push(spec);
            Ok(a.area)
        },
    );
}

AsPlist! {
    pub struct MakeGridpoolArgs {
        id: i64,
        name: Option<String> {= None},
        areas<":areas">: Vec<String>,
        self_trade_policy<":self-trade-policy">: Option<String> {= None},
    }
}

fn register_gridpools(ctx: &mut TulispContext, gridpools: Arc<Mutex<Vec<GridpoolSpec>>>) {
    use crate::sim::gridpool::SelfTradePolicy;
    ctx.defun(
        "%make-gridpool",
        move |args: Plist<MakeGridpoolArgs>| -> Result<i64, Error> {
            let a = args.into_inner();
            if a.areas.is_empty() {
                return Err(Error::os_error(
                    "make-gridpool: :areas must list at least one area code",
                ));
            }
            let policy = match a.self_trade_policy.as_deref() {
                None | Some("allow") => SelfTradePolicy::Allow,
                Some("reject") => SelfTradePolicy::Reject,
                Some(other) => {
                    return Err(Error::os_error(format!(
                        "make-gridpool: :self-trade-policy must be \"allow\" or \"reject\"; got {other:?}"
                    )));
                }
            };
            let spec = GridpoolSpec {
                id: a.id as u64,
                name: a.name.unwrap_or_else(|| format!("gridpool-{}", a.id)),
                area_codes: a.areas,
                self_trade_policy: policy,
            };
            let id = spec.id;
            gridpools.lock().push(spec);
            Ok(id as i64)
        },
    );
}

AsPlist! {
    pub struct MakeMmArgs {
        name: String,
        area<":area">: String,
        /// Quarter-hours after the next 15-min boundary on or after
        /// Config::anchor. 0 = the next quarter; 4 = one hour later.
        quarter_offset<":quarter-offset">: Option<i64> {= None},
        reference<":reference">: Option<f64> {= None},
        spread<":spread">: Option<f64> {= None},
        size<":size">: Option<f64> {= None},
        demand<":demand">: Option<f64> {= None},
        surplus<":surplus">: Option<f64> {= None},
        noise<":noise">: Option<f64> {= None},
        seed<":seed">: Option<i64> {= None},
    }
}

pub fn next_quarter_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let bucket = (secs / 900 + 1) * 900;
    DateTime::from_timestamp(bucket, 0).unwrap()
}

fn register_time_helpers(ctx: &mut TulispContext) {
    // Hour-of-day (UTC, fractional) for the contract delivered at
    // the given quarter offset from "now". A scenario uses this to
    // pick e.g. an aggressor side-bias from the time-of-day curve.
    ctx.defun("quarter-offset-hour", |offset: i64| -> f64 {
        let now = Utc::now();
        let boundary = next_quarter_boundary(now);
        let period_start = boundary + chrono::Duration::minutes(15 * offset);
        use chrono::Timelike;
        period_start.hour() as f64 + period_start.minute() as f64 / 60.0
    });
}

fn f64_to_dec(v: f64) -> Decimal {
    Decimal::try_from(v).unwrap_or(Decimal::ZERO)
}

fn register_market_makers(
    ctx: &mut TulispContext,
    market_makers: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
    anchor: DateTime<Utc>,
) {
    let mm = market_makers.clone();
    ctx.defun(
        "%make-market-maker",
        move |args: Plist<MakeMmArgs>| -> Result<String, Error> {
            let a = args.into_inner();
            let area = Area::eic(&a.area);
            let period = DeliveryPeriod {
                start: next_quarter_boundary(anchor)
                    + chrono::Duration::minutes(15 * a.quarter_offset.unwrap_or(0)),
                duration: DeliveryDuration::DeliveryDuration15,
            };
            let reference = a
                .reference
                .map(f64_to_dec)
                .unwrap_or_else(|| f64_to_dec(85.00));
            let mut cfg = MarketMakerConfig {
                area,
                period,
                currency: Currency::Eur,
                // Seed baseline and live price together so the first
                // refresh has nowhere to drift from until the bias
                // tick computes a fundamentals-derived baseline.
                reference_baseline: reference,
                reference_price: reference,
                spread: a.spread.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(0.40)),
                size: a.size.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(1.0)),
                demand: a.demand.map(f64_to_dec).unwrap_or(Decimal::ZERO),
                surplus: a.surplus.map(f64_to_dec).unwrap_or(Decimal::ZERO),
                price_noise: a.noise.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(0.10)),
                tick: f64_to_dec(0.01),
                follow_last_trade: Decimal::ZERO,
            };
            // demand/surplus are already in cfg; nothing else to set.
            let _ = &mut cfg;
            let seed = a.seed.unwrap_or(1) as u64;
            let mut map = mm.lock();
            if let Some(existing) = map.get(&a.name) {
                // Reload-friendly: keep the SharedConfig handle alive
                // so the running MM task picks up the new values on
                // its next refresh. Seed only takes effect on first
                // insert; rotating an MM's seed needs a process
                // restart.
                let mut w = existing.shared_config.write();
                *w = cfg;
            } else {
                map.insert(
                    a.name.clone(),
                    MarketMakerSpec {
                        name: a.name.clone(),
                        shared_config: Arc::new(RwLock::new(cfg)),
                        seed,
                        quarter_offset: a.quarter_offset.unwrap_or(0),
                    },
                );
            }
            Ok(a.name)
        },
    );

    // Helper to build a mutator defun: take (name, value) and apply
    // a closure on the named MM's shared config.
    fn mk_setter<F>(
        ctx: &mut TulispContext,
        defun_name: &'static str,
        mm: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
        f: F,
    ) where
        F: Fn(&mut MarketMakerConfig, f64) + Send + Sync + 'static,
    {
        ctx.defun(
            defun_name,
            move |name: String, value: f64| -> Result<bool, Error> {
                let guard = mm.lock();
                match guard.get(&name) {
                    Some(spec) => {
                        let mut cfg = spec.shared_config.write();
                        f(&mut cfg, value);
                        Ok(true)
                    }
                    None => Err(Error::os_error(format!("unknown market-maker {name:?}"))),
                }
            },
        );
    }

    mk_setter(ctx, "set-mm-reference", market_makers.clone(), |c, v| {
        // (set-mm-reference NAME EUR) is the explicit-snap path —
        // jumps the live price as well as the baseline so a lisp
        // callback that wants to peg an MM doesn't have to wait for
        // mean reversion. The scenario bias tick uses a different
        // path that touches baseline only.
        let d = f64_to_dec(v);
        c.reference_baseline = d;
        c.reference_price = d;
    });
    mk_setter(ctx, "set-mm-spread", market_makers.clone(), |c, v| {
        c.spread = f64_to_dec(v);
    });
    mk_setter(ctx, "set-mm-size", market_makers.clone(), |c, v| {
        c.size = f64_to_dec(v);
    });
    mk_setter(ctx, "set-mm-demand", market_makers.clone(), |c, v| {
        c.demand = f64_to_dec(v);
    });
    mk_setter(ctx, "set-mm-surplus", market_makers.clone(), |c, v| {
        c.surplus = f64_to_dec(v);
    });
    mk_setter(ctx, "set-mm-noise", market_makers.clone(), |c, v| {
        c.price_noise = f64_to_dec(v);
    });
    mk_setter(ctx, "set-mm-follow-last-trade", market_makers, |c, v| {
        // 0.0 = static; 1.0 = snap to last trade each refresh.
        c.follow_last_trade = f64_to_dec(v.clamp(0.0, 1.0));
    });
}

fn register_metadata(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-socket-addr", move |addr: String| -> String {
        m.write().socket_addr = addr.clone();
        addr
    });
    let m = metadata.clone();
    ctx.defun("set-physics-tick-ms", move |ms: i64| -> i64 {
        let ms = ms.max(1) as u64;
        m.write().physics_tick = Duration::from_millis(ms);
        ms as i64
    });
    let m = metadata;
    ctx.defun("get-socket-addr", move || -> String {
        m.read().socket_addr.clone()
    });
}

fn add_log_functions(ctx: &mut TulispContext) {
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(body: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new().suffix(".lisp").tempfile().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn empty_config_yields_defaults() {
        let f = write_tmp(";; empty\n");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::1]:8810");
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(100));
    }

    #[tokio::test]
    async fn set_socket_addr_takes_effect() {
        let f = write_tmp(r#"(set-socket-addr "[::]:9000")"#);
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::]:9000");
    }

    #[tokio::test]
    async fn set_physics_tick_ms_takes_effect() {
        let f = write_tmp("(set-physics-tick-ms 200)");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(200));
    }

    #[tokio::test]
    async fn make_market_maker_registers_spec() {
        let f = write_tmp(
            r#"
            (%make-market-maker
              :name "q0"
              :area "10Y1001A1001A82H"
              :quarter-offset 0
              :reference 90.00
              :spread 0.50
              :size 2.0
              :seed 7)
            "#,
        );
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        let mms = cfg.market_makers();
        assert_eq!(mms.len(), 1);
        let mm = &mms[0];
        assert_eq!(mm.name, "q0");
        assert_eq!(mm.seed, 7);
        let inner = mm.shared_config.read();
        assert_eq!(inner.reference_price, rust_decimal::dec!(90.00));
        assert_eq!(inner.spread, rust_decimal::dec!(0.50));
        assert_eq!(inner.size, rust_decimal::dec!(2.0));
        // Next 15-min boundary after `anchor`.
        assert!(inner.period.start > cfg.anchor());
        assert_eq!(inner.period.duration, DeliveryDuration::DeliveryDuration15);
    }

    #[tokio::test]
    async fn set_mm_demand_after_make_takes_effect_on_shared() {
        let f = write_tmp(
            r#"
            (%make-market-maker :name "h0" :area "10Y1001A1001A82H")
            (set-mm-demand "h0" 0.20)
            (set-mm-surplus "h0" 0.05)
            "#,
        );
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        let mms = cfg.market_makers();
        let inner = mms[0].shared_config.read();
        assert_eq!(inner.demand, rust_decimal::dec!(0.20));
        assert_eq!(inner.surplus, rust_decimal::dec!(0.05));
    }

    #[tokio::test]
    async fn set_mm_demand_unknown_name_errors() {
        let f = write_tmp(r#"(set-mm-demand "nonexistent" 0.10)"#);
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("nonexistent"), "got: {e}"),
        }
    }

    #[tokio::test]
    async fn make_gridpool_registers_spec() {
        let f = write_tmp(
            r#"
            (%make-gridpool :id 1 :name "default" :areas '("10Y1001A1001A82H"))
            (%make-gridpool :id 2 :areas '("10YFR-RTE------C" "10YBE----------2"))
            "#,
        );
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        let gps = cfg.gridpools();
        assert_eq!(gps.len(), 2);
        assert_eq!(gps[0].id, 1);
        assert_eq!(gps[0].name, "default");
        assert_eq!(gps[0].area_codes, vec!["10Y1001A1001A82H"]);
        assert_eq!(gps[1].id, 2);
        assert_eq!(gps[1].name, "gridpool-2");
        assert_eq!(gps[1].area_codes.len(), 2);
    }

    #[tokio::test]
    async fn make_gridpool_parses_self_trade_policy() {
        use crate::sim::gridpool::SelfTradePolicy;
        let f = write_tmp(
            r#"
            (%make-gridpool :id 1 :areas '("10Y1001A1001A82H"))
            (%make-gridpool :id 2 :areas '("10Y1001A1001A82H")
                            :self-trade-policy "reject")
            "#,
        );
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        let gps = cfg.gridpools();
        assert_eq!(gps[0].self_trade_policy, SelfTradePolicy::Allow);
        assert_eq!(gps[1].self_trade_policy, SelfTradePolicy::Reject);
    }

    #[tokio::test]
    async fn make_gridpool_rejects_unknown_self_trade_policy() {
        let f = write_tmp(
            r#"(%make-gridpool :id 1 :areas '("10Y1001A1001A82H")
                                :self-trade-policy "nope")"#,
        );
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("self-trade-policy"), "got: {e}"),
        }
    }

    #[tokio::test]
    async fn make_gridpool_rejects_empty_areas() {
        let f = write_tmp(r#"(%make-gridpool :id 1 :areas '())"#);
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("at least one area code"), "got: {e}"),
        }
    }

    #[tokio::test]
    async fn lisp_error_surfaces_with_trace() {
        let f = write_tmp("(this-is-not-a-defun 1 2 3)");
        // Config doesn't impl Debug (SharedMut<TulispContext> doesn't),
        // so test the error path via match instead of unwrap_err.
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error from {path}"),
            Err(e) => assert!(e.contains("this-is-not-a-defun"), "got: {e}"),
        }
    }

    #[tokio::test]
    async fn run_with_timer_callback_fires_and_mutates_shared_config() {
        // tulisp-async exposes `(run-with-timer first repeat fn …)`.
        // The (every …) sugar wrapper is built on top of it in
        // sim/common.lisp (not loaded in tests).
        let f = write_tmp(
            r#"
            (%make-market-maker :name "h0" :area "10Y1001A1001A82H")
            (setq counter 0)
            (run-with-timer 0.001 0.001
              (lambda ()
                (setq counter (+ counter 1))
                (set-mm-demand "h0" (* counter 0.01))))
            "#,
        );
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        let mm = cfg.market_makers().pop().unwrap();
        assert_eq!(mm.shared_config.read().demand, rust_decimal::dec!(0));

        let _drain = cfg.spawn_timer_loop(Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let demand = mm.shared_config.read().demand;
        assert!(
            demand > rust_decimal::dec!(0),
            "demand stayed at 0: {demand}"
        );
    }
}
