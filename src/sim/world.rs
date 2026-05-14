//! The single owner of mutable sim state. Phase 3 carries just
//! `MarketRegistry` + one `OrderBook` per contract + the monotonic
//! id sequence. Phase 4 adds `GridpoolRegistry`, a clock, and the
//! tick loop; the World stays the integration point for those.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::broadcast;

use crate::sim::book::OrderBook;
use crate::sim::decimal::is_multiple_of;
use crate::sim::gridpool::{Gridpool, GridpoolRegistry};
use crate::sim::market::{Area, DeliveryPeriod, MarketRegistry};
use crate::sim::matching::{IncomingLimit, LimitMatchOutcome, match_limit};
use crate::sim::order::{
    ExecutionOption, GridpoolId, MarketActor, Order, OrderDetail, OrderId, OrderState, OrderType,
    StateDetail, StateReason,
};
use crate::sim::trade::{PublicTrade, Trade, TradeId, TradeState};

/// Per-gridpool order-update fan-out. Capacity is enough to keep the
/// "lagged" failure mode rare under normal load; a stream consumer
/// that genuinely can't keep up still recovers (with a Lagged error
/// that the gRPC stream task swallows).
const ORDER_BROADCAST_CAPACITY: usize = 256;

/// Global public-trade fan-out + per-gridpool trade fan-out share
/// the same capacity. PublicTrade events are emitted globally (one
/// per fill); Trade events are per-gridpool (two per fill, one per
/// side of the match).
const TRADE_BROADCAST_CAPACITY: usize = 512;

/// (delivery area, delivery period) — the identity of a contract.
/// Cheap to clone; the area code is short and the period is `Copy`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContractKey {
    pub area: Area,
    pub period: DeliveryPeriod,
}

pub struct World {
    markets: MarketRegistry,
    gridpools: GridpoolRegistry,
    /// Per-gridpool fan-out for OrderDetail updates. ReceiveGridpoolOrdersStream
    /// subscribes once and applies the request filter per item.
    gridpool_order_tx: HashMap<GridpoolId, broadcast::Sender<OrderDetail>>,
    /// Per-gridpool fan-out for the maker- and taker-side Trade
    /// records produced by every fill. ReceiveGridpoolTradesStream
    /// subscribes once and applies the request filter.
    gridpool_trade_tx: HashMap<GridpoolId, broadcast::Sender<Trade>>,
    /// Global public-trade tape — one event per fill. The taker
    /// gridpool's submit path emits the public trade; counterparty
    /// fills go through the same path so the tape is the
    /// authoritative match log regardless of who initiated.
    public_trade_tx: broadcast::Sender<PublicTrade>,
    books: HashMap<ContractKey, OrderBook>,
    /// Reverse index from a resting OrderId to the gridpool that owns
    /// it. Populated when an order rests on the book; cleared on full
    /// fill or cancel. The matcher returns `Fill { maker_id, .. }`
    /// and the World needs this map to credit the maker's trade to
    /// its gridpool.
    order_to_gridpool: HashMap<OrderId, GridpoolId>,
    /// Monotonic source of `OrderId`s. Allocated at admit time, so a
    /// rejected order never burns an id.
    next_order_id: u64,
    /// Monotonic source of `TradeId`s. Each Fill from the matcher
    /// produces one PublicTrade and two private Trades — all three
    /// share this id (one trade event, two views).
    next_trade_id: u64,
}

/// Why submit_order can reject. Maps onto the proto's
/// `STATE_REASON_VALIDATION_FAIL` at the gRPC layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    UnknownGridpool,
    UnknownArea,
    AreaNotAllowedForGridpool,
    UnsupportedDurationForMarket,
    UnalignedDeliveryPeriod,
    PriceOffGrid,
    NonPositivePrice,
    QuantityOffGrid,
    NonPositiveQuantity,
    CurrencyMismatch,
    /// Phase 4 only handles LIMIT; the others land in Phase 6+.
    UnsupportedOrderType(OrderType),
    /// Phase 4 ignores execution options; Phase 6 wires AON/FOK/IOC.
    UnsupportedExecutionOption(ExecutionOption),
    /// Cancel: id isn't on the gridpool at all.
    OrderNotFound,
    /// Cancel: id is on the gridpool but already in a terminal state.
    OrderAlreadyTerminal,
}

