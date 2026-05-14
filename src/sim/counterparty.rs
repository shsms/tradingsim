//! Synthetic counterparties. The market-maker engine continuously
//! refreshes a bid + ask pair around a configurable reference price,
//! so user orders have someone to trade against without a lisp config
//! yet. Demand/surplus knobs let a scenario tilt the spread without
//! changing the reference.
//!
//! The engine is plain data — it takes a `&mut World` and steps once
//! per `refresh()` call. The driver (a tokio task in the binary)
//! decides when to fire it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use rand::Rng;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use rust_decimal::Decimal;
use rust_decimal::dec;

use crate::sim::decimal::snap_to_tick;
use crate::sim::market::{Area, Currency, DeliveryPeriod};
use crate::sim::order::{Order, OrderId, OrderType, Side};
use crate::sim::world::World;

/// Knobs the binary (and later, lisp) sets per market-maker.
#[derive(Clone, Debug)]
pub struct MarketMakerConfig {
    pub area: Area,
    pub period: DeliveryPeriod,
    pub currency: Currency,
    /// Mid-price reference, EUR/MWh.
    pub reference_price: Decimal,
    /// Half-spread; bid is reference - spread, ask is reference + spread.
    pub spread: Decimal,
    /// Quantity per quote, MW.
    pub size: Decimal,
    /// Demand tilt: shifts the bid upward (the MM is hungrier to buy)
    /// — makes asks fill faster for user sells against the MM, and
    /// raises the prevailing price.
    pub demand: Decimal,
    /// Surplus tilt: shifts the ask downward (the MM is desperate to
    /// sell) — makes bids fill faster for user buys, lowers prices.
    pub surplus: Decimal,
    /// Per-refresh random walk magnitude on the reference, EUR/MWh.
    /// Use 0 for deterministic tests.
    pub price_noise: Decimal,
    /// Price tick to snap to. Should match the market's tick.
    pub tick: Decimal,
    /// If > 0, pull `reference_price` toward the most recent public
    /// trade on this contract by `follow_last_trade * (last - ref)`
    /// at the start of each refresh. 0 = static reference;
    /// 1.0 = snap to last trade each tick.
    pub follow_last_trade: Decimal,
}

impl MarketMakerConfig {
    pub fn de_lu_default(area: Area, period: DeliveryPeriod) -> Self {
        Self {
            area,
            period,
            currency: Currency::Eur,
            reference_price: dec!(85.00),
            spread: dec!(0.40),
            size: dec!(1.0),
            demand: dec!(0),
            surplus: dec!(0),
            price_noise: dec!(0.10),
            tick: dec!(0.01),
            follow_last_trade: dec!(0),
        }
    }
}

/// Shared handle on a MarketMakerConfig. The MM task holds one;
/// lisp callbacks ((set-mm-demand …) etc.) hold others; updates
/// take effect on the next refresh tick.
pub type SharedConfig = Arc<RwLock<MarketMakerConfig>>;

pub struct MarketMaker {
    config: SharedConfig,
    bid_id: Option<OrderId>,
    ask_id: Option<OrderId>,
    rng: SmallRng,
}

impl MarketMaker {
    pub fn new(config: MarketMakerConfig, seed: u64) -> Self {
        Self::with_shared_config(Arc::new(RwLock::new(config)), seed)
    }

    pub fn with_shared_config(config: SharedConfig, seed: u64) -> Self {
        Self {
            config,
            bid_id: None,
            ask_id: None,
            rng: SmallRng::seed_from_u64(seed),
        }
    }

    /// Hand out a clone of the shared config so external code (the
    /// lisp defun layer, the UI) can mutate demand/surplus/reference
    /// — updates take effect on the next refresh.
    pub fn shared_config(&self) -> SharedConfig {
        self.config.clone()
    }

