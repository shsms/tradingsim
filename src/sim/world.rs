//! The single owner of mutable sim state. Phase 3 carries just
//! `MarketRegistry` + one `OrderBook` per contract + the monotonic
//! id sequence. Phase 4 adds `GridpoolRegistry`, a clock, and the
//! tick loop; the World stays the integration point for those.

use std::collections::HashMap;

use crate::sim::book::OrderBook;
use crate::sim::gridpool::{Gridpool, GridpoolRegistry};
use crate::sim::market::{Area, DeliveryPeriod, MarketRegistry};
use crate::sim::matching::{LimitMatchOutcome, match_limit};
use crate::sim::matching::IncomingLimit;
use crate::sim::order::{GridpoolId, OrderId};

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
}

impl World {
    pub fn new(markets: MarketRegistry) -> Self {
        Self {
            markets,
            gridpools: GridpoolRegistry::new(),
            books: HashMap::new(),
            order_to_gridpool: HashMap::new(),
            next_order_id: 1,
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
        self.gridpools.insert(gp);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::market::{DeliveryDuration, MarketRules};
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