impl World {
    pub fn new(markets: MarketRegistry) -> Self {
        let (public_trade_tx, _rx) = broadcast::channel(TRADE_BROADCAST_CAPACITY);
        Self {
            markets,
            gridpools: GridpoolRegistry::new(),
            gridpool_order_tx: HashMap::new(),
            gridpool_trade_tx: HashMap::new(),
            public_trade_tx,
            books: HashMap::new(),
            order_to_gridpool: HashMap::new(),
            next_order_id: 1,
            next_trade_id: 1,
        }
    }

    pub fn markets(&self) -> &MarketRegistry {
        &self.markets
    }

    pub fn gridpools(&self) -> &GridpoolRegistry {
        &self.gridpools
    }

    pub fn gridpools_mut(&mut self) -> &mut GridpoolRegistry {
        &mut self.gridpools
    }

    pub fn register_gridpool(&mut self, gp: Gridpool) {
        let id = gp.id;
        self.gridpools.insert(gp);
        let (order_tx, _) = broadcast::channel(ORDER_BROADCAST_CAPACITY);
        self.gridpool_order_tx.insert(id, order_tx);
        let (trade_tx, _) = broadcast::channel(TRADE_BROADCAST_CAPACITY);
        self.gridpool_trade_tx.insert(id, trade_tx);
    }

    /// Subscribe to a gridpool's order-update fan-out. Returns None
    /// if the gridpool isn't registered; the caller turns that into
    /// gRPC NOT_FOUND.
    pub fn subscribe_orders(&self, gridpool_id: GridpoolId) -> Option<broadcast::Receiver<OrderDetail>> {
        self.gridpool_order_tx.get(&gridpool_id).map(|tx| tx.subscribe())
    }

    /// Subscribe to a gridpool's private trade tape.
    pub fn subscribe_gridpool_trades(
        &self,
        gridpool_id: GridpoolId,
    ) -> Option<broadcast::Receiver<Trade>> {
        self.gridpool_trade_tx.get(&gridpool_id).map(|tx| tx.subscribe())
    }

    /// Subscribe to the global public trade tape.
    pub fn subscribe_public_trades(&self) -> broadcast::Receiver<PublicTrade> {
        self.public_trade_tx.subscribe()
    }

    /// Publish an OrderDetail update for `gridpool_id`. No-op if no
    /// subscribers — broadcast::Sender::send returning Err means the
    /// receiver count was zero, which is fine for the sim.
    fn publish_order_update(&self, gridpool_id: GridpoolId, detail: OrderDetail) {
        if let Some(tx) = self.gridpool_order_tx.get(&gridpool_id) {
            let _ = tx.send(detail);
        }
    }

    fn publish_gridpool_trade(&self, gridpool_id: GridpoolId, trade: Trade) {
        if let Some(tx) = self.gridpool_trade_tx.get(&gridpool_id) {
            let _ = tx.send(trade);
        }
    }

    fn publish_public_trade(&self, trade: PublicTrade) {
        let _ = self.public_trade_tx.send(trade);
    }

    /// Record that `order_id` is now resting on the book and belongs
    /// to `gridpool_id`. Called by the admit path when a LIMIT
    /// remainder lands on the book; the matcher uses the reverse
    /// lookup to credit maker-side trades.
    pub fn bind_resting_order(&mut self, order_id: OrderId, gridpool_id: GridpoolId) {
        self.order_to_gridpool.insert(order_id, gridpool_id);
    }

    pub fn owner_of(&self, order_id: OrderId) -> Option<GridpoolId> {
        self.order_to_gridpool.get(&order_id).copied()
    }

    /// Remove the resting-id binding (full fill or cancel). Returns
    /// the previously-bound gridpool, if any.
    pub fn unbind_resting_order(&mut self, order_id: OrderId) -> Option<GridpoolId> {
        self.order_to_gridpool.remove(&order_id)
    }