    /// Cancel any outstanding quotes, then post a fresh bid + ask
    /// around the (drift-walked) reference price.
    pub fn refresh(&mut self, world: &mut World, now: DateTime<Utc>) {
        // Reference-drift pass — if follow_last_trade > 0, pull the
        // shared reference toward the most recent public trade on
        // this contract. Runs before the quote-cancel/repost so the
        // new quotes reflect the drift.
        self.drift_reference(world);

        if let Some(id) = self.bid_id.take() {
            world.cancel_counterparty_order(id);
        }
        if let Some(id) = self.ask_id.take() {
            world.cancel_counterparty_order(id);
        }

        // Snapshot the config under the read lock — the lisp side may
        // race us on a write, but we want one consistent set of knobs
        // for this quote pair.
        let cfg = self.config.read().clone();

        let noise = if cfg.price_noise.is_zero() {
            Decimal::ZERO
        } else {
            let n: i64 = self.rng.gen_range(-100..=100);
            Decimal::new(n, 2) * cfg.price_noise
        };
        let mid = cfg.reference_price + noise;
        let bid_raw = mid - cfg.spread + cfg.demand;
        let ask_raw = mid + cfg.spread - cfg.surplus;
        let bid_price = snap_to_tick(bid_raw, cfg.tick);
        let ask_price = snap_to_tick(ask_raw, cfg.tick);

        if bid_price >= ask_price || bid_price <= Decimal::ZERO {
            return;
        }

        if let Ok(id) = world.submit_counterparty_order(build(&cfg, Side::Buy, bid_price), now) {
            self.bid_id = Some(id);
        }
        if let Ok(id) = world.submit_counterparty_order(build(&cfg, Side::Sell, ask_price), now) {
            self.ask_id = Some(id);
        }
    }

    fn drift_reference(&mut self, world: &World) {
        let (rate, area, period, current_ref) = {
            let c = self.config.read();
            (
                c.follow_last_trade,
                c.area.clone(),
                c.period,
                c.reference_price,
            )
        };
        if rate <= Decimal::ZERO {
            return;
        }
        let hist = world.public_trade_history();
        let last = hist
            .iter()
            .rev()
            .find(|t| t.period == period && (t.buy_area == area || t.sell_area == area));
        if let Some(t) = last {
            let new_ref = current_ref + (t.price - current_ref) * rate;
            self.config.write().reference_price = new_ref;
        }
    }
}

// -----------------------------------------------------------------------------
// Aggressor — a non-gridpool taker that crosses the best opposite-side
// price each fire(). Generates public trades against whatever liquidity
// is on the book (typically the market-maker), driving movement on the
// public trade tape.
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AggressorConfig {
    pub area: Area,
    pub period: DeliveryPeriod,
    pub currency: Currency,
    /// Quantity per shot, MW. Must align with the market qty step.
    pub size: Decimal,
    /// 0.0 = always sell; 1.0 = always buy; 0.5 = balanced. Decoupled
    /// from the MM's demand/surplus so an aggressor can simulate
    /// asymmetric trader flow without changing the MM's quote shape.
    pub side_bias: f64,
}

impl AggressorConfig {
    pub fn de_lu_default(area: Area, period: DeliveryPeriod) -> Self {
        Self {
            area,
            period,
            currency: Currency::Eur,
            size: dec!(0.2),
            side_bias: 0.5,
        }
    }
}

pub type SharedAggressorConfig = Arc<RwLock<AggressorConfig>>;

pub struct Aggressor {
    config: SharedAggressorConfig,
    rng: SmallRng,
}

impl Aggressor {
    pub fn new(config: AggressorConfig, seed: u64) -> Self {
        Self::with_shared_config(Arc::new(RwLock::new(config)), seed)
    }

    pub fn with_shared_config(config: SharedAggressorConfig, seed: u64) -> Self {
        Self {
            config,
            rng: SmallRng::seed_from_u64(seed),
        }
    }

    pub fn shared_config(&self) -> SharedAggressorConfig {
        self.config.clone()
    }

