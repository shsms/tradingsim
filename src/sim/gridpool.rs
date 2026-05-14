//! Gridpool registry: the per-portfolio order + trade index. One
//! Gridpool is the unit a CreateGridpoolOrder request targets; its
//! `areas` set is what pre-trade validation checks against.
//!
//! Lifecycle of an OrderDetail inside a gridpool:
//!   - `record_order` admits a freshly-built detail (PENDING/ACTIVE).
//!   - `update_order` mutates an existing entry in place (state, fills,
//!     modification_time). Returns false if the id isn't in the pool.
//!   - `remove_order` is for hot-reset only — the cancel path leaves
//!     the detail in place with state CANCELED so the order is still
//!     queryable post-cancel, mirroring real-exchange behaviour.

use std::collections::HashMap;

use crate::sim::market::Area;
use crate::sim::order::{GridpoolId, OrderDetail, OrderId};
use crate::sim::trade::Trade;

/// What the matcher does when an incoming aggressive order would
/// cross a resting order owned by the same gridpool.
///
/// - `Reject` (default): pre-trade check rejects the incoming
///   order before any state mutates; the gRPC layer maps this
///   to `FailedPrecondition`. Mirrors the simplest STPF
///   variant — any prospective self-trade kills the new order.
///   Sensible default for a single-trader sim: avoids the
///   surprise of e.g. a buy at 50 filling against your own
///   sell at 50 just because both rested on the same pool.
/// - `Allow`: the order matches normally; whoever's on the
///   other side, including yourself, is a counterparty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelfTradePolicy {
    #[default]
    Reject,
    Allow,
}

#[derive(Clone, Debug)]
pub struct Gridpool {
    pub id: GridpoolId,
    pub name: String,
    pub areas: Vec<Area>,
    pub self_trade_policy: SelfTradePolicy,
    orders: HashMap<OrderId, OrderDetail>,
    trades: Vec<Trade>,
}

impl Gridpool {
    pub fn new(id: GridpoolId, name: impl Into<String>, areas: Vec<Area>) -> Self {
        Self {
            id,
            name: name.into(),
            areas,
            self_trade_policy: SelfTradePolicy::Reject,
            orders: HashMap::new(),
            trades: Vec::new(),
        }
    }

    pub fn with_self_trade_policy(mut self, policy: SelfTradePolicy) -> Self {
        self.self_trade_policy = policy;
        self
    }

    /// True iff `area` is one of the gridpool's allowed delivery
    /// areas. Pre-trade validation rejects orders whose area falls
    /// outside this set.
    pub fn allows_area(&self, area: &Area) -> bool {
        self.areas.iter().any(|a| a == area)
    }

    /// Admit a freshly-built order detail. Returns false if the id
    /// is already present (a bug in the admit path — World mints
    /// unique ids).
    pub fn record_order(&mut self, detail: OrderDetail) -> bool {
        if self.orders.contains_key(&detail.id) {
            return false;
        }
        self.orders.insert(detail.id, detail);
        true
    }

    /// Apply an in-place mutation to an existing order. Returns
    /// false if the id isn't on the gridpool (caller should treat
    /// that as NOT_FOUND at the gRPC layer).
    pub fn update_order<F>(&mut self, id: OrderId, f: F) -> bool
    where
        F: FnOnce(&mut OrderDetail),
    {
        match self.orders.get_mut(&id) {
            Some(d) => {
                f(d);
                true
            }
            None => false,
        }
    }

    pub fn get_order(&self, id: OrderId) -> Option<&OrderDetail> {
        self.orders.get(&id)
    }

    pub fn orders(&self) -> impl Iterator<Item = &OrderDetail> {
        self.orders.values()
    }

    pub fn record_trade(&mut self, trade: Trade) {
        self.trades.push(trade);
    }

    pub fn trades(&self) -> &[Trade] {
        &self.trades
    }
}

/// id -> Gridpool. Held by World; configured at startup via lisp
/// (Phase 5). For Phase 4 the binary admits a single hard-coded
/// gridpool so tsctl has something to talk to.
#[derive(Default, Debug)]
pub struct GridpoolRegistry {
    by_id: HashMap<GridpoolId, Gridpool>,
}