    /// Mint the next monotonic id. Server-side admit path is the
    /// only legitimate caller; tests bypass this when they want
    /// stable ids.
    pub fn next_id(&mut self) -> OrderId {
        let id = OrderId(self.next_order_id);
        self.next_order_id += 1;
        id
    }

    /// Borrow a contract's book, creating an empty one on demand.
    /// Auto-creation keeps the matcher simple — pre-trade validation
    /// upstream gates which (area, period) pairs are admissible.
    pub fn book_mut(&mut self, key: ContractKey) -> &mut OrderBook {
        self.books.entry(key).or_default()
    }

    pub fn book(&self, key: &ContractKey) -> Option<&OrderBook> {
        self.books.get(key)
    }

    /// Run the continuous matcher for `key` against `incoming`.
    /// Thin wrapper so the server layer doesn't have to fish out
    /// the book itself.
    pub fn match_limit_in(
        &mut self,
        key: ContractKey,
        incoming: IncomingLimit,
    ) -> LimitMatchOutcome {
        match_limit(self.book_mut(key), incoming)
    }

    pub fn contracts(&self) -> impl Iterator<Item = &ContractKey> {
        self.books.keys()
    }

    fn next_trade_id(&mut self) -> TradeId {
        let id = TradeId(self.next_trade_id);
        self.next_trade_id += 1;
        id
    }

