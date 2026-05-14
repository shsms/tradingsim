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
use crate::sim::matching::{ExecMode, IncomingLimit, LimitMatchOutcome, match_limit};
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
    /// Cancel / modify: id isn't on the gridpool at all.
    OrderNotFound,
    /// Cancel / modify: id is on the gridpool but already in a
    /// terminal state.
    OrderAlreadyTerminal,
    /// Modify: the new quantity is below the already-filled amount.
    ModifyQuantityBelowFilled,
}

/// Field-set the Update RPC can write. None = "leave alone".
#[derive(Clone, Debug, Default)]
pub struct OrderUpdate {
    pub price: Option<Decimal>,
    pub quantity: Option<Decimal>,
    pub valid_until: Option<DateTime<Utc>>,
    pub tag: Option<String>,
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

    /// Run the continuous matcher for `key` against `incoming` with
    /// the default Resting mode. Thin wrapper so the server layer
    /// doesn't have to fish out the book itself.
    pub fn match_limit_in(
        &mut self,
        key: ContractKey,
        incoming: IncomingLimit,
    ) -> LimitMatchOutcome {
        match_limit(self.book_mut(key), incoming, ExecMode::Resting)
    }

    /// Variant of `match_limit_in` that lets the caller pick the
    /// execution mode (FOK / IOC / Resting).
    pub fn match_limit_in_with_mode(
        &mut self,
        key: ContractKey,
        incoming: IncomingLimit,
        mode: ExecMode,
    ) -> LimitMatchOutcome {
        match_limit(self.book_mut(key), incoming, mode)
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

        // 2-4. Shared validation: market, type, grid.
        self.validate_common(&order)?;
        // FOK / IOC are honoured starting in Phase 6. AON is still
        // rejected — partial-quantity-or-rest semantics are open.
        if let Some(ExecutionOption::Aon) = order.execution_option {
            return Err(SubmitError::UnsupportedExecutionOption(
                ExecutionOption::Aon,
            ));
        }
        if order.execution_option.is_some() && order.valid_until.is_some() {
            return Err(SubmitError::UnsupportedExecutionOption(
                order.execution_option.unwrap(),
            ));
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

        // 6. Match — execution option picks the matcher mode.
        let mode = match order.execution_option {
            Some(ExecutionOption::Fok) => ExecMode::FillOrKill,
            Some(ExecutionOption::Ioc) => ExecMode::ImmediateOrCancel,
            _ => ExecMode::Resting,
        };
        let outcome = self.match_limit_in_with_mode(
            key,
            IncomingLimit {
                id: taker_id,
                side: taker_side,
                price: taker_price,
                quantity: total_qty,
            },
            mode,
        );

        // 7. Record trades + update maker state. The taker is always
        // a gridpool order; the maker may be a gridpool order or an
        // unbound counterparty order — owner_of returns None for the
        // latter, and we skip the gridpool-side bookkeeping then.
        for fill in &outcome.fills {
            self.record_fill(
                fill,
                taker_id,
                taker_side,
                taker_currency,
                Some(gridpool_id),
                order.area.clone(),
                order.period,
                now,
            );
        }

        // 8. Build the taker's OrderDetail.
        let filled: Decimal = outcome.fills.iter().map(|f| f.quantity).sum();
        let open = total_qty - filled;
        // FOK / IOC outcomes:
        //   FOK + 0 fills = no execution at all → Canceled, Reject.
        //   IOC + filled < total = killed leftover → Canceled if any
        //     leftover; Filled if all matched.
        //   Resting + filled < total but no rest = unreachable for now.
        // (mode, rested, no-fills, fully-filled) → (state, reason).
        let (state, reason) = match (mode, outcome.rested.is_some(), filled.is_zero(), open.is_zero()) {
            // Restable + on-book outcomes (Resting only — IOC/FOK
            // never rest):
            (_, true, true, _) => (OrderState::Active, StateReason::Add),
            (_, true, false, _) => (OrderState::Active, StateReason::PartialExecution),
            // Fully filled outcomes (any mode):
            (_, false, false, true) => (OrderState::Filled, StateReason::FullExecution),
            // FOK: pre-check fails → 0 fills, no rest → Canceled.
            (ExecMode::FillOrKill, false, true, _) => (OrderState::Canceled, StateReason::Reject),
            // IOC: some fills, leftover killed → Canceled with
            // PartialExecution reason.
            (ExecMode::ImmediateOrCancel, false, false, false) => {
                (OrderState::Canceled, StateReason::PartialExecution)
            }
            // IOC: no fills, nothing to take → Canceled / Reject.
            (ExecMode::ImmediateOrCancel, false, true, _) => {
                (OrderState::Canceled, StateReason::Reject)
            }
            // Resting w/o rest is only the "fully filled" case above.
            // FOK can't reach (filled=true, fully=false) because the
            // pre-check makes the matcher take everything or nothing.
            // Cover the unreachable arms with Failed/Reject so the
            // exhaustiveness checker is satisfied.
            _ => (OrderState::Failed, StateReason::Reject),
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

    /// Run the validation rules (gridpool / area-allow checks
    /// excluded) for an Order; used by both submit_order and
    /// submit_counterparty_order. Returns the matched MarketRules
    /// so the caller can hand them to the matcher without a second
    /// lookup.
    fn validate_common(&self, order: &Order) -> Result<(), SubmitError> {
        let rules = self
            .markets
            .get(&order.area)
            .ok_or(SubmitError::UnknownArea)?;
        if order.order_type != OrderType::Limit {
            return Err(SubmitError::UnsupportedOrderType(order.order_type));
        }
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
        Ok(())
    }

    /// Apply one fill: build + record trades, update maker state if
    /// any, emit the public-tape event. `taker_gridpool` is None for
    /// counterparty takers (no gridpool-side bookkeeping then).
    #[allow(clippy::too_many_arguments)]
    fn record_fill(
        &mut self,
        fill: &crate::sim::matching::Fill,
        taker_id: OrderId,
        taker_side: crate::sim::order::Side,
        taker_currency: crate::sim::market::Currency,
        taker_gridpool: Option<GridpoolId>,
        area: Area,
        period: DeliveryPeriod,
        now: DateTime<Utc>,
    ) {
        let trade_id = self.next_trade_id();
        let maker_id = fill.maker_id;
        let maker_gridpool = self.owner_of(maker_id);

        // Taker side: only credit to a gridpool if the taker is one.
        if let Some(gp) = taker_gridpool {
            let trade = Trade {
                id: trade_id,
                order_id: taker_id,
                side: taker_side,
                area: area.clone(),
                period,
                execution_time: now,
                price: fill.price,
                currency: taker_currency,
                quantity: fill.quantity,
                state: TradeState::Active,
            };
            self.gridpools
                .get_mut(gp)
                .expect("taker gridpool exists")
                .record_trade(trade.clone());
            self.publish_gridpool_trade(gp, trade);
        }

        // Maker side: same, only if the maker belongs to a gridpool.
        if let Some(mgp) = maker_gridpool {
            let trade = Trade {
                id: trade_id,
                order_id: maker_id,
                side: taker_side.opposite(),
                area: area.clone(),
                period,
                execution_time: now,
                price: fill.price,
                currency: taker_currency,
                quantity: fill.quantity,
                state: TradeState::Active,
            };
            self.gridpools
                .get_mut(mgp)
                .expect("maker gridpool exists")
                .record_trade(trade.clone());
            self.publish_gridpool_trade(mgp, trade);
        }

        // Public tape: one event per fill, always.
        self.publish_public_trade(PublicTrade {
            id: trade_id,
            buy_area: area.clone(),
            sell_area: area.clone(),
            period,
            execution_time: now,
            price: fill.price,
            currency: taker_currency,
            quantity: fill.quantity,
            state: TradeState::Active,
        });

        // Maker order state update + fan-out (if maker is gridpool-owned).
        if let Some(mgp) = maker_gridpool {
            let mut fully_filled = false;
            self.gridpools
                .get_mut(mgp)
                .expect("maker gridpool exists")
                .update_order(maker_id, |d| {
                    d.filled_quantity += fill.quantity;
                    d.open_quantity -= fill.quantity;
                    d.modification_time = now;
                    d.state.actor = MarketActor::System;
                    if d.open_quantity.is_zero() {
                        d.state.state = OrderState::Filled;
                        d.state.reason = StateReason::FullExecution;
                        fully_filled = true;
                    } else {
                        d.state.reason = StateReason::PartialExecution;
                    }
                });
            if fully_filled {
                self.unbind_resting_order(maker_id);
            }
            if let Some(d) = self
                .gridpools
                .get(mgp)
                .and_then(|g| g.get_order(maker_id))
                .cloned()
            {
                self.publish_order_update(mgp, d);
            }
        } else {
            // Counterparty maker: no order to update, but if the
            // matcher fully consumed it, the book already popped it
            // — nothing else to clean up.
        }
    }

    /// Admit a counterparty (non-gridpool) order. Same validation as
    /// submit_order minus the gridpool / area-allow gates. Returns
    /// the OrderId of the admitted order; the caller uses it for
    /// subsequent cancel_counterparty_order calls. Fills emit
    /// public-tape events and maker-side gridpool trades as usual.
    pub fn submit_counterparty_order(
        &mut self,
        order: Order,
        now: DateTime<Utc>,
    ) -> Result<OrderId, SubmitError> {
        self.validate_common(&order)?;

        let taker_id = self.next_id();
        let key = ContractKey {
            area: order.area.clone(),
            period: order.period,
        };
        let taker_side = order.side;
        let taker_currency = order.currency;

        let outcome = self.match_limit_in(
            key,
            IncomingLimit {
                id: taker_id,
                side: taker_side,
                price: order.price,
                quantity: order.quantity,
            },
        );
        for fill in &outcome.fills {
            self.record_fill(
                fill,
                taker_id,
                taker_side,
                taker_currency,
                None,
                order.area.clone(),
                order.period,
                now,
            );
        }
        // Counterparty leftovers rest on the book without a binding —
        // owner_of stays None, and cancel_counterparty_order is the
        // only way they leave.
        Ok(taker_id)
    }

    /// Remove a resting counterparty order from the book. No state
    /// flip (counterparty orders don't have an OrderDetail). Returns
    /// true if the id was on the book, false otherwise.
    pub fn cancel_counterparty_order(&mut self, order_id: OrderId) -> bool {
        let mut found = false;
        for book in self.books.values_mut() {
            if book.contains(order_id) {
                book.cancel(order_id);
                found = true;
                break;
            }
        }
        // Belt-and-suspenders: an accidental binding (shouldn't
        // happen, but cheap) gets cleared.
        self.unbind_resting_order(order_id);
        found
    }

    /// Walk every gridpool's order index and transition any
    /// non-terminal order whose `valid_until` has lapsed to
    /// `OrderState::Expired`. Returns the count of orders touched so
    /// the caller can log expiry activity. The driver loop in the
    /// binary calls this periodically; clients can also call it from
    /// the lisp side via a future `(expire-lapsed-orders)` defun.
    pub fn expire_lapsed_orders(&mut self, now: DateTime<Utc>) -> usize {
        let mut expirables: Vec<(GridpoolId, OrderId, ContractKey)> = Vec::new();
        for gp in self.gridpools.iter() {
            for d in gp.orders() {
                if d.state.state.is_terminal() {
                    continue;
                }
                if let Some(vu) = d.order.valid_until {
                    if vu <= now {
                        expirables.push((
                            gp.id,
                            d.id,
                            ContractKey {
                                area: d.order.area.clone(),
                                period: d.order.period,
                            },
                        ));
                    }
                }
            }
        }
        let count = expirables.len();
        for (gp_id, order_id, key) in expirables {
            // Remove from book if resting.
            if self.owner_of(order_id) == Some(gp_id) {
                self.unbind_resting_order(order_id);
                if let Some(book) = self.books.get_mut(&key) {
                    book.cancel(order_id);
                }
            }
            self.gridpools
                .get_mut(gp_id)
                .expect("gridpool present")
                .update_order(order_id, |d| {
                    d.state.state = OrderState::Expired;
                    d.state.reason = StateReason::Delete;
                    d.state.actor = MarketActor::System;
                    d.modification_time = now;
                });
            if let Some(d) = self
                .gridpools
                .get(gp_id)
                .and_then(|g| g.get_order(order_id))
                .cloned()
            {
                self.publish_order_update(gp_id, d);
            }
        }
        count
    }

    /// Modify a resting order's price / quantity / valid_until / tag.
    /// All four are tri-state via Option<…>: None on the OrderUpdate
    /// field leaves the underlying field untouched. Quantity-decrease
    /// and tag-only updates preserve time priority; everything else
    /// cancels-from-book + re-inserts at the new tail. After re-insert
    /// the matcher runs against the new price in case it crosses.
    pub fn modify_order(
        &mut self,
        gridpool_id: GridpoolId,
        order_id: OrderId,
        upd: OrderUpdate,
        now: DateTime<Utc>,
    ) -> Result<OrderDetail, SubmitError> {
        if self.gridpools.get(gridpool_id).is_none() {
            return Err(SubmitError::UnknownGridpool);
        }
        let snapshot = self
            .gridpools
            .get(gridpool_id)
            .and_then(|g| g.get_order(order_id))
            .cloned()
            .ok_or(SubmitError::OrderNotFound)?;
        if snapshot.state.state.is_terminal() {
            return Err(SubmitError::OrderAlreadyTerminal);
        }

        let rules = self
            .markets
            .get(&snapshot.order.area)
            .ok_or(SubmitError::UnknownArea)?;

        let new_price = upd.price.unwrap_or(snapshot.order.price);
        // The "total quantity" the user thinks of equals filled +
        // open at any moment; we let upd.quantity rewrite that total.
        let prior_total = snapshot.filled_quantity + snapshot.open_quantity;
        let new_total = upd.quantity.unwrap_or(prior_total);
        let new_valid_until = upd.valid_until.or(snapshot.order.valid_until);
        let new_tag = upd.tag.clone().or_else(|| snapshot.order.tag.clone());

        if new_price <= Decimal::ZERO {
            return Err(SubmitError::NonPositivePrice);
        }
        if !is_multiple_of(new_price, rules.price_tick) {
            return Err(SubmitError::PriceOffGrid);
        }
        if new_total <= Decimal::ZERO {
            return Err(SubmitError::NonPositiveQuantity);
        }
        if !is_multiple_of(new_total, rules.qty_step) {
            return Err(SubmitError::QuantityOffGrid);
        }
        if new_total < snapshot.filled_quantity {
            return Err(SubmitError::ModifyQuantityBelowFilled);
        }

        // Cancel from the book (if resting). Quantity decrease alone
        // with same price could preserve priority, but the simple
        // cancel + reinsert path is correct everywhere; we accept the
        // priority loss for that case and revisit later.
        let key = ContractKey {
            area: snapshot.order.area.clone(),
            period: snapshot.order.period,
        };
        if self.owner_of(order_id) == Some(gridpool_id) {
            self.unbind_resting_order(order_id);
            if let Some(book) = self.books.get_mut(&key) {
                book.cancel(order_id);
            }
        }

        // Apply field changes to the gridpool index.
        self.gridpools
            .get_mut(gridpool_id)
            .expect("gridpool still present")
            .update_order(order_id, |d| {
                d.order.price = new_price;
                d.order.quantity = new_total;
                d.order.valid_until = new_valid_until;
                d.order.tag = new_tag;
                d.open_quantity = new_total - d.filled_quantity;
                d.modification_time = now;
                d.state.reason = StateReason::Modify;
                d.state.actor = MarketActor::User;
                d.state.state = if d.open_quantity.is_zero() {
                    OrderState::Filled
                } else {
                    OrderState::Active
                };
            });

        // Re-run the matcher against the new price in case it crosses.
        let open = self
            .gridpools
            .get(gridpool_id)
            .and_then(|g| g.get_order(order_id))
            .map(|d| d.open_quantity)
            .unwrap_or(Decimal::ZERO);
        if open > Decimal::ZERO {
            let taker_side = snapshot.order.side;
            let taker_currency = snapshot.order.currency;
            let outcome = self.match_limit_in(
                key.clone(),
                IncomingLimit {
                    id: order_id,
                    side: taker_side,
                    price: new_price,
                    quantity: open,
                },
            );
            for fill in &outcome.fills {
                self.record_fill(
                    fill,
                    order_id,
                    taker_side,
                    taker_currency,
                    Some(gridpool_id),
                    snapshot.order.area.clone(),
                    snapshot.order.period,
                    now,
                );
            }
            // record_fill updates the maker side; the modifying order
            // (the taker here) needs its own filled/open quantity +
            // state to reflect the post-modify fills.
            let total_filled: Decimal = outcome.fills.iter().map(|f| f.quantity).sum();
            if total_filled > Decimal::ZERO {
                self.gridpools
                    .get_mut(gridpool_id)
                    .expect("gridpool present")
                    .update_order(order_id, |d| {
                        d.filled_quantity += total_filled;
                        d.open_quantity -= total_filled;
                        d.modification_time = now;
                        d.state.actor = MarketActor::System;
                        if d.open_quantity.is_zero() {
                            d.state.state = OrderState::Filled;
                            d.state.reason = StateReason::FullExecution;
                        } else {
                            d.state.reason = StateReason::PartialExecution;
                        }
                    });
            }
            if outcome.rested.is_some() {
                self.bind_resting_order(order_id, gridpool_id);
            }
        }

        let detail = self
            .gridpools
            .get(gridpool_id)
            .and_then(|g| g.get_order(order_id))
            .cloned()
            .expect("just updated");
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
                duration: DeliveryDuration::DeliveryDuration60,
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
                duration: DeliveryDuration::DeliveryDuration60,
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
                        duration: DeliveryDuration::DeliveryDuration60,
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
                    execution_option: Some(ExecutionOption::Aon),
                    ..sample_buy(dec!(1.0), dec!(85.0))
                },
                SubmitError::UnsupportedExecutionOption(ExecutionOption::Aon),
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
    fn counterparty_order_rests_then_fills_gridpool_taker() {
        let (mut w, gp) = setup_world_with_pool();
        let mut public_rx = w.subscribe_public_trades();
        let mut gridpool_rx = w.subscribe_gridpool_trades(gp).unwrap();

        // Counterparty posts a sell at 85.0.
        let cp_id = w
            .submit_counterparty_order(sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        // Counterparty resting orders have no gridpool binding.
        assert!(w.owner_of(cp_id).is_none());
        // No fills yet.
        assert!(public_rx.try_recv().is_err());
        assert!(gridpool_rx.try_recv().is_err());

        // Gridpool takes against it.
        let taker = w
            .submit_order(gp, sample_buy(dec!(0.5), dec!(85.0)), t0())
            .unwrap();
        assert_eq!(taker.state.state, OrderState::Filled);
        assert_eq!(taker.filled_quantity, dec!(0.5));

        // Public tape: one event for the fill.
        let pt = public_rx.try_recv().unwrap();
        assert_eq!(pt.quantity, dec!(0.5));
        // Gridpool tape: one event (taker side only — no maker
        // gridpool to credit).
        let gt = gridpool_rx.try_recv().unwrap();
        assert_eq!(gt.order_id, taker.id);
        assert!(gridpool_rx.try_recv().is_err());

        // Counterparty's resting half-MW is still on the book.
        let mut counterparty_cancel = w.cancel_counterparty_order(cp_id);
        assert!(counterparty_cancel);
        // Double-cancel returns false (idempotent).
        counterparty_cancel = w.cancel_counterparty_order(cp_id);
        assert!(!counterparty_cancel);
    }

    #[test]
    fn counterparty_taker_against_resting_gridpool_maker() {
        let (mut w, gp) = setup_world_with_pool();
        let mut public_rx = w.subscribe_public_trades();
        let mut gridpool_rx = w.subscribe_gridpool_trades(gp).unwrap();

        // Gridpool posts a resting buy.
        let maker = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        // discard the order-update publication
        let mut order_rx = w.subscribe_orders(gp).unwrap();
        // (subscribed after the place, so no events backed up).
        let _ = order_rx.try_recv();

        // Counterparty crosses with a sell.
        let _cp_id = w
            .submit_counterparty_order(sample_sell(dec!(0.4), dec!(85.0)), t0())
            .unwrap();

        // Public tape gets the fill.
        let pt = public_rx.try_recv().unwrap();
        assert_eq!(pt.quantity, dec!(0.4));
        // Gridpool tape gets exactly one event (maker side).
        let gt = gridpool_rx.try_recv().unwrap();
        assert_eq!(gt.order_id, maker.id);
        assert!(gridpool_rx.try_recv().is_err());
        // Order update fan-out for the maker.
        let upd = order_rx.try_recv().unwrap();
        assert_eq!(upd.id, maker.id);
        assert_eq!(upd.filled_quantity, dec!(0.4));
        assert_eq!(upd.state.state, OrderState::Active);
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
    fn expire_lapsed_orders_transitions_active_to_expired() {
        let (mut w, gp) = setup_world_with_pool();
        let earlier = t0() + chrono::Duration::seconds(30);
        let order = Order {
            valid_until: Some(t0() + chrono::Duration::seconds(60)),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, order, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Active);

        // Before lapse: no-op.
        let n = w.expire_lapsed_orders(earlier);
        assert_eq!(n, 0);
        assert_eq!(
            w.gridpools().get(gp).unwrap().get_order(d.id).unwrap().state.state,
            OrderState::Active
        );

        // After lapse: expired + removed from the book.
        let later = t0() + chrono::Duration::seconds(120);
        let n = w.expire_lapsed_orders(later);
        assert_eq!(n, 1);
        let post = w.gridpools().get(gp).unwrap().get_order(d.id).unwrap();
        assert_eq!(post.state.state, OrderState::Expired);
        assert_eq!(post.state.actor, MarketActor::System);
        assert!(w.book(&de_lu_hour()).unwrap().is_empty());
    }

    #[test]
    fn fok_with_insufficient_depth_cancels_without_fills() {
        let (mut w, gp) = setup_world_with_pool();
        // Resting sell of 0.5; FOK buy of 1.0 needs full match.
        w.submit_order(gp, sample_sell(dec!(0.5), dec!(85.0)), t0()).unwrap();
        let fok = Order {
            execution_option: Some(ExecutionOption::Fok),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, fok, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Canceled);
        assert_eq!(d.state.reason, StateReason::Reject);
        assert_eq!(d.filled_quantity, dec!(0));
        // Resting 0.5 sell is untouched.
        assert_eq!(
            w.book(&de_lu_hour()).unwrap().best_ask(),
            Some(dec!(85.0))
        );
    }

    #[test]
    fn fok_with_sufficient_depth_fully_fills() {
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0()).unwrap();
        let fok = Order {
            execution_option: Some(ExecutionOption::Fok),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, fok, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Filled);
        assert_eq!(d.filled_quantity, dec!(1.0));
    }

    #[test]
    fn ioc_takes_what_it_can_then_cancels_rest() {
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(0.4), dec!(85.0)), t0()).unwrap();
        let ioc = Order {
            execution_option: Some(ExecutionOption::Ioc),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, ioc, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Canceled);
        assert_eq!(d.state.reason, StateReason::PartialExecution);
        assert_eq!(d.filled_quantity, dec!(0.4));
        // No leftover on the book.
        assert_eq!(w.book(&de_lu_hour()).unwrap().best_bid(), None);
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
