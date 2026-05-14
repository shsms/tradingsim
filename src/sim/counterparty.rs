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
                    duration: DeliveryDuration::DeliveryDuration60,
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