    /// Validate-and-admit pipeline. Mirrors the gRPC layer's
    /// CreateGridpoolOrder: rejects with SubmitError for any
    /// pre-trade violation; otherwise mints an id, runs the matcher,
    /// records the resulting trades + state on both sides, and
    /// returns the taker's OrderDetail.
    pub fn submit_order(
        &mut self,
        gridpool_id: GridpoolId,
        order: Order,
        now: DateTime<Utc>,
    ) -> Result<OrderDetail, SubmitError> {
        // 1. Gridpool exists and accepts the area.
        let gp = self
            .gridpools
            .get(gridpool_id)
            .ok_or(SubmitError::UnknownGridpool)?;
        if !gp.allows_area(&order.area) {
            return Err(SubmitError::AreaNotAllowedForGridpool);
        }

        // 2. Market exists for the area.
        let rules = self
            .markets
            .get(&order.area)
            .ok_or(SubmitError::UnknownArea)?;

        // 3. Type / execution-option gating (Phase 4 = LIMIT only).
        if order.order_type != OrderType::Limit {
            return Err(SubmitError::UnsupportedOrderType(order.order_type));
        }
        if let Some(e) = order.execution_option {
            return Err(SubmitError::UnsupportedExecutionOption(e));
        }

        // 4. Currency / duration / alignment / grid checks.
        if order.currency != rules.currency {
            return Err(SubmitError::CurrencyMismatch);
        }
        if !rules.allows(order.period.duration) {
            return Err(SubmitError::UnsupportedDurationForMarket);
        }
        if !order.period.is_aligned() {
            return Err(SubmitError::UnalignedDeliveryPeriod);
        }
        if order.price <= Decimal::ZERO {
            return Err(SubmitError::NonPositivePrice);
        }
        if !is_multiple_of(order.price, rules.price_tick) {
            return Err(SubmitError::PriceOffGrid);
        }
        if order.quantity <= Decimal::ZERO {
            return Err(SubmitError::NonPositiveQuantity);
        }
        if !is_multiple_of(order.quantity, rules.qty_step) {
            return Err(SubmitError::QuantityOffGrid);
        }

        // 5. Admit. Mint id; build the contract key.
        let taker_id = self.next_id();
        let key = ContractKey {
            area: order.area.clone(),
            period: order.period,
        };
        let taker_side = order.side;
        let taker_price = order.price;
        let taker_currency = order.currency;
        let total_qty = order.quantity;

        // 6. Match.
        let outcome = self.match_limit_in(
            key,
            IncomingLimit {
                id: taker_id,
                side: taker_side,
                price: taker_price,
                quantity: total_qty,
            },
        );

        // 7. Record trades on both sides + update maker order state.
        // Each statement scopes its &mut gridpools borrow tightly so
        // self.publish_* calls (which take & self) can interleave.
        for fill in &outcome.fills {
            let trade_id = self.next_trade_id();
            let maker_id = fill.maker_id;
            let maker_gridpool = self
                .owner_of(maker_id)
                .expect("resting order had no gridpool binding");

            let taker_trade = Trade {
                id: trade_id,
                order_id: taker_id,
                side: taker_side,
                area: order.area.clone(),
                period: order.period,
                execution_time: now,
                price: fill.price,
                currency: taker_currency,
                quantity: fill.quantity,
                state: TradeState::Active,
            };
            self.gridpools
                .get_mut(gridpool_id)
                .expect("gridpool existed at start of submit")
                .record_trade(taker_trade.clone());
            self.publish_gridpool_trade(gridpool_id, taker_trade);

            let maker_trade = Trade {
                id: trade_id,
                order_id: maker_id,
                side: taker_side.opposite(),
                area: order.area.clone(),
                period: order.period,
                execution_time: now,
                price: fill.price,
                currency: taker_currency,
                quantity: fill.quantity,
                state: TradeState::Active,
            };
            self.gridpools
                .get_mut(maker_gridpool)
                .expect("bound gridpool exists")
                .record_trade(maker_trade.clone());
            self.publish_gridpool_trade(maker_gridpool, maker_trade);

            // Public tape: one event per fill. Both areas equal in the
            // single-area case Phase 4 supports; cross-area SIDC
            // matches in Phase 7 will fork them.
            self.publish_public_trade(PublicTrade {
                id: trade_id,
                buy_area: order.area.clone(),
                sell_area: order.area.clone(),
                period: order.period,
                execution_time: now,
                price: fill.price,
                currency: taker_currency,
                quantity: fill.quantity,
                state: TradeState::Active,
            });

            let mut maker_fully_filled = false;
            self.gridpools
                .get_mut(maker_gridpool)
                .expect("bound gridpool exists")
                .update_order(maker_id, |d| {
                    d.filled_quantity += fill.quantity;
                    d.open_quantity -= fill.quantity;
                    d.modification_time = now;
                    d.state.actor = MarketActor::System;
                    if d.open_quantity.is_zero() {
                        d.state.state = OrderState::Filled;
                        d.state.reason = StateReason::FullExecution;
                        maker_fully_filled = true;
                    } else {
                        d.state.reason = StateReason::PartialExecution;
                    }
                });
            if maker_fully_filled {
                self.unbind_resting_order(maker_id);
            }
            let maker_detail = self
                .gridpools
                .get(maker_gridpool)
                .and_then(|g| g.get_order(maker_id))
                .cloned();
            if let Some(d) = maker_detail {
                self.publish_order_update(maker_gridpool, d);
            }
        }

        // 8. Build the taker's OrderDetail.
        let filled: Decimal = outcome.fills.iter().map(|f| f.quantity).sum();
        let open = total_qty - filled;
        let (state, reason) = match outcome.rested {
            Some(_) if filled.is_zero() => (OrderState::Active, StateReason::Add),
            Some(_) => (OrderState::Active, StateReason::PartialExecution),
            None => (OrderState::Filled, StateReason::FullExecution),
        };
        let detail = OrderDetail {
            id: taker_id,
            order,
            state: StateDetail {
                state,
                reason,
                actor: MarketActor::User,
            },
            open_quantity: open,
            filled_quantity: filled,
            create_time: now,
            modification_time: now,
        };

        // 9. Record on taker's gridpool; bind if it rests; fan out.
        self.gridpools
            .get_mut(gridpool_id)
            .expect("gridpool still exists")
            .record_order(detail.clone());
        if outcome.rested.is_some() {
            self.bind_resting_order(taker_id, gridpool_id);
        }
        self.publish_order_update(gridpool_id, detail.clone());

        Ok(detail)
    }

