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
use rand::SeedableRng;
use rand::rngs::SmallRng;
use rust_decimal::Decimal;
use rust_decimal::dec;

use crate::sim::decimal::snap_to_tick;
use crate::sim::market::{Area, Currency, DeliveryPeriod};
use crate::sim::order::{Order, OrderId, Side};
use crate::sim::world::World;

/// Knobs the binary (and later, lisp) sets per market-maker.
#[derive(Clone, Debug)]
pub struct MarketMakerConfig {
    pub area: Area,
    pub period: DeliveryPeriod,
    pub currency: Currency,
    /// Fundamentals target, EUR/MWh. The scenario bias tick rewrites
    /// this every 5 s from `forward_curve + weather + stage_bias`;
    /// the MM refresh mean-reverts `reference_price` toward it. The
    /// split lets `follow_last_trade` actually accumulate over many
    /// refreshes — previously the bias tick clobbered the drifted
    /// reference straight back to the curve value 5 s later, so
    /// quotes never moved without a stage change.
    pub reference_baseline: Decimal,
    /// Live mid-price, EUR/MWh. Walks each refresh: mean-reverts
    /// toward `reference_baseline`, follows recent public trades at
    /// `follow_last_trade`, plus a random step scaled by
    /// `price_noise` × horizon-to-gate. Read by `quote_pair` for
    /// posting bid/ask.
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
    pub fn default_for(area: Area, period: DeliveryPeriod) -> Self {
        Self {
            area,
            period,
            currency: Currency::Eur,
            reference_baseline: dec!(85.00),
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

    /// Step the reference, compute a fresh bid + ask, then — only
    /// once the new pair is known to be valid — cancel the old
    /// quotes and post the new ones.
    pub fn refresh(&mut self, world: &mut World, now: DateTime<Utc>) {
        // Step the reference once per refresh — mean-revert toward
        // the fundamentals baseline, follow the last public trade,
        // and take one random-walk step whose amplitude grows as
        // delivery approaches.
        self.step_reference(world, now);

        // Snapshot the config under the read lock — the lisp side may
        // race us on a write, but we want one consistent set of knobs
        // for this quote pair.
        let cfg = self.config.read().clone();

        let mid = cfg.reference_price;
        let bid_price = snap_to_tick(mid - cfg.spread + cfg.demand, cfg.tick);
        let ask_price = snap_to_tick(mid + cfg.spread - cfg.surplus, cfg.tick);

        if bid_price >= ask_price {
            // Demand + surplus collapsed the spread. Leave the
            // previous quotes resting — wiping them would create a
            // window where the MM contributes no liquidity, and
            // aggressors firing through that window have lifted
            // stale far-off-market orders in the past.
            return;
        }

        if let Some(id) = self.bid_id.take() {
            world.cancel_counterparty_order(id);
        }
        if let Some(id) = self.ask_id.take() {
            world.cancel_counterparty_order(id);
        }

        if let Ok(id) = world.submit_counterparty_order(build(&cfg, Side::Buy, bid_price), now) {
            self.bid_id = Some(id);
        }
        if let Ok(id) = world.submit_counterparty_order(build(&cfg, Side::Sell, ask_price), now) {
            self.ask_id = Some(id);
        }
    }

    /// Move `reference_price` one refresh forward. Three additive
    /// components:
    ///
    /// 1. Mean-reversion toward `reference_baseline` at
    ///    [`MEAN_REVERT_RATE`] per tick — half-life ≈ 35 refreshes
    ///    (~70 s at the default 2 s cadence). Slow enough that
    ///    follow-last-trade and walk can accumulate visible drift
    ///    between scenario stage transitions; fast enough that the
    ///    quote catches up to a new stage's fundamentals within
    ///    about a minute.
    /// 2. Follow-last-trade pull: a `follow_last_trade` × gap step
    ///    toward the most recent public trade on this contract.
    /// 3. Random-walk step of amplitude `price_noise`, multiplied
    ///    by a horizon-to-gate scale (1× at ≥2 h to gate, ramping
    ///    linearly to 3× at gate close). Real intraday markets'
    ///    volatility increases as delivery approaches; this
    ///    approximates that ramp.
    fn step_reference(&mut self, world: &World, now: DateTime<Utc>) {
        let (area, period, baseline, current, follow_rate, walk_amp) = {
            let c = self.config.read();
            (
                c.area.clone(),
                c.period,
                c.reference_baseline,
                c.reference_price,
                c.follow_last_trade,
                c.price_noise,
            )
        };

        let mut new_ref = current + (baseline - current) * MEAN_REVERT_RATE;

        if follow_rate > Decimal::ZERO {
            let hist = world.public_trade_history();
            let last = hist
                .iter()
                .rev()
                .find(|t| t.period == period && (t.buy_area == area || t.sell_area == area));
            if let Some(t) = last {
                new_ref += (t.price - current) * follow_rate;
            }
        }

        if !walk_amp.is_zero() {
            let n: i64 = self.rng.gen_range(-100..=100);
            let scale = gate_close_scale(period.start, now);
            let amp = walk_amp * Decimal::try_from(scale).unwrap_or(Decimal::ONE);
            new_ref += Decimal::new(n, 2) * amp;
        }

        self.config.write().reference_price = new_ref;
    }
}

/// Per-refresh fraction of the gap to `reference_baseline` that
/// `step_reference` closes. Calibrated as a half-life: a value `r`
/// gives half-life `ln(2) / r` refreshes, so 0.02 → 35 refreshes
/// (~70 s at the default 2 s cadence).
const MEAN_REVERT_RATE: Decimal = dec!(0.02);

/// Scales `price_noise` as the contract approaches gate close
/// (= `period.start`). 1.0 at ≥2 h to gate, linear ramp to 3.0
/// right at gate. Past gate the MM shouldn't be quoting anyway,
/// but for safety the value clamps non-negative.
///
/// Also used by the aggressor spawner to shorten the inter-fire
/// sleep + grow per-fire size near gate, so contract volume is
/// back-loaded the way real intraday is rather than uniform.
pub fn gate_close_scale(gate: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let secs = (gate - now).num_seconds().max(0) as f64;
    let hours = secs / 3600.0;
    let proximity = ((2.0 - hours) / 2.0).clamp(0.0, 1.0);
    1.0 + 2.0 * proximity
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
    /// Fair-value anchor — what the aggressor thinks the contract
    /// is worth. The bias tick writes this each cycle from
    /// forward_curve + weather + scenario, same way it writes
    /// MM reference_baseline. Aggressor fires at
    /// reference_price ± max_slippage so a stale resting order
    /// 100 EUR off market can't pull it away from sensible price.
    pub reference_price: Decimal,
    /// Inter-fire wait, milliseconds, before gate_close_scale ramps
    /// it down near the contract gate. Lives on the live config (not
    /// the spawn-time AggressorSpec) so editing it in config.lisp and
    /// hot-reloading, or calling `(set-aggressor-rate-ms …)`, takes
    /// effect on the running task's next iteration. 50 ms floor is
    /// enforced by every write site so a typo can't busy-spin the
    /// task.
    pub rate_ms: u64,
    /// Maximum slippage (EUR) from reference the aggressor accepts.
    /// 5.0 covers the MM's typical 0.40 half-spread plus a few
    /// EUR of natural intraday wandering, and stays well under
    /// what a runaway resting order needs to escape detection.
    pub max_slippage: Decimal,
}

impl AggressorConfig {
    pub fn default_for(area: Area, period: DeliveryPeriod) -> Self {
        Self {
            area,
            period,
            currency: Currency::Eur,
            size: dec!(0.2),
            side_bias: 0.5,
            reference_price: dec!(85.00),
            rate_ms: 1000,
            max_slippage: dec!(5.00),
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

    /// Pick a side (random with bias), check the book's best
    /// opposite is within the aggressor's slippage band of its
    /// reference, and submit an IOC limit at reference ± slippage.
    /// No-op when the opposite side is empty or only carries
    /// orders that are too far off fair value (previously the
    /// aggressor would happily lift a 173 EUR resting ask while
    /// it thought the contract was worth ~80 — IOC at a sane
    /// limit price stops that).
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
        let limit_price = match side {
            Side::Buy => cfg.reference_price + cfg.max_slippage,
            Side::Sell => cfg.reference_price - cfg.max_slippage,
            Side::Unspecified => return,
        };
        let limit_price = snap_to_tick(limit_price, dec!(0.01)).max(dec!(0.01));
        // Bail if the book has nothing on the opposite side, or
        // if its best price is outside the slippage band — the
        // aggressor would rather not trade than lift a stale
        // far-off order.
        let crosses = world
            .book(&key)
            .and_then(|b| match side {
                Side::Buy => b.best_ask(),
                Side::Sell => b.best_bid(),
                _ => None,
            })
            .map(|p| match side {
                Side::Buy => p <= limit_price,
                Side::Sell => p >= limit_price,
                _ => false,
            })
            .unwrap_or(false);
        if !crosses {
            return;
        }
        let target_price = limit_price;
        // Grow per-fire size as the gate approaches: 1× at ≥2 h
        // out, ramping to 2× right at gate (gate_close_scale's
        // 1→3 ramp, halved). Combined with the rate ramp in the
        // spawn task, this back-loads volume the way real
        // intraday curves do without making one print absurdly
        // larger than the surrounding ones.
        let rate_scale = gate_close_scale(cfg.period.start, now);
        let size_mult = (rate_scale + 1.0) / 2.0;
        let scaled_size = snap_to_tick(
            cfg.size * Decimal::try_from(size_mult).unwrap_or(Decimal::ONE),
            crate::sim::decimal::DEFAULT_QTY_STEP,
        )
        .max(crate::sim::decimal::DEFAULT_QTY_STEP);
        // IOC — take what crosses at the slippage-bounded limit;
        // kill the rest. Stops aggressors from accumulating
        // resting residuals on the book that distort the bid/ask
        // shape on subsequent fires.
        let order = Order {
            execution_option: Some(crate::sim::order::ExecutionOption::Ioc),
            ..Order::limit(
                cfg.area.clone(),
                cfg.period,
                side,
                target_price,
                scaled_size,
                cfg.currency,
            )
        };
        let _ = world.submit_counterparty_order(order, now);
    }
}

fn build(cfg: &MarketMakerConfig, side: Side, price: Decimal) -> Order {
    Order::limit(
        cfg.area.clone(),
        cfg.period,
        side,
        price,
        cfg.size,
        cfg.currency,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::market::{DeliveryDuration, MarketRegistry, MarketRules};
    use chrono::TimeZone;

    fn setup_world() -> (World, MarketMakerConfig) {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        let world = World::new(markets);
        let cfg = MarketMakerConfig {
            price_noise: dec!(0), // deterministic
            ..MarketMakerConfig::default_for(
                Area::eic("10YDE-EON------1"),
                DeliveryPeriod {
                    start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                    duration: DeliveryDuration::DeliveryDuration15,
                },
            )
        };
        (world, cfg)
    }

    /// Deterministic "now" four hours before the test period — sits
    /// well inside the trading window so the gate-closed check at
    /// submission time passes.
    fn t0() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 13, 8, 0, 0).unwrap()
    }

    #[test]
    fn refresh_posts_bid_and_ask_around_reference() {
        let (mut world, cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: cfg.area.clone(),
            period: cfg.period,
        };
        let mut mm = MarketMaker::new(cfg.clone(), 42);
        mm.refresh(&mut world, t0());
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
        mm.refresh(&mut world, t0());
        mm.refresh(&mut world, t0());
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
        mm.refresh(&mut world, t0());
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
        mm.refresh(&mut world, t0());
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
        mm.refresh(&mut world, t0());
        let pre_bid = world.book(&key).unwrap().best_bid().unwrap();

        // External writer raises demand by 0.20 EUR/MWh.
        shared.write().demand = dec!(0.20);
        mm.refresh(&mut world, t0());
        let post_bid = world.book(&key).unwrap().best_bid().unwrap();
        assert_eq!(post_bid - pre_bid, dec!(0.20));
    }

    #[test]
    fn aggressor_buys_against_best_ask() {
        let (mut world, mm_cfg) = setup_world();
        let mut mm = MarketMaker::new(mm_cfg.clone(), 42);
        mm.refresh(&mut world, t0());
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
            ..AggressorConfig::default_for(mm_cfg.area.clone(), mm_cfg.period)
        };
        let mut ag = Aggressor::new(ag_cfg, 7);
        ag.fire(&mut world, t0());
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
    fn aggressor_skips_when_best_opposite_exceeds_slippage() {
        // Regression for: user places a sell at 173 EUR while the
        // MM reference (and aggressor fair value) is ~80 EUR.
        // With size > MM depth, an aggressor previously depleted
        // the MM ask and then lifted the user's 173 sell on its
        // next fire because target_price tracked best_ask
        // blindly. The slippage clamp now stops that.
        let (mut world, mm_cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: mm_cfg.area.clone(),
            period: mm_cfg.period,
        };
        // Seed a far-off-market sell. Two crossing counterparty
        // orders place it without anyone immediately taking it.
        let stale_sell = Order::limit(
            mm_cfg.area.clone(),
            mm_cfg.period,
            Side::Sell,
            dec!(173.00),
            dec!(10.0),
            Currency::Eur,
        );
        world
            .submit_counterparty_order(stale_sell, t0())
            .expect("place stale sell");
        let trades_before = world.public_trade_history().len();

        // Aggressor with reference 80 + slippage 5 sees a 173 ask
        // and refuses to fire — the crossing check fails.
        let ag_cfg = AggressorConfig {
            side_bias: 1.0, // always buy
            size: dec!(0.5),
            reference_price: dec!(80.00),
            max_slippage: dec!(5.00),
            ..AggressorConfig::default_for(mm_cfg.area.clone(), mm_cfg.period)
        };
        let mut ag = Aggressor::new(ag_cfg, 0);
        for _ in 0..10 {
            ag.fire(&mut world, t0());
        }
        // No trades happened; the 173 sell still sits untouched.
        assert_eq!(world.public_trade_history().len(), trades_before);
        let book = world.book(&key).unwrap();
        assert_eq!(book.depth_at(Side::Sell, dec!(173.00)), dec!(10.0));
    }

    #[test]
    fn aggressor_skips_empty_book() {
        let (mut world, mm_cfg) = setup_world();
        // No MM refresh — book is empty for this contract.
        let ag_cfg = AggressorConfig::default_for(mm_cfg.area.clone(), mm_cfg.period);
        let mut ag = Aggressor::new(ag_cfg, 0);
        ag.fire(&mut world, t0());
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
        let mk = |side, price| {
            Order::limit(cfg.area.clone(), cfg.period, side, price, dec!(0.1), Currency::Eur)
        };
        world
            .submit_counterparty_order(mk(Side::Sell, dec!(90.0)), t0())
            .unwrap();
        world
            .submit_counterparty_order(mk(Side::Buy, dec!(90.0)), t0())
            .unwrap();

        // Now spawn the MM with drift enabled. First refresh sees
        // the seeded trade at 90 → ref pulls 85 → 87.50.
        let shared = Arc::new(RwLock::new(cfg.clone()));
        let mut mm = MarketMaker::with_shared_config(shared.clone(), 0);
        mm.refresh(&mut world, t0());
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
        mm.refresh(&mut world, t0());
        let book = world.book(&key);
        // Book may not even exist yet; if it does, it should be empty.
        if let Some(b) = book {
            assert!(b.is_empty());
        }
    }

    #[test]
    fn gate_close_scale_known_horizons() {
        let gate = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        // ≥ 2 h to gate → base 1×
        assert!((gate_close_scale(gate, gate - chrono::Duration::hours(2)) - 1.0).abs() < 1e-9);
        assert!((gate_close_scale(gate, gate - chrono::Duration::hours(4)) - 1.0).abs() < 1e-9);
        // 1 h to gate → halfway → 2×
        assert!((gate_close_scale(gate, gate - chrono::Duration::hours(1)) - 2.0).abs() < 1e-9);
        // 30 min → 2.5×
        assert!((gate_close_scale(gate, gate - chrono::Duration::minutes(30)) - 2.5).abs() < 1e-9);
        // At gate → 3×
        assert!((gate_close_scale(gate, gate) - 3.0).abs() < 1e-9);
        // Past gate (negative horizon) clamps to 3×; the
        // aggressor / MM shouldn't be running but be defensive.
        assert!((gate_close_scale(gate, gate + chrono::Duration::hours(1)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn mm_step_reference_mean_reverts_toward_baseline() {
        let (mut world, mut cfg) = setup_world();
        cfg.price_noise = dec!(0); // disable the walk
        cfg.follow_last_trade = dec!(0); // disable follow
        cfg.reference_baseline = dec!(90.00); // target 5 EUR above price
        cfg.reference_price = dec!(85.00);
        let mut mm = MarketMaker::new(cfg, 0);
        mm.refresh(&mut world, t0());
        // MEAN_REVERT_RATE = 0.02. 85 + 0.02 * 5 = 85.10
        let r1 = mm.shared_config().read().reference_price;
        assert_eq!(r1, dec!(85.10));
        mm.refresh(&mut world, t0());
        // 85.10 + 0.02 * (90 - 85.10) = 85.198
        let r2 = mm.shared_config().read().reference_price;
        assert_eq!(r2, dec!(85.198));
    }

    #[test]
    fn mm_step_reference_baseline_equal_to_price_is_a_no_op() {
        let (mut world, mut cfg) = setup_world();
        cfg.price_noise = dec!(0);
        cfg.follow_last_trade = dec!(0);
        cfg.reference_baseline = dec!(85.00);
        cfg.reference_price = dec!(85.00);
        let mut mm = MarketMaker::new(cfg, 0);
        mm.refresh(&mut world, t0());
        assert_eq!(mm.shared_config().read().reference_price, dec!(85.00));
    }

    /// Helper for the aggressor tests: post a resting MM bid at
    /// `bid_price` so a sell-side aggressor has someone to cross.
    /// Returns (world, mm_cfg) for further setup.
    fn world_with_resting_bid(now: chrono::DateTime<Utc>, bid_price: Decimal) -> World {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        let mut world = World::new(markets);
        // Spawn a chubby MM whose bid sits where we want it. Push
        // reference well above bid_price so the ask is also far
        // above — guarantees the aggressor's sell crosses the bid.
        let mm_cfg = MarketMakerConfig {
            reference_baseline: bid_price + dec!(0.40),
            reference_price: bid_price + dec!(0.40),
            spread: dec!(0.40),
            size: dec!(50.0), // huge so it absorbs many fires
            price_noise: dec!(0),
            ..MarketMakerConfig::default_for(
                Area::eic("10YDE-EON------1"),
                DeliveryPeriod {
                    start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                    duration: DeliveryDuration::DeliveryDuration15,
                },
            )
        };
        let mut mm = MarketMaker::new(mm_cfg, 99);
        mm.refresh(&mut world, now);
        world
    }

    #[test]
    fn aggressor_fire_size_ramps_at_gate() {
        let gate = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let bid_price = dec!(80.00);

        // Fire 2 h from gate — size should land at base (no ramp).
        let mut world_far = world_with_resting_bid(gate - chrono::Duration::hours(2), bid_price);
        let ag_cfg = AggressorConfig {
            side_bias: 0.0, // always sell — crosses the resting bid
            size: dec!(0.5),
            ..AggressorConfig::default_for(
                Area::eic("10YDE-EON------1"),
                DeliveryPeriod {
                    start: gate,
                    duration: DeliveryDuration::DeliveryDuration15,
                },
            )
        };
        let mut ag_far = Aggressor::new(ag_cfg.clone(), 0);
        ag_far.fire(&mut world_far, gate - chrono::Duration::hours(2));
        let trade_far = world_far.public_trade_history().last().cloned().unwrap();

        // Fire 1 min before gate — submit_order rejects at/after
        // delivery start, so step right up to the edge instead of
        // landing on it. The scale at 1 min out is ~2.99 → size
        // mult ≈ 1.99 → snaps to 1.0 on the 0.1 grid.
        let just_before_gate = gate - chrono::Duration::minutes(1);
        let mut world_close = world_with_resting_bid(just_before_gate, bid_price);
        let mut ag_close = Aggressor::new(ag_cfg, 0);
        ag_close.fire(&mut world_close, just_before_gate);
        let trade_close = world_close.public_trade_history().last().cloned().unwrap();

        // Base 0.5 × 1.0 = 0.5 (snapped to 0.5 on 0.1 grid).
        // Gate  0.5 × 1.99 ≈ 1.0 (snap-up).
        assert_eq!(trade_far.quantity, dec!(0.5));
        assert_eq!(trade_close.quantity, dec!(1.0));
    }

    #[test]
    fn aggressor_ioc_residual_does_not_rest() {
        // Regression: pre-fix, submit_counterparty_order hardcoded
        // ExecMode::Resting so an aggressor's IOC flag was ignored.
        // A 1 MW aggressor buy at limit 55 with only 0.4 MW of
        // crossable ask depth would rest 0.6 MW on the bid side at
        // 55, and the next opposite-side fire would trade against
        // it — producing a stream of trades at the slippage cap
        // (e.g. repeated 55 EUR prints on night-curve contracts).
        let (mut world, mm_cfg) = setup_world();
        let key = crate::sim::world::ContractKey {
            area: mm_cfg.area.clone(),
            period: mm_cfg.period,
        };
        // Seed a thin MM ask at 50 with only 0.4 MW of depth.
        let thin_ask = Order::limit(
            mm_cfg.area.clone(),
            mm_cfg.period,
            Side::Sell,
            dec!(50.00),
            dec!(0.4),
            Currency::Eur,
        );
        world
            .submit_counterparty_order(thin_ask, t0())
            .expect("seed thin ask");

        // Aggressor with reference 50 + slippage 5 wants 1 MW.
        // Eats the 0.4 MW thin ask; remainder (0.6 MW) must NOT
        // rest at the slippage-cap (55) — IOC should kill it.
        let ag_cfg = AggressorConfig {
            side_bias: 1.0, // always buy
            size: dec!(1.0),
            reference_price: dec!(50.00),
            max_slippage: dec!(5.00),
            ..AggressorConfig::default_for(mm_cfg.area.clone(), mm_cfg.period)
        };
        let mut ag = Aggressor::new(ag_cfg, 0);
        ag.fire(&mut world, t0());

        // One fill happened (the 0.4 MW thin ask), and nothing
        // rests on the bid side — IOC dropped the leftover.
        let trades = world.public_trade_history();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].price, dec!(50.00));
        let book = world.book(&key).unwrap();
        assert_eq!(book.best_bid(), None);
    }
}