    /// Pick a side (random with bias), look at the best opposite price
    /// on the book, submit a marketable limit order at that price.
    /// No-op when the opposite side is empty.
    pub fn fire(&mut self, world: &mut World, now: DateTime<Utc>) {
        let cfg = self.config.read().clone();
        let side = if self.rng.gen_range(0.0..=1.0_f64) < cfg.side_bias {
            Side::Buy
        } else {
            Side::Sell
        };
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let target_price = match world.book(&key).and_then(|b| match side {
            Side::Buy => b.best_ask(),
            Side::Sell => b.best_bid(),
            _ => None,
        }) {
            Some(p) => p,
            None => return,
        };
        let order = Order {
            area: cfg.area.clone(),
            period: cfg.period,
            order_type: OrderType::Limit,
            side,
            price: target_price,
            currency: cfg.currency,
            quantity: cfg.size,
            stop_price: None,
            peak_price_delta: None,
            display_quantity: None,
            execution_option: None,
            valid_until: None,
            payload: None,
            tag: None,
        };
        let _ = world.submit_counterparty_order(order, now);
    }
}

fn build(cfg: &MarketMakerConfig, side: Side, price: Decimal) -> Order {
    Order {
        area: cfg.area.clone(),
        period: cfg.period,
        order_type: OrderType::Limit,
        side,
        price,
        currency: cfg.currency,
        quantity: cfg.size,
        stop_price: None,
        peak_price_delta: None,
        display_quantity: None,
        execution_option: None,
        valid_until: None,
        payload: None,
        tag: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::market::{DeliveryDuration, MarketRegistry, MarketRules};
    use chrono::TimeZone;

    fn setup_world() -> (World, MarketMakerConfig) {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::de_lu());
        let world = World::new(markets);
        let cfg = MarketMakerConfig {
            price_noise: dec!(0), // deterministic
            ..MarketMakerConfig::de_lu_default(
                Area::eic("10Y1001A1001A82H"),
                DeliveryPeriod {
                    start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                    duration: DeliveryDuration::DeliveryDuration15,
                },
            )
        };
        (world, cfg)
    }

    #[test]
    fn refresh_posts_bid_and_ask_around_reference() {
        let (mut world, cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg.clone(), 42);
        mm.refresh(&mut world, Utc::now());
        let book = world.book(&key).unwrap();
        assert_eq!(book.best_bid(), Some(dec!(84.60)));
        assert_eq!(book.best_ask(), Some(dec!(85.40)));
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn refresh_cancels_previous_quotes() {
        let (mut world, cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg, 42);
        mm.refresh(&mut world, Utc::now());
        mm.refresh(&mut world, Utc::now());
        let book = world.book(&key).unwrap();
        // After two refreshes the book still holds exactly one bid
        // + one ask — the old ones were cancelled, not stacked.
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn demand_tilts_bid_up() {
        let (mut world, mut cfg) = setup_world();
        cfg.demand = dec!(0.20);
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg, 42);
        mm.refresh(&mut world, Utc::now());
        let book = world.book(&key).unwrap();
        assert_eq!(book.best_bid(), Some(dec!(84.80)));
        assert_eq!(book.best_ask(), Some(dec!(85.40)));
    }

    #[test]
    fn surplus_tilts_ask_down() {
        let (mut world, mut cfg) = setup_world();
        cfg.surplus = dec!(0.20);
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg, 42);
        mm.refresh(&mut world, Utc::now());
        let book = world.book(&key).unwrap();
        assert_eq!(book.best_ask(), Some(dec!(85.20)));
    }

    #[test]
    fn shared_config_mutation_takes_effect_on_next_refresh() {
        let (mut world, cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg, 42);
        let shared = mm.shared_config();
        mm.refresh(&mut world, Utc::now());
        let pre_bid = world.book(&key).unwrap().best_bid().unwrap();

        // External writer raises demand by 0.20 EUR/MWh.
        shared.write().demand = dec!(0.20);
        mm.refresh(&mut world, Utc::now());
        let post_bid = world.book(&key).unwrap().best_bid().unwrap();
        assert_eq!(post_bid - pre_bid, dec!(0.20));
    }

    #[test]
    fn aggressor_buys_against_best_ask() {
        let (mut world, mm_cfg) = setup_world();
        let mut mm = MarketMaker::new(mm_cfg.clone(), 42);
        mm.refresh(&mut world, Utc::now());
        // After MM refresh: best ask should be reference + spread = 85.4.
        let ask_before = world
            .book(&crate::sim::world::ContractKey {
                area: mm_cfg.area.clone(),
                period: mm_cfg.period,
            })
            .unwrap()
            .best_ask()
            .unwrap();
        let ag_cfg = AggressorConfig {
            side_bias: 1.0, // always buy
            size: dec!(0.5),
            ..AggressorConfig::de_lu_default(mm_cfg.area.clone(), mm_cfg.period)
        };
        let mut ag = Aggressor::new(ag_cfg, 7);
        ag.fire(&mut world, Utc::now());
        // The MM's ask depth at that price drops by 0.5; if it
        // started at 1.0 it's now 0.5.
        let depth = world
            .book(&crate::sim::world::ContractKey {
                area: mm_cfg.area.clone(),
                period: mm_cfg.period,
            })
            .unwrap()
            .depth_at(Side::Sell, ask_before);
        assert_eq!(depth, dec!(0.5));
    }

    #[test]
    fn aggressor_skips_empty_book() {
        let (mut world, mm_cfg) = setup_world();
        // No MM refresh — book is empty for this contract.
        let ag_cfg = AggressorConfig::de_lu_default(mm_cfg.area.clone(), mm_cfg.period);
        let mut ag = Aggressor::new(ag_cfg, 0);
        ag.fire(&mut world, Utc::now());
        // No panic, no trades — graceful no-op.
        let key = crate::sim::world::ContractKey {
            area: mm_cfg.area.clone(),
            period: mm_cfg.period,
        };
        assert!(world.book(&key).map(|b| b.is_empty()).unwrap_or(true));
    }

    #[test]
    fn mm_drift_pulls_reference_toward_last_trade() {
        let (mut world, mut cfg) = setup_world();
        cfg.follow_last_trade = dec!(0.5); // pull halfway each refresh

        // Pre-seed a public trade at 90 on this contract — two
        // crossing counterparty orders on an empty book (no MM
        // resting yet to interfere with the match).
        let mk = |side, price| Order {
            area: cfg.area.clone(),
            period: cfg.period,
            order_type: OrderType::Limit,
            side,
            price,
            currency: Currency::Eur,
            quantity: dec!(0.1),
            stop_price: None,
            peak_price_delta: None,
            display_quantity: None,
            execution_option: None,
            valid_until: None,
            payload: None,
            tag: None,
        };
        world
            .submit_counterparty_order(mk(Side::Sell, dec!(90.0)), Utc::now())
            .unwrap();
        world
            .submit_counterparty_order(mk(Side::Buy, dec!(90.0)), Utc::now())
            .unwrap();

        // Now spawn the MM with drift enabled. First refresh sees
        // the seeded trade at 90 → ref pulls 85 → 87.50.
        let shared = Arc::new(RwLock::new(cfg.clone()));
        let mut mm = MarketMaker::with_shared_config(shared.clone(), 0);
        mm.refresh(&mut world, Utc::now());
        assert_eq!(shared.read().reference_price, dec!(87.50));
    }

    #[test]
    fn collapsed_spread_skips_posting() {
        let (mut world, mut cfg) = setup_world();
        cfg.demand = dec!(10);
        cfg.surplus = dec!(10);
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg, 42);
        mm.refresh(&mut world, Utc::now());
        let book = world.book(&key);
        // Book may not even exist yet; if it does, it should be empty.
        if let Some(b) = book {
            assert!(b.is_empty());
        }
    }
}