    /// Cancel a non-terminal order. Returns the cancelled detail
    /// (state CANCELED + Delete reason). Errors:
    /// `UnknownGridpool`, `OrderNotFound`, `OrderAlreadyTerminal`.
    pub fn cancel_order(
        &mut self,
        gridpool_id: GridpoolId,
        order_id: OrderId,
        now: DateTime<Utc>,
    ) -> Result<OrderDetail, SubmitError> {
        if self.gridpools.get(gridpool_id).is_none() {
            return Err(SubmitError::UnknownGridpool);
        }
        let current_state = self
            .gridpools
            .get(gridpool_id)
            .and_then(|g| g.get_order(order_id))
            .map(|d| d.state.state);
        match current_state {
            None => return Err(SubmitError::OrderNotFound),
            Some(s) if s.is_terminal() => return Err(SubmitError::OrderAlreadyTerminal),
            Some(_) => {}
        }
        // Remove from the book if still resting. For LIMIT-only we
        // can scan books by id since the resting set is small and
        // there's no Phase-4 contract index yet.
        if self.owner_of(order_id) == Some(gridpool_id) {
            self.unbind_resting_order(order_id);
            for book in self.books.values_mut() {
                if book.contains(order_id) {
                    book.cancel(order_id);
                    break;
                }
            }
        }
        let gp = self.gridpools.get_mut(gridpool_id).unwrap();
        gp.update_order(order_id, |d| {
            d.state.state = OrderState::Canceled;
            d.state.reason = StateReason::Delete;
            d.state.actor = MarketActor::User;
            d.modification_time = now;
        });
        let detail = gp.get_order(order_id).cloned().unwrap();
        self.publish_order_update(gridpool_id, detail.clone());
        Ok(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::market::{Currency, DeliveryDuration, MarketRules};
    use crate::sim::order::Side;
    use chrono::{TimeZone, Utc};
    use rust_decimal::dec;

    fn de_lu_hour() -> ContractKey {
        ContractKey {
            area: Area::eic("10Y1001A1001A82H"),
            period: DeliveryPeriod {
                start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                duration: DeliveryDuration::Hour,
            },
        }
    }

    #[test]
    fn next_id_is_monotonic() {
        let mut w = World::new(MarketRegistry::new());
        assert_eq!(w.next_id(), OrderId(1));
        assert_eq!(w.next_id(), OrderId(2));
        assert_eq!(w.next_id(), OrderId(3));
    }

    #[test]
    fn book_mut_auto_creates_then_persists() {
        let mut w = World::new(MarketRegistry::new());
        let k = de_lu_hour();
        assert!(w.book(&k).is_none());
        w.book_mut(k.clone()).insert(
            Side::Buy,
            dec!(85.0),
            crate::sim::book::Resting {
                id: OrderId(1),
                open_qty: dec!(1.0),
            },
        );
        assert!(w.book(&k).is_some());
        assert_eq!(w.book(&k).unwrap().best_bid(), Some(dec!(85.0)));
    }

    #[test]
    fn register_and_lookup_gridpool() {
        let mut w = World::new(MarketRegistry::new());
        let area = Area::eic("10Y1001A1001A82H");
        w.register_gridpool(Gridpool::new(
            GridpoolId(1),
            "battery-arb",
            vec![area.clone()],
        ));
        assert_eq!(w.gridpools().len(), 1);
        let gp = w.gridpools().get(GridpoolId(1)).unwrap();
        assert!(gp.allows_area(&area));
    }

    #[test]
    fn bind_and_unbind_resting_order() {
        let mut w = World::new(MarketRegistry::new());
        w.bind_resting_order(OrderId(7), GridpoolId(2));
        assert_eq!(w.owner_of(OrderId(7)), Some(GridpoolId(2)));
        assert_eq!(w.unbind_resting_order(OrderId(7)), Some(GridpoolId(2)));
        assert_eq!(w.owner_of(OrderId(7)), None);
    }

    fn setup_world_with_pool() -> (World, GridpoolId) {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::de_lu());
        let mut w = World::new(markets);
        let area = Area::eic("10Y1001A1001A82H");
        w.register_gridpool(Gridpool::new(GridpoolId(1), "test", vec![area]));
        (w, GridpoolId(1))
    }

    fn sample_buy(qty: Decimal, price: Decimal) -> Order {
        Order {
            area: Area::eic("10Y1001A1001A82H"),
            period: DeliveryPeriod {
                start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                duration: DeliveryDuration::Hour,
            },
            order_type: OrderType::Limit,
            side: Side::Buy,
            price,
            currency: Currency::Eur,
            quantity: qty,
            stop_price: None,
            peak_price_delta: None,
            display_quantity: None,
            execution_option: None,
            valid_until: None,
            payload: None,
            tag: None,
        }
    }

    fn sample_sell(qty: Decimal, price: Decimal) -> Order {
        Order {
            side: Side::Sell,
            ..sample_buy(qty, price)
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 13, 8, 0, 0).unwrap()
    }

    #[test]
    fn submit_admits_resting_order() {
        let (mut w, gp) = setup_world_with_pool();
        let d = w.submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Active);
        assert_eq!(d.state.reason, StateReason::Add);
        assert_eq!(d.open_quantity, dec!(1.0));
        assert_eq!(d.filled_quantity, dec!(0));
        assert_eq!(w.owner_of(d.id), Some(gp));
        assert_eq!(w.book(&de_lu_hour()).unwrap().best_bid(), Some(dec!(85.0)));
    }