impl GridpoolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, pool: Gridpool) {
        self.by_id.insert(pool.id, pool);
    }

    pub fn get(&self, id: GridpoolId) -> Option<&Gridpool> {
        self.by_id.get(&id)
    }

    pub fn get_mut(&mut self, id: GridpoolId) -> Option<&mut Gridpool> {
        self.by_id.get_mut(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Gridpool> {
        self.by_id.values()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::market::{Currency, DeliveryDuration, DeliveryPeriod};
    use crate::sim::order::{
        MarketActor, Order, OrderState, OrderType, Side, StateDetail, StateReason,
    };
    use chrono::{TimeZone, Utc};
    use rust_decimal::dec;

    fn sample_detail(id: u64, state: OrderState) -> OrderDetail {
        OrderDetail {
            id: OrderId(id),
            order: Order {
                area: Area::eic("10YDE-EON------1"),
                period: DeliveryPeriod {
                    start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                    duration: DeliveryDuration::DeliveryDuration15,
                },
                order_type: OrderType::Limit,
                side: Side::Buy,
                price: dec!(85.0),
                currency: Currency::Eur,
                quantity: dec!(1.0),
                stop_price: None,
                peak_price_delta: None,
                display_quantity: None,
                execution_option: None,
                valid_until: None,
                payload: None,
                tag: None,
            },
            state: StateDetail {
                state,
                reason: StateReason::Add,
                actor: MarketActor::User,
            },
            open_quantity: dec!(1.0),
            filled_quantity: dec!(0),
            create_time: Utc.with_ymd_and_hms(2026, 5, 13, 8, 0, 0).unwrap(),
            modification_time: Utc.with_ymd_and_hms(2026, 5, 13, 8, 0, 0).unwrap(),
        }
    }

    #[test]
    fn record_then_get() {
        let mut gp = Gridpool::new(GridpoolId(1), "test", vec![Area::eic("10YDE-EON------1")]);
        assert!(gp.record_order(sample_detail(7, OrderState::Active)));
        assert!(gp.get_order(OrderId(7)).is_some());
        assert!(gp.get_order(OrderId(99)).is_none());
    }

    #[test]
    fn duplicate_admit_rejected() {
        let mut gp = Gridpool::new(GridpoolId(1), "test", vec![]);
        assert!(gp.record_order(sample_detail(7, OrderState::Active)));
        assert!(!gp.record_order(sample_detail(7, OrderState::Filled)));
    }

    #[test]
    fn update_in_place() {
        let mut gp = Gridpool::new(GridpoolId(1), "test", vec![]);
        gp.record_order(sample_detail(7, OrderState::Active));
        let updated = gp.update_order(OrderId(7), |d| {
            d.state.state = OrderState::Canceled;
            d.state.reason = StateReason::Delete;
        });
        assert!(updated);
        assert_eq!(
            gp.get_order(OrderId(7)).unwrap().state.state,
            OrderState::Canceled
        );
    }

    #[test]
    fn update_missing_returns_false() {
        let mut gp = Gridpool::new(GridpoolId(1), "test", vec![]);
        assert!(!gp.update_order(OrderId(7), |_| {}));
    }

    #[test]
    fn allows_area_membership() {
        let de = Area::eic("10YDE-EON------1");
        let fr = Area::eic("10YFR-RTE------C");
        let gp = Gridpool::new(GridpoolId(1), "test", vec![de.clone()]);
        assert!(gp.allows_area(&de));
        assert!(!gp.allows_area(&fr));
    }

    #[test]
    fn registry_round_trip() {
        let mut reg = GridpoolRegistry::new();
        reg.insert(Gridpool::new(GridpoolId(1), "a", vec![]));
        reg.insert(Gridpool::new(GridpoolId(2), "b", vec![]));
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get(GridpoolId(1)).unwrap().name, "a");
        assert!(reg.get(GridpoolId(99)).is_none());
    }
}
