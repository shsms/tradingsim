//! The single owner of mutable sim state. Phase 3 carries just
//! `MarketRegistry` + one `OrderBook` per contract + the monotonic
//! id sequence. Phase 4 adds `GridpoolRegistry`, a clock, and the
//! tick loop; the World stays the integration point for those.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::broadcast;

use crate::proto::trading as proto_trading;
use crate::proto_conv::{power_to_proto, price_to_proto, timestamp_to_proto};
use crate::sim::book::OrderBook;
use crate::sim::decimal::is_multiple_of;
use crate::sim::gridpool::{Gridpool, GridpoolRegistry, SelfTradePolicy};
use crate::sim::market::{Area, Currency, DeliveryPeriod, MarketRegistry};
use crate::sim::matching::{ExecMode, IncomingLimit, LimitMatchOutcome, match_limit};
use crate::sim::order::{
    ExecutionOption, GridpoolId, MarketActor, Order, OrderDetail, OrderId, OrderState, OrderType,
    Side, StateDetail, StateReason,
};
use crate::sim::trade::{PublicTrade, Trade, TradeId, TradeState};

/// Cap on the in-memory public-tape history rings. Older entries are
/// evicted FIFO once the cap is reached; replay requests for events
/// older than the oldest retained record return nothing for that
/// segment.
const HISTORY_CAP: usize = 10_000;

/// Per-gridpool order-update fan-out. Capacity is enough to keep the
/// "lagged" failure mode rare under normal load; a stream consumer
/// that genuinely can't keep up still recovers (with a Lagged error
/// that the gRPC stream task swallows).
const ORDER_BROADCAST_CAPACITY: usize = 256;

/// Global public-trade fan-out + per-gridpool trade fan-out share
/// the same capacity. PublicTrade events are emitted globally (one
/// per fill); Trade events are per-gridpool (two per fill, one per
/// side of the match).
const TRADE_BROADCAST_CAPACITY: usize = 8192;

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
    /// SIDC-style couplings between delivery areas. Symmetric:
    /// if A→B is here, B→A is too. The inner `Coupling` carries
    /// the cross-border gate offset and an optional capacity cap
    /// (MWh per contract).
    couplings: HashMap<Area, HashMap<Area, Coupling>>,
    /// Per-(unordered area pair, delivery period) total MWh that
    /// has crossed the edge. Used to enforce per-edge capacity
    /// limits during matching. Pair is stored with the lexically-
    /// smaller area first so (A, B, p) and (B, A, p) hash the same.
    coupling_fills: HashMap<(Area, Area, DeliveryPeriod), Decimal>,
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
    /// Public order book event tape — one record per resting-order
    /// state change (add / update / full-consume). proto-shaped so
    /// the stream task ships records straight through without a
    /// per-item conversion.
    public_book_tx: broadcast::Sender<proto_trading::PublicOrderBookRecord>,
    /// In-memory replay ring for public trades. Drained by stream
    /// handlers asking for a `start_time` in the recent past.
    public_trade_history: parking_lot::Mutex<std::collections::VecDeque<PublicTrade>>,
    /// In-memory replay ring for public order book records.
    public_book_history:
        parking_lot::Mutex<std::collections::VecDeque<proto_trading::PublicOrderBookRecord>>,
    /// First-seen timestamp per resting order id. Populated when a
    /// book entry first appears; cleared on cancel / full-fill.
    /// Used to populate PublicOrderBookRecord.create_time.
    book_first_seen: HashMap<OrderId, DateTime<Utc>>,
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
    /// Flips to true when `(suspend-market)` fires; validate_common
    /// rejects everything until `(resume-market)` flips it back.
    /// Held in an Arc<RwLock<bool>> so the lisp layer and World
    /// share the same flag without going through World every time.
    market_suspended: Arc<parking_lot::RwLock<bool>>,
}

/// One symmetric coupling edge. Cross-border gate plus optional
/// per-contract capacity in MWh.
#[derive(Clone, Copy, Debug)]
pub struct Coupling {
    pub gate_offset: std::time::Duration,
    /// Max MWh that can flow across this edge for a single
    /// contract. `None` = unlimited.
    pub capacity_mw: Option<Decimal>,
}

