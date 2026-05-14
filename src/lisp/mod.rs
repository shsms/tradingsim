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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use rust_decimal::Decimal;
use tulisp::{AsPlist, Error, Plist, SharedMut, TulispContext};

use crate::sim::counterparty::{MarketMakerConfig, SharedConfig};
use crate::sim::market::{Area, Currency, DeliveryDuration, DeliveryPeriod};

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
}

#[derive(Clone)]
pub struct Config {
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    pub(crate) ctx: SharedMut<TulispContext>,
    metadata: Arc<RwLock<Metadata>>,
    market_makers: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
    /// Anchor time for relative period offsets. Set at Config::new
    /// so that `(make-market-maker :hour-offset N …)` always builds
    /// the same absolute period within one config-load.
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
        let anchor = Utc::now();

        let load_dir: PathBuf = match Path::new(filename).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| format!("set_load_path({}): {e}", load_dir.display()))?;

        register_runtime(&mut ctx, metadata.clone(), market_makers.clone(), anchor);

        if let Err(e) = ctx.eval_file(filename) {
            return Err(e.format(&ctx));
        }

        Ok(Self {
            filename: filename.to_string(),
            ctx: SharedMut::new(ctx),
            metadata,
            market_makers,
            anchor,
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

    pub fn anchor(&self) -> DateTime<Utc> {
        self.anchor
    }
}

fn register_runtime(
    ctx: &mut TulispContext,
    metadata: Arc<RwLock<Metadata>>,
    market_makers: Arc<Mutex<HashMap<String, MarketMakerSpec>>>,
    anchor: DateTime<Utc>,
) {
    add_log_functions(ctx);
    register_metadata(ctx, metadata);
    register_market_makers(ctx, market_makers, anchor);
}

AsPlist! {
    pub struct MakeMmArgs {
        name: String,
        area<":area">: String,
        /// Hours after the next hour boundary on or after Config::anchor.
        /// 0 = next hour; 1 = the one after that.
        hour_offset<":hour-offset">: Option<i64> {= None},
        reference<":reference">: Option<f64> {= None},
        spread<":spread">: Option<f64> {= None},
        size<":size">: Option<f64> {= None},
        demand<":demand">: Option<f64> {= None},
        surplus<":surplus">: Option<f64> {= None},
        noise<":noise">: Option<f64> {= None},
        seed<":seed">: Option<i64> {= None},
    }
}

fn next_hour_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let secs = now.timestamp();
    let bucket = (secs / 3600 + 1) * 3600;
    DateTime::from_timestamp(bucket, 0).unwrap()
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
                start: next_hour_boundary(anchor)
                    + chrono::Duration::hours(a.hour_offset.unwrap_or(0)),
                duration: DeliveryDuration::DeliveryDuration60,
            };
            let mut cfg = MarketMakerConfig {
                area,
                period,
                currency: Currency::Eur,
                reference_price: a.reference.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(85.00)),
                spread: a.spread.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(0.40)),
                size: a.size.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(1.0)),
                demand: a.demand.map(f64_to_dec).unwrap_or(Decimal::ZERO),
                surplus: a.surplus.map(f64_to_dec).unwrap_or(Decimal::ZERO),
                price_noise: a.noise.map(f64_to_dec).unwrap_or_else(|| f64_to_dec(0.10)),
                tick: f64_to_dec(0.01),
            };
            // demand/surplus are already in cfg; nothing else to set.
            let _ = &mut cfg;
            let shared = Arc::new(RwLock::new(cfg));
            let seed = a.seed.unwrap_or(1) as u64;
            mm.lock().insert(
                a.name.clone(),
                MarketMakerSpec {
                    name: a.name.clone(),
                    shared_config: shared,
                    seed,
                },
            );
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
        c.reference_price = f64_to_dec(v);
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
    mk_setter(ctx, "set-mm-noise", market_makers, |c, v| {
        c.price_noise = f64_to_dec(v);
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
        let mut f = tempfile::Builder::new()
            .suffix(".lisp")
            .tempfile()
            .unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn empty_config_yields_defaults() {
        let f = write_tmp(";; empty\n");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::1]:8810");
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(100));
    }

    #[test]
    fn set_socket_addr_takes_effect() {
        let f = write_tmp(r#"(set-socket-addr "[::]:9000")"#);
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.socket_addr(), "[::]:9000");
    }

    #[test]
    fn set_physics_tick_ms_takes_effect() {
        let f = write_tmp("(set-physics-tick-ms 200)");
        let cfg = Config::new(f.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.metadata().physics_tick, Duration::from_millis(200));
    }

    #[test]
    fn make_market_maker_registers_spec() {
        let f = write_tmp(
            r#"
            (%make-market-maker
              :name "h0"
              :area "10Y1001A1001A82H"
              :hour-offset 0
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
        assert_eq!(mm.name, "h0");
        assert_eq!(mm.seed, 7);
        let inner = mm.shared_config.read();
        assert_eq!(inner.reference_price, rust_decimal::dec!(90.00));
        assert_eq!(inner.spread, rust_decimal::dec!(0.50));
        assert_eq!(inner.size, rust_decimal::dec!(2.0));
        // Next hour boundary after `anchor`.
        assert!(inner.period.start > cfg.anchor());
        assert_eq!(inner.period.duration, DeliveryDuration::DeliveryDuration60);
    }

    #[test]
    fn set_mm_demand_after_make_takes_effect_on_shared() {
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

    #[test]
    fn set_mm_demand_unknown_name_errors() {
        let f = write_tmp(r#"(set-mm-demand "nonexistent" 0.10)"#);
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("nonexistent"), "got: {e}"),
        }
    }

    #[test]
    fn lisp_error_surfaces_with_trace() {
        let f = write_tmp("(this-is-not-a-defun 1 2 3)");
        // Config doesn't impl Debug (SharedMut<TulispContext> doesn't),
        // so test the error path via match instead of unwrap_err.
        let path = f.path().to_str().unwrap().to_string();
        match Config::new(&path) {
            Ok(_) => panic!("expected error from {path}"),
            Err(e) => assert!(e.contains("this-is-not-a-defun"), "got: {e}"),
        }
    }
}