    #[test]
    fn submit_cross_match_fills_both_sides() {
        let (mut w, gp) = setup_world_with_pool();
        let buy = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let sell = w
            .submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();

        // Taker (sell) fully filled.
        assert_eq!(sell.state.state, OrderState::Filled);
        assert_eq!(sell.filled_quantity, dec!(1.0));
        assert_eq!(sell.open_quantity, dec!(0));

        // Maker (buy) updated in-place to Filled.
        let buy_after = w.gridpools().get(gp).unwrap().get_order(buy.id).unwrap();
        assert_eq!(buy_after.state.state, OrderState::Filled);
        assert_eq!(buy_after.state.reason, StateReason::FullExecution);
        assert_eq!(buy_after.filled_quantity, dec!(1.0));

        // Two private trades (one per side); shared trade id.
        let trades = w.gridpools().get(gp).unwrap().trades();
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].id, trades[1].id);

        // Book empty + binding cleared.
        assert!(w.book(&de_lu_hour()).unwrap().is_empty());
        assert!(w.owner_of(buy.id).is_none());
    }

    #[test]
    fn submit_partial_cross_leaves_taker_active() {
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(0.5), dec!(85.0)), t0()).unwrap();
        let buy = w
            .submit_order(gp, sample_buy(dec!(2.0), dec!(85.0)), t0())
            .unwrap();
        assert_eq!(buy.state.state, OrderState::Active);
        assert_eq!(buy.state.reason, StateReason::PartialExecution);
        assert_eq!(buy.filled_quantity, dec!(0.5));
        assert_eq!(buy.open_quantity, dec!(1.5));
        assert_eq!(w.owner_of(buy.id), Some(gp));
    }

    #[test]
    fn submit_validation_errors() {
        let (mut w, gp) = setup_world_with_pool();

        let cases: Vec<(Order, SubmitError)> = vec![
            (
                Order {
                    area: Area::eic("XXXX"),
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::AreaNotAllowedForGridpool,
            ),
            (sample_buy(dec!(1.0), dec!(85.005)), SubmitError::PriceOffGrid),
            (sample_buy(dec!(1.05), dec!(85.0)), SubmitError::QuantityOffGrid),
            (sample_buy(dec!(0), dec!(85.0)), SubmitError::NonPositiveQuantity),
            (
                Order {
                    currency: Currency::Usd,
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::CurrencyMismatch,
            ),
            (
                Order {
                    period: DeliveryPeriod {
                        start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 15, 0).unwrap(),
                        duration: DeliveryDuration::Hour,
                    },
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::UnalignedDeliveryPeriod,
            ),
            (
                Order {
                    order_type: OrderType::Iceberg,
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::UnsupportedOrderType(OrderType::Iceberg),
            ),
            (
                Order {
                    execution_option: Some(ExecutionOption::Fok),
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::UnsupportedExecutionOption(ExecutionOption::Fok),
            ),
        ];
        for (order, want) in cases {
            let err = w.submit_order(gp, order, t0()).unwrap_err();
            assert_eq!(err, want);
        }

        let err = w
            .submit_order(GridpoolId(99), sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap_err();
        assert_eq!(err, SubmitError::UnknownGridpool);
    }

    #[test]
    fn cross_match_emits_public_trade_and_per_gridpool_trades() {
        let (mut w, gp) = setup_world_with_pool();
        let mut public_rx = w.subscribe_public_trades();
        let mut gridpool_rx = w.subscribe_gridpool_trades(gp).unwrap();
        let _buy = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let _sell = w
            .submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();

        // Public tape: one event per fill, here one fill.
        let pt = public_rx.try_recv().unwrap();
        assert_eq!(pt.price, dec!(85.0));
        assert_eq!(pt.quantity, dec!(1.0));
        assert!(public_rx.try_recv().is_err());

        // Private tape on the (self-traded) gridpool: two events per
        // fill (one per side of the match).
        let t1 = gridpool_rx.try_recv().unwrap();
        let t2 = gridpool_rx.try_recv().unwrap();
        assert_eq!(t1.id, t2.id);
        assert_ne!(t1.side, t2.side);
    }

    #[test]
    fn submit_publishes_order_update() {
        let (mut w, gp) = setup_world_with_pool();
        let mut rx = w.subscribe_orders(gp).unwrap();
        let d = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.id, d.id);
        assert_eq!(received.state.state, OrderState::Active);
    }

    #[test]
    fn cancel_publishes_order_update() {
        let (mut w, gp) = setup_world_with_pool();
        let d = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let mut rx = w.subscribe_orders(gp).unwrap();
        w.cancel_order(gp, d.id, t0()).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.state.state, OrderState::Canceled);
    }

    #[test]
    fn cancel_active_order_removes_from_book() {
        let (mut w, gp) = setup_world_with_pool();
        let d = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let cancelled = w.cancel_order(gp, d.id, t0()).unwrap();
        assert_eq!(cancelled.state.state, OrderState::Canceled);
        assert_eq!(cancelled.state.reason, StateReason::Delete);
        assert!(w.owner_of(d.id).is_none());
        assert!(w.book(&de_lu_hour()).unwrap().is_empty());
    }

    #[test]
    fn cancel_errors() {
        let (mut w, gp) = setup_world_with_pool();
        let d = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();

        assert_eq!(
            w.cancel_order(GridpoolId(99), d.id, t0()).unwrap_err(),
            SubmitError::UnknownGridpool
        );
        assert_eq!(
            w.cancel_order(gp, OrderId(9999), t0()).unwrap_err(),
            SubmitError::OrderNotFound
        );

        // First cancel succeeds; second errors out as already-terminal.
        w.cancel_order(gp, d.id, t0()).unwrap();
        assert_eq!(
            w.cancel_order(gp, d.id, t0()).unwrap_err(),
            SubmitError::OrderAlreadyTerminal
        );
    }

    #[test]
    fn match_limit_in_routes_to_right_contract() {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::de_lu());
        let mut w = World::new(markets);

        let k = de_lu_hour();
        let id1 = w.next_id();
        w.match_limit_in(
            k.clone(),
            IncomingLimit {
                id: id1,
                side: Side::Sell,
                price: dec!(85.0),
                quantity: dec!(1.0),
            },
        );

        let id2 = w.next_id();
        let out = w.match_limit_in(
            k.clone(),
            IncomingLimit {
                id: id2,
                side: Side::Buy,
                price: dec!(85.0),
                quantity: dec!(1.0),
            },
        );

        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].maker_id, id1);
        assert_eq!(out.fills[0].taker_id, id2);
        assert!(w.book(&k).unwrap().is_empty());
    }
}