fn canon_pair(a: &Area, b: &Area) -> (Area, Area) {
    if a.code <= b.code {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
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
    /// Submitted at or after the delivery-period start — the gate is
    /// closed. Trading on a contract ends exactly at its delivery
    /// start time (no pre-delivery buffer).
    GateClosed,
    /// Market is suspended (TSO emergency or scheduled outage). All
    /// submissions reject until `(resume-market)` flips the flag.
    SuspendedMarket,
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
    /// Self-trade prevention: the gridpool's
    /// `SelfTradePolicy::Reject` is in effect and at least one of
    /// the resting orders that the incoming order would consume
    /// belongs to the same gridpool.
    SelfTradeRejected,
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
        let (public_trade_tx, _) = broadcast::channel(TRADE_BROADCAST_CAPACITY);
        let (public_book_tx, _) = broadcast::channel(TRADE_BROADCAST_CAPACITY);
        Self {
            markets,
            gridpools: GridpoolRegistry::new(),
            couplings: HashMap::new(),
            gridpool_order_tx: HashMap::new(),
            gridpool_trade_tx: HashMap::new(),
            public_trade_tx,
            public_book_tx,
            public_trade_history: parking_lot::Mutex::new(
                std::collections::VecDeque::with_capacity(HISTORY_CAP),
            ),
            public_book_history: parking_lot::Mutex::new(
                std::collections::VecDeque::with_capacity(HISTORY_CAP),
            ),
            book_first_seen: HashMap::new(),
            books: HashMap::new(),
            order_to_gridpool: HashMap::new(),
            next_order_id: 1,
            next_trade_id: 1,
            market_suspended: Arc::new(parking_lot::RwLock::new(false)),
            coupling_fills: HashMap::new(),
        }
    }

    /// Shared "market suspended" flag. The lisp layer holds a clone
    /// via Config::market_suspended() and flips it from
    /// `(suspend-market)` / `(resume-market)`.
    pub fn market_suspended_handle(&self) -> Arc<parking_lot::RwLock<bool>> {
        self.market_suspended.clone()
    }

    /// Replace the suspended-flag handle so the World shares state
    /// with the lisp Config. Called once at boot in the bin; before
    /// that, each World holds its own (locally-true) flag.
    pub fn set_market_suspended_handle(&mut self, handle: Arc<parking_lot::RwLock<bool>>) {
        self.market_suspended = handle;
    }

    pub fn is_market_suspended(&self) -> bool {
        *self.market_suspended.read()
    }

    /// Register a symmetric SIDC-style coupling between two
    /// delivery areas. `gate_offset` is the lead time before
    /// delivery at which the coupling closes; pass `Duration::ZERO`
    /// for intra-zone couplings. `capacity_mw` is an optional
    /// per-contract cap in MWh; `None` = unlimited.
    pub fn add_coupling(
        &mut self,
        a: Area,
        b: Area,
        gate_offset: std::time::Duration,
        capacity_mw: Option<Decimal>,
    ) {
        if a == b {
            return;
        }
        let coupling = Coupling {
            gate_offset,
            capacity_mw,
        };
        self.couplings
            .entry(a.clone())
            .or_default()
            .insert(b.clone(), coupling);
        self.couplings.entry(b).or_default().insert(a, coupling);
    }

    /// Coupled areas with the full coupling record for each.
    pub fn coupled_areas(&self, area: &Area) -> Vec<(Area, Coupling)> {
        self.couplings
            .get(area)
            .map(|map| map.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default()
    }

    /// Number of distinct undirected coupling edges. Each edge is
    /// stored twice in the inner maps (A→B + B→A), so half the total
    /// is the unique-pair count.
    pub fn coupling_count(&self) -> usize {
        self.couplings.values().map(|m| m.len()).sum::<usize>() / 2
    }

    /// MWh remaining on the (a, b) edge for the given contract.
    /// Returns `None` when the edge is uncapped.
    pub fn remaining_capacity(
        &self,
        a: &Area,
        b: &Area,
        period: DeliveryPeriod,
    ) -> Option<Decimal> {
        let coupling = self.couplings.get(a)?.get(b)?;
        let cap = coupling.capacity_mw?;
        let (lo, hi) = canon_pair(a, b);
        let used = self
            .coupling_fills
            .get(&(lo, hi, period))
            .copied()
            .unwrap_or(Decimal::ZERO);
        Some((cap - used).max(Decimal::ZERO))
    }

    fn debit_capacity(&mut self, a: &Area, b: &Area, period: DeliveryPeriod, mw: Decimal) {
        let (lo, hi) = canon_pair(a, b);
        *self
            .coupling_fills
            .entry((lo, hi, period))
            .or_insert(Decimal::ZERO) += mw;
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
    pub fn subscribe_orders(
        &self,
        gridpool_id: GridpoolId,
    ) -> Option<broadcast::Receiver<OrderDetail>> {
        self.gridpool_order_tx
            .get(&gridpool_id)
            .map(|tx| tx.subscribe())
    }

    /// Subscribe to a gridpool's private trade tape.
    pub fn subscribe_gridpool_trades(
        &self,
        gridpool_id: GridpoolId,
    ) -> Option<broadcast::Receiver<Trade>> {
        self.gridpool_trade_tx
            .get(&gridpool_id)
            .map(|tx| tx.subscribe())
    }

    /// Subscribe to the global public trade tape.
    pub fn subscribe_public_trades(&self) -> broadcast::Receiver<PublicTrade> {
        self.public_trade_tx.subscribe()
    }

    /// Subscribe to the public order book event tape. Returns
    /// proto-shaped records the stream task can ship through
    /// unchanged.
    pub fn subscribe_public_book(
        &self,
    ) -> broadcast::Receiver<proto_trading::PublicOrderBookRecord> {
        self.public_book_tx.subscribe()
    }

    /// Snapshot the public-trade history ring; used by the stream
    /// handler to replay events from a `start_time` before the
    /// subscription point.
    pub fn public_trade_history(&self) -> Vec<PublicTrade> {
        self.public_trade_history.lock().iter().cloned().collect()
    }

    /// Snapshot the public-book history ring.
    pub fn public_book_history(&self) -> Vec<proto_trading::PublicOrderBookRecord> {
        self.public_book_history.lock().iter().cloned().collect()
    }

    /// Snapshot every resting order on every still-tradeable book
    /// as proto-shaped records. The WS book stream sends this on
    /// connect so a fresh subscriber sees the current state instead
    /// of an empty book until the next live event. Books whose gate
    /// has already closed are skipped.
    pub fn snapshot_books(&self, now: DateTime<Utc>) -> Vec<proto_trading::PublicOrderBookRecord> {
        let mut out = Vec::new();
        for (key, book) in &self.books {
            if key.period.start <= now {
                continue;
            }
            for (order_id, side, price, qty) in book.iter_with_quantity() {
                let create_time = self.book_first_seen.get(&order_id).copied().unwrap_or(now);
                out.push(proto_trading::PublicOrderBookRecord {
                    id: order_id.0,
                    delivery_area: Some((&key.area).into()),
                    delivery_period: Some(key.period.into()),
                    r#type: None,
                    side: side as i32,
                    price: Some(price_to_proto(price, Currency::Eur)),
                    quantity: Some(power_to_proto(qty)),
                    execution_option: None,
                    create_time: Some(timestamp_to_proto(create_time)),
                    update_time: Some(timestamp_to_proto(now)),
                });
            }
        }
        out
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
        {
            let mut hist = self.public_trade_history.lock();
            if hist.len() >= HISTORY_CAP {
                hist.pop_front();
            }
            hist.push_back(trade.clone());
        }
        let _ = self.public_trade_tx.send(trade);
    }

    fn publish_public_book_record(&self, rec: proto_trading::PublicOrderBookRecord) {
        {
            let mut hist = self.public_book_history.lock();
            if hist.len() >= HISTORY_CAP {
                hist.pop_front();
            }
            hist.push_back(rec.clone());
        }
        let _ = self.public_book_tx.send(rec);
    }

    /// Emit a PublicOrderBookRecord for a resting order whose state
    /// just changed. `now` becomes the record's update_time;
    /// create_time is tracked across calls via `book_first_seen`,
    /// inserted on first emission and cleared when qty drops to 0.
    #[allow(clippy::too_many_arguments)]
    fn emit_book_event(
        &mut self,
        order_id: OrderId,
        side: Side,
        area: Area,
        period: DeliveryPeriod,
        price: Decimal,
        open_qty: Decimal,
        currency: Currency,
        now: DateTime<Utc>,
    ) {
        let create_time = *self.book_first_seen.entry(order_id).or_insert(now);
        if open_qty.is_zero() {
            self.book_first_seen.remove(&order_id);
        }
        let rec = proto_trading::PublicOrderBookRecord {
            id: order_id.0,
            delivery_area: Some((&area).into()),
            delivery_period: Some(period.into()),
            r#type: None, // ORDER_TYPE not tracked on the book entry today
            side: side as i32,
            price: Some(price_to_proto(price, currency)),
            quantity: Some(power_to_proto(open_qty)),
            execution_option: None,
            create_time: Some(timestamp_to_proto(create_time)),
            update_time: Some(timestamp_to_proto(now)),
        };
        self.publish_public_book_record(rec);
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

    /// Cross-area variant: sweep the taker's own book plus all
    /// coupled-area books in global price-time priority. Each fill
    /// carries the maker area so the caller can emit a PublicTrade
    /// with the right buy/sell area split. Resting leftover always
    /// lands on the taker's own area book. `currency` is used to
    /// build the proto-shaped book event records.
    #[allow(clippy::too_many_arguments)]
    fn match_limit_across(
        &mut self,
        taker_key: ContractKey,
        mut taker: IncomingLimit,
        mode: ExecMode,
        currency: Currency,
        now: DateTime<Utc>,
    ) -> (
        Vec<(crate::sim::matching::Fill, Area)>,
        Option<crate::sim::book::Resting>,
    ) {
        let keys = self.candidate_keys(&taker_key, now);

        // FOK pre-check across all candidate books.
        if mode == ExecMode::FillOrKill {
            let depth: Decimal = keys
                .iter()
                .filter_map(|k| self.books.get(k))
                .map(|b| b.marketable_depth(taker.side, taker.price))
                .sum();
            if depth < taker.quantity {
                return (Vec::new(), None);
            }
        }

        let mut fills: Vec<(crate::sim::matching::Fill, Area)> = Vec::new();
        loop {
            // Find the (book_key, level_price) with the best
            // marketable price across all candidate books.
            let best = keys
                .iter()
                .filter_map(|k| {
                    self.books
                        .get(k)
                        .and_then(|b| b.peek_opposite(taker.side).map(|p| (k.clone(), p)))
                })
                .filter(|(_, price)| match taker.side {
                    Side::Buy => *price <= taker.price,
                    Side::Sell => *price >= taker.price,
                    _ => false,
                })
                .min_by(|(_, p1), (_, p2)| match taker.side {
                    Side::Buy => p1.cmp(p2),
                    Side::Sell => p2.cmp(p1),
                    _ => std::cmp::Ordering::Equal,
                });
            let Some((book_key, _)) = best else { break };
            let book = self.books.get_mut(&book_key).expect("found above");
            let (price, maker_id, open_before, taken, _fully) = book
                .consume_front(taker.side, taker.quantity)
                .expect("peek said non-empty");
            let maker_open_after = open_before - taken;
            // Cross-area fill: debit the per-edge capacity. The
            // check before the loop already gates further crosses
            // once the edge is exhausted, but one fill may push
            // past the cap (the soft-cap behaviour real SIDC
            // doesn't have but the sim accepts for simplicity).
            if book_key.area != taker_key.area {
                self.debit_capacity(&taker_key.area, &book_key.area, taker_key.period, taken);
            }
            fills.push((
                crate::sim::matching::Fill {
                    taker_id: taker.id,
                    maker_id,
                    price,
                    quantity: taken,
                },
                book_key.area.clone(),
            ));
            // Maker's resting state just changed — emit a book event.
            self.emit_book_event(
                maker_id,
                taker.side.opposite(),
                book_key.area.clone(),
                book_key.period,
                price,
                maker_open_after,
                currency,
                now,
            );
            taker.quantity -= taken;
            if taker.quantity.is_zero() {
                break;
            }
        }

        let rested = if mode == ExecMode::Resting && taker.quantity > Decimal::ZERO {
            let r = crate::sim::book::Resting {
                id: taker.id,
                open_qty: taker.quantity,
            };
            self.book_mut(taker_key.clone())
                .insert(taker.side, taker.price, r);
            self.emit_book_event(
                taker.id,
                taker.side,
                taker_key.area.clone(),
                taker_key.period,
                taker.price,
                taker.quantity,
                currency,
                now,
            );
            Some(r)
        } else {
            None
        };
        (fills, rested)
    }

    pub fn contracts(&self) -> impl Iterator<Item = &ContractKey> {
        self.books.keys()
    }

    /// The contract keys a taker order rooted at `taker_key` is
    /// allowed to match against: the home contract plus every
    /// coupled area whose cross-border gate is still open and
    /// whose per-contract edge capacity isn't already exhausted.
    /// Used by both the matcher and the self-trade pre-flight.
    fn candidate_keys(&self, taker_key: &ContractKey, now: DateTime<Utc>) -> Vec<ContractKey> {
        let mut keys: Vec<ContractKey> = vec![taker_key.clone()];
        for (other, coupling) in self.coupled_areas(&taker_key.area) {
            if coupling.gate_offset > std::time::Duration::ZERO {
                let cutoff = taker_key.period.start
                    - chrono::Duration::from_std(coupling.gate_offset).unwrap_or_default();
                if now >= cutoff {
                    continue;
                }
            }
            if let Some(rem) = self.remaining_capacity(&taker_key.area, &other, taker_key.period)
                && rem <= Decimal::ZERO
            {
                continue;
            }
            keys.push(ContractKey {
                area: other,
                period: taker_key.period,
            });
        }
        keys
    }

    /// Simulate the matcher's marketable walk against the candidate
    /// books (taker's area + currently-open coupled areas) without
    /// mutating any state. Returns `true` if any resting order the
    /// matcher would consume to fill `quantity` belongs to
    /// `gridpool_id`. Conservative: if the same-pool resting sits
    /// deeper than the marketable quantity will reach, this returns
    /// `false`.
    fn would_self_trade(
        &self,
        taker_key: &ContractKey,
        side: Side,
        taker_price: Decimal,
        mut remaining: Decimal,
        gridpool_id: GridpoolId,
        now: DateTime<Utc>,
    ) -> bool {
        let keys = self.candidate_keys(taker_key, now);
        let mut walk: Vec<(OrderId, Decimal, Decimal)> = Vec::new();
        for key in &keys {
            if let Some(b) = self.books.get(key) {
                walk.extend(b.marketable_orders(side, taker_price));
            }
        }
        walk.sort_by(|a, b| match side {
            Side::Buy => a.1.cmp(&b.1),
            Side::Sell => b.1.cmp(&a.1),
            Side::Unspecified => std::cmp::Ordering::Equal,
        });
        for (id, _price, qty) in walk {
            if remaining <= Decimal::ZERO {
                return false;
            }
            if self.owner_of(id) == Some(gridpool_id) {
                return true;
            }
            remaining -= qty;
        }
        false
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

        // 2-4. Shared validation: market, type, grid, gate.
        self.validate_common(&order, now)?;
        // FOK / IOC are honoured starting in Phase 6. AON is still
        // rejected — partial-quantity-or-rest semantics are open.
        if let Some(ExecutionOption::Aon) = order.execution_option {
            return Err(SubmitError::UnsupportedExecutionOption(
                ExecutionOption::Aon,
            ));
        }
        if let Some(opt) = order.execution_option
            && order.valid_until.is_some()
        {
            return Err(SubmitError::UnsupportedExecutionOption(opt));
        }

        // 4b. Self-trade prevention. Only if the pool opted in;
        // default is Allow so existing configs keep their behaviour.
        if matches!(gp.self_trade_policy, SelfTradePolicy::Reject) {
            let probe_key = ContractKey {
                area: order.area.clone(),
                period: order.period,
            };
            if self.would_self_trade(
                &probe_key,
                order.side,
                order.price,
                order.quantity,
                gridpool_id,
                now,
            ) {
                return Err(SubmitError::SelfTradeRejected);
            }
        }

        // 5-7. Admit + match + record fills via the shared core.
        let total_qty = order.quantity;
        let (taker_id, fills_with_areas, rested, mode) =
            self.admit_and_match(&order, Some(gridpool_id), now);
        let outcome = LimitMatchOutcome {
            fills: fills_with_areas.iter().map(|(f, _)| *f).collect(),
            rested,
        };

        // 8. Build the taker's OrderDetail.
        let filled: Decimal = outcome.fills.iter().map(|f| f.quantity).sum();
        let open = total_qty - filled;
        // FOK / IOC outcomes:
        //   FOK + 0 fills = no execution at all → Canceled, Reject.
        //   IOC + filled < total = killed leftover → Canceled if any
        //     leftover; Filled if all matched.
        //   Resting + filled < total but no rest = unreachable for now.
        // (mode, rested, no-fills, fully-filled) → (state, reason).
        let (state, reason) = match (
            mode,
            outcome.rested.is_some(),
            filled.is_zero(),
            open.is_zero(),
        ) {
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
    fn validate_common(&self, order: &Order, now: DateTime<Utc>) -> Result<(), SubmitError> {
        if self.is_market_suspended() {
            return Err(SubmitError::SuspendedMarket);
        }
        let rules = self
            .markets
            .get(&order.area)
            .ok_or(SubmitError::UnknownArea)?;
        if order.period.start <= now {
            return Err(SubmitError::GateClosed);
        }
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
        // Real intraday markets (continuous-trading etc.) admit negative prices
        // under supply gluts. The sim follows suit; only the tick / grid
        // checks remain on the price value itself.
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
    /// `taker_area` is the taker's area; `maker_area` is the area
    /// the matched order rested in (same as taker_area for non-
    /// cross-area fills).
    #[allow(clippy::too_many_arguments)]
    fn record_fill(
        &mut self,
        fill: &crate::sim::matching::Fill,
        taker_id: OrderId,
        taker_side: crate::sim::order::Side,
        taker_currency: crate::sim::market::Currency,
        taker_gridpool: Option<GridpoolId>,
        taker_area: Area,
        maker_area: Area,
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
                area: taker_area.clone(),
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
                area: maker_area.clone(),
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

        // Public tape: one event per fill. buy_area / sell_area
        // resolve from (taker_side, taker_area, maker_area).
        let (buy_area, sell_area) = match taker_side {
            Side::Buy => (taker_area.clone(), maker_area.clone()),
            Side::Sell => (maker_area.clone(), taker_area.clone()),
            Side::Unspecified => (taker_area.clone(), maker_area.clone()),
        };
        self.publish_public_trade(PublicTrade {
            id: trade_id,
            buy_area,
            sell_area,
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
    /// Honors `order.execution_option` (FOK / IOC) the same way
    /// submit_order does — without this, an IOC residual would
    /// rest at the slippage-cap and the next opposite-side fire
    /// would trade against it at the cap repeatedly.
    pub fn submit_counterparty_order(
        &mut self,
        order: Order,
        now: DateTime<Utc>,
    ) -> Result<OrderId, SubmitError> {
        self.validate_common(&order, now)?;
        let (taker_id, _, _, _) = self.admit_and_match(&order, None, now);
        // Counterparty leftovers (in Resting mode) sit on the book
        // without a binding — owner_of stays None, and
        // cancel_counterparty_order is the only way they leave.
        Ok(taker_id)
    }

    /// Common admit + match + record-fills core, called by both
    /// gridpool and counterparty submit paths. The pre-trade
    /// checks (gridpool gate, area allow, self-trade prevention,
    /// AON / valid_until rejection) and the post-trade work
    /// (OrderDetail construction, state machine, pool record,
    /// resting-order binding, order-update fanout) live in the
    /// caller. `taker_gridpool` is the taker's owning pool for
    /// gridpool submissions, or `None` for unbound counterparty
    /// orders — `record_fill` reads it to skip the pool-side
    /// bookkeeping on the counterparty path.
    fn admit_and_match(
        &mut self,
        order: &Order,
        taker_gridpool: Option<GridpoolId>,
        now: DateTime<Utc>,
    ) -> (
        OrderId,
        Vec<(crate::sim::matching::Fill, Area)>,
        Option<crate::sim::book::Resting>,
        ExecMode,
    ) {
        let taker_id = self.next_id();
        let key = ContractKey {
            area: order.area.clone(),
            period: order.period,
        };
        let mode = match order.execution_option {
            Some(ExecutionOption::Fok) => ExecMode::FillOrKill,
            Some(ExecutionOption::Ioc) => ExecMode::ImmediateOrCancel,
            _ => ExecMode::Resting,
        };
        // match_limit_across (not match_limit_in) so the public
        // order-book stream sees ADD records when an order rests,
        // and UPDATE records when fills change a maker's depth.
        let (fills_with_areas, rested) = self.match_limit_across(
            key,
            IncomingLimit {
                id: taker_id,
                side: order.side,
                price: order.price,
                quantity: order.quantity,
            },
            mode,
            order.currency,
            now,
        );
        for (fill, maker_area) in &fills_with_areas {
            self.record_fill(
                fill,
                taker_id,
                order.side,
                order.currency,
                taker_gridpool,
                order.area.clone(),
                maker_area.clone(),
                order.period,
                now,
            );
        }
        (taker_id, fills_with_areas, rested, mode)
    }

    /// Remove a resting counterparty order from the book. No state
    /// flip (counterparty orders don't have an OrderDetail). Returns
    /// true if the id was on the book, false otherwise.
    pub fn cancel_counterparty_order(&mut self, order_id: OrderId) -> bool {
        let mut found_meta: Option<(Side, Decimal, ContractKey)> = None;
        for (key, book) in self.books.iter_mut() {
            if let Some((side, price)) = book.cancel_with_meta(order_id) {
                found_meta = Some((side, price, key.clone()));
                break;
            }
        }
        // Belt-and-suspenders: an accidental binding (shouldn't
        // happen, but cheap) gets cleared.
        self.unbind_resting_order(order_id);
        if let Some((side, price, key)) = found_meta {
            // Counterparty orders are EUR-only (the only currency
            // the MM defaults emit).
            self.emit_book_event(
                order_id,
                side,
                key.area,
                key.period,
                price,
                Decimal::ZERO,
                Currency::Eur,
                Utc::now(),
            );
            true
        } else {
            false
        }
    }

    /// Walk every gridpool's order index and transition any
    /// non-terminal order whose `valid_until` has lapsed (or whose
    /// delivery gate has closed — period.start <= now) to
    /// `OrderState::Expired`. Counterparty orders resting on the
    /// book for gate-closed contracts are removed alongside, with
    /// qty=0 events emitted on the public book stream. Returns the
    /// count of orders touched so the caller can log activity.
    pub fn expire_lapsed_orders(&mut self, now: DateTime<Utc>) -> usize {
        let mut expirables: Vec<(GridpoolId, OrderId, ContractKey)> = Vec::new();
        for gp in self.gridpools.iter() {
            for d in gp.orders() {
                if d.state.state.is_terminal() {
                    continue;
                }
                let valid_until_lapsed = d.order.valid_until.map(|vu| vu <= now).unwrap_or(false);
                let gate_closed = d.order.period.start <= now;
                if valid_until_lapsed || gate_closed {
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
        let count = expirables.len();
        for (gp_id, order_id, key) in expirables {
            // Remove from book if resting + remember the side/price
            // so we can emit a qty=0 PublicOrderBookRecord.
            let book_meta = if self.owner_of(order_id) == Some(gp_id) {
                self.unbind_resting_order(order_id);
                self.books
                    .get_mut(&key)
                    .and_then(|b| b.cancel_with_meta(order_id))
            } else {
                None
            };
            // Snapshot currency for the book event before the closure.
            let currency = self
                .gridpools
                .get(gp_id)
                .and_then(|g| g.get_order(order_id))
                .map(|d| d.order.currency)
                .unwrap_or(Currency::Eur);
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
            if let Some((side, price)) = book_meta {
                self.emit_book_event(
                    order_id,
                    side,
                    key.area.clone(),
                    key.period,
                    price,
                    Decimal::ZERO,
                    currency,
                    now,
                );
            }
        }

        // Counterparty orders aren't on any gridpool, so the loop
        // above misses them. Sweep every book whose contract has
        // gated out and cancel each resting order on it; emit
        // qty=0 events so the public book stream tells subscribers
        // those entries are gone.
        type CpEntries = Vec<(ContractKey, Vec<(OrderId, Side, Decimal)>)>;
        let cp_targets: CpEntries = self
            .books
            .iter()
            .filter(|(k, _)| k.period.start <= now)
            .map(|(k, b)| (k.clone(), b.iter_with_meta()))
            .filter(|(_, entries)| !entries.is_empty())
            .collect();
        let mut cp_count = 0;
        for (key, entries) in cp_targets {
            for (order_id, side, price) in entries {
                // owner_of None == counterparty; gridpool-owned rests
                // were already handled above.
                if self.owner_of(order_id).is_some() {
                    continue;
                }
                if let Some(book) = self.books.get_mut(&key) {
                    book.cancel_with_meta(order_id);
                }
                self.emit_book_event(
                    order_id,
                    side,
                    key.area.clone(),
                    key.period,
                    price,
                    Decimal::ZERO,
                    Currency::Eur,
                    now,
                );
                cp_count += 1;
            }
        }
        count + cp_count
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
                // Modify's re-match runs single-area for now; cross-
                // area on modify is a nice-to-have.
                self.record_fill(
                    fill,
                    order_id,
                    taker_side,
                    taker_currency,
                    Some(gridpool_id),
                    snapshot.order.area.clone(),
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
        self.cancel_order_with_actor(gridpool_id, order_id, now, MarketActor::User)
    }

    /// Force-cancel by id without needing to know the gridpool, with
    /// `actor = System` to signal it's a TSO recall rather than a
    /// user-initiated cancel. No-op (`OrderNotFound`) when the id
    /// isn't owned by any gridpool — counterparty rests use the
    /// counterparty cancel path.
    pub fn recall_order(
        &mut self,
        order_id: OrderId,
        now: DateTime<Utc>,
    ) -> Result<OrderDetail, SubmitError> {
        // owner_of finds the resting binding; once an order is
        // terminal the binding is gone, so fall back to iterating
        // gridpools to surface OrderAlreadyTerminal cleanly.
        let gp_id = self
            .owner_of(order_id)
            .or_else(|| {
                self.gridpools
                    .iter()
                    .find(|gp| gp.get_order(order_id).is_some())
                    .map(|gp| gp.id)
            })
            .ok_or(SubmitError::OrderNotFound)?;
        self.cancel_order_with_actor(gp_id, order_id, now, MarketActor::System)
    }

    fn cancel_order_with_actor(
        &mut self,
        gridpool_id: GridpoolId,
        order_id: OrderId,
        now: DateTime<Utc>,
        actor: MarketActor,
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
        let mut cancelled_meta: Option<(Side, Decimal, ContractKey, Currency)> = None;
        if self.owner_of(order_id) == Some(gridpool_id) {
            self.unbind_resting_order(order_id);
            // Snapshot enough to emit a DELETE book event after the
            // cancel commits.
            let detail = self
                .gridpools
                .get(gridpool_id)
                .and_then(|g| g.get_order(order_id))
                .cloned();
            for (key, book) in self.books.iter_mut() {
                if let Some((side, price)) = book.cancel_with_meta(order_id) {
                    if let Some(d) = detail {
                        cancelled_meta = Some((side, price, key.clone(), d.order.currency));
                    }
                    break;
                }
            }
        }
        let gp = self.gridpools.get_mut(gridpool_id).unwrap();
        gp.update_order(order_id, |d| {
            d.state.state = OrderState::Canceled;
            d.state.reason = StateReason::Delete;
            d.state.actor = actor;
            d.modification_time = now;
        });
        let detail = gp.get_order(order_id).cloned().unwrap();
        self.publish_order_update(gridpool_id, detail.clone());
        if let Some((side, price, key, currency)) = cancelled_meta {
            self.emit_book_event(
                order_id,
                side,
                key.area,
                key.period,
                price,
                Decimal::ZERO,
                currency,
                now,
            );
        }
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

    fn test_hour() -> ContractKey {
        ContractKey {
            area: Area::eic("10YDE-EON------1"),
            period: DeliveryPeriod {
                start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                duration: DeliveryDuration::DeliveryDuration15,
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
        let k = test_hour();
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
        let area = Area::eic("10YDE-EON------1");
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
        // Tests built on this fixture exercise self-crossing within
        // a single pool (buy + sell from gp #1 filling each other).
        // The runtime default flipped to Reject, so opt back into
        // Allow here to keep those tests exercising the matcher
        // mechanics they were written for.
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        let mut w = World::new(markets);
        let area = Area::eic("10YDE-EON------1");
        w.register_gridpool(
            Gridpool::new(GridpoolId(1), "test", vec![area])
                .with_self_trade_policy(crate::sim::gridpool::SelfTradePolicy::Allow),
        );
        (w, GridpoolId(1))
    }

    fn sample_buy(qty: Decimal, price: Decimal) -> Order {
        Order::limit(
            Area::eic("10YDE-EON------1"),
            DeliveryPeriod {
                start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                duration: DeliveryDuration::DeliveryDuration15,
            },
            Side::Buy,
            price,
            qty,
            Currency::Eur,
        )
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
        let d = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        assert_eq!(d.state.state, OrderState::Active);
        assert_eq!(d.state.reason, StateReason::Add);
        assert_eq!(d.open_quantity, dec!(1.0));
        assert_eq!(d.filled_quantity, dec!(0));
        assert_eq!(w.owner_of(d.id), Some(gp));
        assert_eq!(w.book(&test_hour()).unwrap().best_bid(), Some(dec!(85.0)));
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
        assert!(w.book(&test_hour()).unwrap().is_empty());
        assert!(w.owner_of(buy.id).is_none());
    }

    #[test]
    fn submit_partial_cross_leaves_taker_active() {
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(0.5), dec!(85.0)), t0())
            .unwrap();
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
            (
                sample_buy(dec!(1.0), dec!(85.005)),
                SubmitError::PriceOffGrid,
            ),
            (
                sample_buy(dec!(1.05), dec!(85.0)),
                SubmitError::QuantityOffGrid,
            ),
            (
                sample_buy(dec!(0), dec!(85.0)),
                SubmitError::NonPositiveQuantity,
            ),
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
                        // 12:07 isn't on a 15-min grid.
                        start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 7, 0).unwrap(),
                        duration: DeliveryDuration::DeliveryDuration15,
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
    fn submit_rejects_orders_at_or_after_delivery_start() {
        let (mut w, gp) = setup_world_with_pool();
        let order = sample_buy(dec!(1.0), dec!(85.0));
        let delivery_start = order.period.start;

        // Exactly at gate: rejected.
        let err = w
            .submit_order(gp, order.clone(), delivery_start)
            .unwrap_err();
        assert_eq!(err, SubmitError::GateClosed);

        // After gate: still rejected.
        let later = delivery_start + chrono::Duration::seconds(1);
        let err = w.submit_order(gp, order.clone(), later).unwrap_err();
        assert_eq!(err, SubmitError::GateClosed);

        // One second before gate: accepted.
        let just_in_time = delivery_start - chrono::Duration::seconds(1);
        let detail = w.submit_order(gp, order, just_in_time).unwrap();
        assert_eq!(detail.state.state, OrderState::Active);
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
    fn suspended_market_rejects_submissions() {
        let (mut w, gp) = setup_world_with_pool();
        *w.market_suspended_handle().write() = true;
        let err = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap_err();
        assert_eq!(err, SubmitError::SuspendedMarket);
        // Resume and confirm submissions go through again.
        *w.market_suspended_handle().write() = false;
        let detail = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        assert_eq!(detail.state.state, OrderState::Active);
    }

    #[test]
    fn recall_order_cancels_with_system_actor() {
        let (mut w, gp) = setup_world_with_pool();
        let placed = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        // Order is resting; recall it.
        let recalled = w.recall_order(placed.id, t0()).unwrap();
        assert_eq!(recalled.state.state, OrderState::Canceled);
        assert_eq!(recalled.state.reason, StateReason::Delete);
        assert_eq!(recalled.state.actor, MarketActor::System);
        // Double-recall should hit OrderAlreadyTerminal.
        let err = w.recall_order(placed.id, t0()).unwrap_err();
        assert_eq!(err, SubmitError::OrderAlreadyTerminal);
        // Recall for an unknown id is OrderNotFound.
        let err = w
            .recall_order(crate::sim::order::OrderId(9999), t0())
            .unwrap_err();
        assert_eq!(err, SubmitError::OrderNotFound);
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
            w.gridpools()
                .get(gp)
                .unwrap()
                .get_order(d.id)
                .unwrap()
                .state
                .state,
            OrderState::Active
        );

        // After lapse: expired + removed from the book.
        let later = t0() + chrono::Duration::seconds(120);
        let n = w.expire_lapsed_orders(later);
        assert_eq!(n, 1);
        let post = w.gridpools().get(gp).unwrap().get_order(d.id).unwrap();
        assert_eq!(post.state.state, OrderState::Expired);
        assert_eq!(post.state.actor, MarketActor::System);
        assert!(w.book(&test_hour()).unwrap().is_empty());
    }

    #[test]
    fn expire_lapsed_orders_sweeps_gate_closed_rests() {
        let (mut w, gp) = setup_world_with_pool();
        let mut book_rx = w.subscribe_public_book();

        // Gridpool rest + counterparty rest on the same contract.
        let gp_rest = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let cp_rest = w
            .submit_counterparty_order(sample_sell(dec!(0.5), dec!(86.0)), t0())
            .unwrap();
        // Drain the ADD events.
        while book_rx.try_recv().is_ok() {}
        assert_eq!(w.book(&test_hour()).unwrap().len(), 2);

        // Cross past the delivery start — gate is closed.
        let after_gate =
            sample_buy(dec!(1.0), dec!(85.0)).period.start + chrono::Duration::seconds(1);
        let n = w.expire_lapsed_orders(after_gate);
        assert_eq!(n, 2);

        // Both rests gone; gridpool order flipped to Expired.
        assert!(w.book(&test_hour()).unwrap().is_empty());
        let post = w
            .gridpools()
            .get(gp)
            .unwrap()
            .get_order(gp_rest.id)
            .unwrap();
        assert_eq!(post.state.state, OrderState::Expired);
        // Counterparty book sweep emits qty=0 — a subsequent
        // explicit cancel is a no-op.
        assert!(!w.cancel_counterparty_order(cp_rest));

        // Two qty=0 events on the public-book stream (one per rest).
        let mut zero_events = 0;
        while let Ok(rec) = book_rx.try_recv() {
            let qty = rec
                .quantity
                .as_ref()
                .and_then(|q| q.mw.as_ref())
                .map(|m| m.value.as_str())
                .unwrap_or("");
            if qty == "0" {
                zero_events += 1;
            }
        }
        assert_eq!(zero_events, 2);
    }

    #[test]
    fn cross_area_match_emits_split_public_trade() {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        markets.insert(MarketRules::for_area(
            Area::eic("10YFR-RTE------C"),
            Currency::Eur,
        ));
        let mut w = World::new(markets);
        let de = Area::eic("10YDE-EON------1");
        let fr = Area::eic("10YFR-RTE------C");
        w.add_coupling(de.clone(), fr.clone(), std::time::Duration::ZERO, None);
        w.register_gridpool(Gridpool::new(GridpoolId(1), "de", vec![de.clone()]));
        w.register_gridpool(Gridpool::new(GridpoolId(2), "fr", vec![fr.clone()]));

        let mut public_rx = w.subscribe_public_trades();

        // FR gridpool rests a sell at 84.0 — cheaper than DE side.
        let fr_sell = Order {
            area: fr.clone(),
            ..sample_sell(dec!(1.0), dec!(84.0))
        };
        w.submit_order(GridpoolId(2), fr_sell, t0()).unwrap();
        // DE gridpool's buy at 85.0 should match the FR side.
        let de_buy = sample_buy(dec!(1.0), dec!(85.0));
        let d = w.submit_order(GridpoolId(1), de_buy, t0()).unwrap();

        assert_eq!(d.state.state, OrderState::Filled);
        assert_eq!(d.filled_quantity, dec!(1.0));

        let pt = public_rx.try_recv().unwrap();
        assert_eq!(pt.buy_area, de);
        assert_eq!(pt.sell_area, fr);
        assert_eq!(pt.price, dec!(84.0));
    }

    #[test]
    fn cross_border_gate_blocks_late_cross_match_but_not_early() {
        // Same setup as cross_area_match_emits_split_public_trade
        // but the coupling carries a 60-minute gate offset.
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        markets.insert(MarketRules::for_area(
            Area::eic("10YFR-RTE------C"),
            Currency::Eur,
        ));
        let mut w = World::new(markets);
        let de = Area::eic("10YDE-EON------1");
        let fr = Area::eic("10YFR-RTE------C");
        w.add_coupling(
            de.clone(),
            fr.clone(),
            std::time::Duration::from_secs(3600),
            None,
        );
        w.register_gridpool(Gridpool::new(GridpoolId(1), "de", vec![de.clone()]));
        w.register_gridpool(Gridpool::new(GridpoolId(2), "fr", vec![fr.clone()]));

        let de_buy = sample_buy(dec!(1.0), dec!(85.0));
        let period_start = de_buy.period.start;

        // FR gridpool rests a sell at 84.0 (cheap; should be the
        // best price the German bid can hit if cross-border is open).
        let fr_sell = Order {
            area: fr.clone(),
            ..sample_sell(dec!(1.0), dec!(84.0))
        };
        w.submit_order(GridpoolId(2), fr_sell, t0()).unwrap();

        // Late submission: now is 30 min before delivery — INSIDE
        // the 60-min cross-border gate. The matcher should skip the
        // FR book and the German bid should rest.
        let late = period_start - chrono::Duration::minutes(30);
        let de_buy_late = de_buy.clone();
        let late_detail = w.submit_order(GridpoolId(1), de_buy_late, late).unwrap();
        assert_eq!(late_detail.state.state, OrderState::Active);
        assert_eq!(late_detail.filled_quantity, dec!(0));

        // Early submission: now is 90 min before delivery — OUTSIDE
        // the gate. The buyer should now cross to the FR sell.
        let early = period_start - chrono::Duration::minutes(90);
        let early_detail = w.submit_order(GridpoolId(1), de_buy, early).unwrap();
        assert_eq!(early_detail.state.state, OrderState::Filled);
        assert_eq!(early_detail.filled_quantity, dec!(1.0));
    }

    #[test]
    fn cross_border_capacity_caps_cross_match_volume() {
        // DE+FR with the coupling capped at 0.5 MW per contract.
        // After ~0.5 MW has crossed, further DE buys can't reach
        // the FR sell and rest in DE instead.
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        markets.insert(MarketRules::for_area(
            Area::eic("10YFR-RTE------C"),
            Currency::Eur,
        ));
        let mut w = World::new(markets);
        let de = Area::eic("10YDE-EON------1");
        let fr = Area::eic("10YFR-RTE------C");
        w.add_coupling(
            de.clone(),
            fr.clone(),
            std::time::Duration::ZERO,
            Some(dec!(0.5)),
        );
        w.register_gridpool(Gridpool::new(GridpoolId(1), "de", vec![de.clone()]));
        w.register_gridpool(Gridpool::new(GridpoolId(2), "fr", vec![fr.clone()]));

        // FR posts a fat 2 MW resting sell @ 84.
        let fr_sell = Order {
            area: fr.clone(),
            quantity: dec!(2.0),
            ..sample_sell(dec!(1.0), dec!(84.0))
        };
        w.submit_order(GridpoolId(2), fr_sell, t0()).unwrap();

        // First DE buy of 0.4 MW @ 85 crosses to FR (within cap).
        let buy1 = Order {
            quantity: dec!(0.4),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        assert_eq!(
            w.submit_order(GridpoolId(1), buy1, t0())
                .unwrap()
                .filled_quantity,
            dec!(0.4)
        );

        // Second DE buy. Edge is at 0.4/0.5 — one more soft-cap
        // fill is allowed; capacity ends overshot at 0.8 MW used.
        let buy2 = Order {
            quantity: dec!(0.4),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        assert_eq!(
            w.submit_order(GridpoolId(1), buy2, t0())
                .unwrap()
                .filled_quantity,
            dec!(0.4)
        );

        // Third DE buy: edge exhausted (0.8 > 0.5). With no DE-side
        // ask on the book, the buy rests.
        let buy3 = Order {
            quantity: dec!(0.4),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d3 = w.submit_order(GridpoolId(1), buy3, t0()).unwrap();
        assert_eq!(d3.state.state, OrderState::Active);
        assert_eq!(d3.filled_quantity, dec!(0));
    }

    #[test]
    fn fok_with_insufficient_depth_cancels_without_fills() {
        let (mut w, gp) = setup_world_with_pool();
        // Resting sell of 0.5; FOK buy of 1.0 needs full match.
        w.submit_order(gp, sample_sell(dec!(0.5), dec!(85.0)), t0())
            .unwrap();
        let fok = Order {
            execution_option: Some(ExecutionOption::Fok),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, fok, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Canceled);
        assert_eq!(d.state.reason, StateReason::Reject);
        assert_eq!(d.filled_quantity, dec!(0));
        // Resting 0.5 sell is untouched.
        assert_eq!(w.book(&test_hour()).unwrap().best_ask(), Some(dec!(85.0)));
    }

    #[test]
    fn fok_with_sufficient_depth_fully_fills() {
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
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
        w.submit_order(gp, sample_sell(dec!(0.4), dec!(85.0)), t0())
            .unwrap();
        let ioc = Order {
            execution_option: Some(ExecutionOption::Ioc),
            ..sample_buy(dec!(1.0), dec!(85.0))
        };
        let d = w.submit_order(gp, ioc, t0()).unwrap();
        assert_eq!(d.state.state, OrderState::Canceled);
        assert_eq!(d.state.reason, StateReason::PartialExecution);
        assert_eq!(d.filled_quantity, dec!(0.4));
        // No leftover on the book.
        assert_eq!(w.book(&test_hour()).unwrap().best_bid(), None);
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
        assert!(w.book(&test_hour()).unwrap().is_empty());
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
        markets.insert(MarketRules::default_for_tests());
        let mut w = World::new(markets);

        let k = test_hour();
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

    fn setup_world_with_reject_pool() -> (World, GridpoolId) {
        let mut markets = MarketRegistry::new();
        markets.insert(MarketRules::default_for_tests());
        let mut w = World::new(markets);
        let area = Area::eic("10YDE-EON------1");
        w.register_gridpool(
            Gridpool::new(GridpoolId(1), "reject-pool", vec![area])
                .with_self_trade_policy(SelfTradePolicy::Reject),
        );
        (w, GridpoolId(1))
    }

    #[test]
    fn self_trade_rejected_when_policy_says_so() {
        let (mut w, gp) = setup_world_with_reject_pool();
        // Rest a sell from the pool first.
        w.submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        // Same pool tries to buy through it — must reject.
        let err = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(86.0)), t0())
            .unwrap_err();
        assert_eq!(err, SubmitError::SelfTradeRejected);
        // Resting sell still on the book; no trades recorded.
        assert_eq!(w.book(&test_hour()).unwrap().best_ask(), Some(dec!(85.0)));
        assert!(w.gridpools().get(gp).unwrap().trades().is_empty());
    }

    #[test]
    fn allow_policy_still_self_trades() {
        // The default Allow policy keeps prior behaviour: a pool can
        // cross its own order. This is what setup_world_with_pool
        // builds; the test mirrors submit_cross_match_fills_both_sides
        // but is explicit about the policy.
        let (mut w, gp) = setup_world_with_pool();
        w.submit_order(gp, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        let buy = w
            .submit_order(gp, sample_buy(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        assert_eq!(buy.state.state, OrderState::Filled);
    }

    #[test]
    fn reject_only_fires_when_same_pool_is_in_the_marketable_path() {
        // Pool A is the reject pool; pool B is an unrelated pool
        // also tradeable in the same area. A's deep ask at 88 sits
        // behind B's tighter ask at 85 — A's 1 MW buy at 86 should
        // consume only B's level, never touch A's, and therefore
        // not trigger the self-trade reject.
        let (mut w, _gp_a) = setup_world_with_reject_pool();
        let area = Area::eic("10YDE-EON------1");
        w.register_gridpool(Gridpool::new(GridpoolId(2), "other", vec![area]));
        let gp_a = GridpoolId(1);
        let gp_b = GridpoolId(2);

        // B's tight ask: at 85, 1 MW.
        w.submit_order(gp_b, sample_sell(dec!(1.0), dec!(85.0)), t0())
            .unwrap();
        // A's deeper ask: at 88, 1 MW. Above A's prospective bid
        // (86), so not marketable to A's buyer.
        w.submit_order(gp_a, sample_sell(dec!(1.0), dec!(88.0)), t0())
            .unwrap();

        let buy = w
            .submit_order(gp_a, sample_buy(dec!(1.0), dec!(86.0)), t0())
            .unwrap();
        assert_eq!(buy.state.state, OrderState::Filled);
        assert_eq!(buy.filled_quantity, dec!(1.0));
    }
}
