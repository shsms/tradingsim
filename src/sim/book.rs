//! One OrderBook per contract = (delivery area, delivery period).
//! Resting orders are stored in price-keyed FIFO queues per side, so
//! price-time priority is the data structure's natural traversal
//! order. The matcher is in `sim::matching` — the book exposes the
//! mutation primitives it needs and nothing more.
//!
//! "Resting" here is the slim view a book entry needs for matching
//! (id, open quantity); the full Order / OrderDetail lives in the
//! gridpool index that the server layer maintains.

use std::collections::{BTreeMap, HashMap, VecDeque};

use rust_decimal::Decimal;

use crate::sim::order::{OrderId, Side};

/// What sits on the book: the bare minimum the matcher reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resting {
    pub id: OrderId,
    pub open_qty: Decimal,
}

/// Continuous-trading order book for one contract.
///
/// Invariants (upheld by every mutator):
/// - For every id in `by_id`, exactly one queue holds a `Resting`
///   with that id at the matching (side, price) level.
/// - No level holds an empty queue.
/// - Iteration order at a single level matches insertion order.
#[derive(Default, Debug)]
pub struct OrderBook {
    bids: BTreeMap<Decimal, VecDeque<Resting>>,
    asks: BTreeMap<Decimal, VecDeque<Resting>>,
    /// Reverse index for cancel / modify lookups: id -> (side, price).
    by_id: HashMap<OrderId, (Side, Decimal)>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push an order onto the tail of its (side, price) FIFO queue.
    /// Returns false if the id was already present (no-op then; the
    /// caller is expected to have cancelled the previous instance
    /// for a modify).
    pub fn insert(&mut self, side: Side, price: Decimal, resting: Resting) -> bool {
        if self.by_id.contains_key(&resting.id) {
            return false;
        }
        let level = match side {
            Side::Buy => self.bids.entry(price).or_default(),
            Side::Sell => self.asks.entry(price).or_default(),
            Side::Unspecified => return false,
        };
        level.push_back(resting);
        self.by_id.insert(resting.id, (side, price));
        true
    }

    /// Remove an order by id. Returns the cancelled `Resting` if it
    /// was on the book, None otherwise.
    pub fn cancel(&mut self, id: OrderId) -> Option<Resting> {
        let (side, price) = self.by_id.remove(&id)?;
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
            Side::Unspecified => return None,
        };
        let queue = levels.get_mut(&price)?;
        let pos = queue.iter().position(|r| r.id == id)?;
        let removed = queue.remove(pos);
        if queue.is_empty() {
            levels.remove(&price);
        }
        removed
    }

    /// Like `cancel`, but also returns the (side, price) the order
    /// was resting at — used by the public book event emitter that
    /// needs to publish a qty=0 record after a cancel.
    pub fn cancel_with_meta(&mut self, id: OrderId) -> Option<(Side, Decimal)> {
        let (side, price) = *self.by_id.get(&id)?;
        self.cancel(id)?;
        Some((side, price))
    }

    /// Snapshot of every resting entry as (id, side, price). For
    /// callers that need to walk the whole book — e.g., the
    /// gate-closure sweep that cancels counterparty rests.
    pub fn iter_with_meta(&self) -> Vec<(OrderId, Side, Decimal)> {
        self.by_id
            .iter()
            .map(|(id, (side, price))| (*id, *side, *price))
            .collect()
    }

    /// Like `iter_with_meta` but also includes each resting entry's
    /// open quantity — for the WS book-snapshot path that serialises
    /// the live state on a fresh subscriber connect.
    pub fn iter_with_quantity(&self) -> Vec<(OrderId, Side, Decimal, Decimal)> {
        let mut out = Vec::with_capacity(self.by_id.len());
        for (price, queue) in &self.bids {
            for r in queue {
                out.push((r.id, Side::Buy, *price, r.open_qty));
            }
        }
        for (price, queue) in &self.asks {
            for r in queue {
                out.push((r.id, Side::Sell, *price, r.open_qty));
            }
        }
        out
    }

    /// Highest resting buy price (best bid), if any.
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.keys().next_back().copied()
    }

    /// Lowest resting sell price (best ask), if any.
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.keys().next().copied()
    }

    /// Total open quantity at the (side, price) cell. Used by tests
    /// and by the matcher's depth queries; runs in O(d) where d is
    /// queue depth.
    pub fn depth_at(&self, side: Side, price: Decimal) -> Decimal {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
            Side::Unspecified => return Decimal::ZERO,
        };
        levels
            .get(&price)
            .map(|q| q.iter().map(|r| r.open_qty).sum())
            .unwrap_or(Decimal::ZERO)
    }

    /// True iff `id` is on the book.
    pub fn contains(&self, id: OrderId) -> bool {
        self.by_id.contains_key(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Total opposite-side depth at-or-better-than `taker_price`.
    /// Used by the FOK feasibility check before any state mutation.
    pub fn marketable_depth(&self, taker: Side, taker_price: Decimal) -> Decimal {
        let mut total = Decimal::ZERO;
        match taker {
            Side::Buy => {
                for (price, queue) in self.asks.iter() {
                    if *price > taker_price {
                        break;
                    }
                    total += queue.iter().map(|r| r.open_qty).sum::<Decimal>();
                }
            }
            Side::Sell => {
                for (price, queue) in self.bids.iter().rev() {
                    if *price < taker_price {
                        break;
                    }
                    total += queue.iter().map(|r| r.open_qty).sum::<Decimal>();
                }
            }
            Side::Unspecified => {}
        }
        total
    }

    /// Sum of open_qty across every resting entry on both sides.
    /// Used by tests for the conservation invariant and by the UI
    /// for "open book volume" headline numbers.
    pub fn total_open_qty(&self) -> Decimal {
        let bid: Decimal = self
            .bids
            .values()
            .flat_map(|q| q.iter())
            .map(|r| r.open_qty)
            .sum();
        let ask: Decimal = self
            .asks
            .values()
            .flat_map(|q| q.iter())
            .map(|r| r.open_qty)
            .sum();
        bid + ask
    }

    /// Best opposite-side price for a `taker` order. Buyer's best
    /// opposite is the lowest ask; seller's is the highest bid.
    pub fn peek_opposite(&self, taker: Side) -> Option<Decimal> {
        match taker {
            Side::Buy => self.best_ask(),
            Side::Sell => self.best_bid(),
            Side::Unspecified => None,
        }
    }

    /// Consume up to `max_qty` from the front of the best
    /// opposite-side level. Mutates the queue in place; if the front
    /// entry is fully drained, removes it (and the level, if it
    /// empties) and clears the by-id index.
    ///
    /// Returns `(level_price, maker_id, open_before, taken,
    /// fully_consumed)`. `open_before` is the resting entry's qty
    /// before this consume (so the caller can compute `open_after =
    /// open_before - taken` for book-event emission). None when the
    /// opposite side is empty.
    pub fn consume_front(
        &mut self,
        taker: Side,
        max_qty: Decimal,
    ) -> Option<(Decimal, OrderId, Decimal, Decimal, bool)> {
        debug_assert!(max_qty > Decimal::ZERO, "max_qty must be positive");
        let (level_price, queue) = match taker {
            Side::Buy => self.asks.iter_mut().next(),
            Side::Sell => self.bids.iter_mut().next_back(),
            Side::Unspecified => return None,
        }?;
        let level_price = *level_price;
        let resting = queue.front_mut()?;
        let open_before = resting.open_qty;
        let taken = max_qty.min(open_before);
        resting.open_qty -= taken;
        let maker_id = resting.id;
        let fully_consumed = resting.open_qty.is_zero();
        if fully_consumed {
            queue.pop_front();
            if queue.is_empty() {
                match taker {
                    Side::Buy => self.asks.remove(&level_price),
                    Side::Sell => self.bids.remove(&level_price),
                    Side::Unspecified => None,
                };
            }
            self.by_id.remove(&maker_id);
        }
        Some((level_price, maker_id, open_before, taken, fully_consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn r(id: u64, qty: Decimal) -> Resting {
        Resting {
            id: OrderId(id),
            open_qty: qty,
        }
    }

    #[test]
    fn insert_then_best_levels() {
        let mut b = OrderBook::new();
        b.insert(Side::Buy, dec!(85.40), r(1, dec!(1.0)));
        b.insert(Side::Buy, dec!(85.50), r(2, dec!(0.5)));
        b.insert(Side::Sell, dec!(85.60), r(3, dec!(2.0)));
        b.insert(Side::Sell, dec!(85.55), r(4, dec!(0.3)));

        assert_eq!(b.best_bid(), Some(dec!(85.50)));
        assert_eq!(b.best_ask(), Some(dec!(85.55)));
        assert_eq!(b.len(), 4);
    }

    #[test]
    fn fifo_at_same_price() {
        let mut b = OrderBook::new();
        b.insert(Side::Buy, dec!(85.0), r(1, dec!(1.0)));
        b.insert(Side::Buy, dec!(85.0), r(2, dec!(1.0)));
        b.insert(Side::Buy, dec!(85.0), r(3, dec!(1.0)));
        let (_, id, _, _, fully) = b.consume_front(Side::Sell, dec!(1.0)).unwrap();
        assert_eq!(id, OrderId(1));
        assert!(fully);
        let (_, id, _, _, _) = b.consume_front(Side::Sell, dec!(1.0)).unwrap();
        assert_eq!(id, OrderId(2));
    }

    #[test]
    fn cancel_removes_and_compacts_level() {
        let mut b = OrderBook::new();
        b.insert(Side::Sell, dec!(90.0), r(1, dec!(1.0)));
        b.insert(Side::Sell, dec!(90.0), r(2, dec!(2.0)));
        assert_eq!(b.depth_at(Side::Sell, dec!(90.0)), dec!(3.0));

        let removed = b.cancel(OrderId(1)).unwrap();
        assert_eq!(removed.id, OrderId(1));
        assert_eq!(b.depth_at(Side::Sell, dec!(90.0)), dec!(2.0));
        assert!(!b.contains(OrderId(1)));

        b.cancel(OrderId(2)).unwrap();
        assert!(b.is_empty());
        assert_eq!(b.best_ask(), None);
    }

    #[test]
    fn cancel_missing_id_is_noop() {
        let mut b = OrderBook::new();
        assert!(b.cancel(OrderId(99)).is_none());
    }

    #[test]
    fn insert_duplicate_id_is_rejected() {
        let mut b = OrderBook::new();
        assert!(b.insert(Side::Buy, dec!(85.0), r(1, dec!(1.0))));
        assert!(!b.insert(Side::Buy, dec!(86.0), r(1, dec!(1.0))));
        assert_eq!(b.len(), 1);
        assert_eq!(b.best_bid(), Some(dec!(85.0)));
    }

    #[test]
    fn consume_front_partial_then_full() {
        let mut b = OrderBook::new();
        b.insert(Side::Sell, dec!(90.0), r(1, dec!(2.0)));

        let (price, id, _, taken, fully) = b.consume_front(Side::Buy, dec!(0.5)).unwrap();
        assert_eq!(price, dec!(90.0));
        assert_eq!(id, OrderId(1));
        assert_eq!(taken, dec!(0.5));
        assert!(!fully);
        assert!(b.contains(OrderId(1)));
        assert_eq!(b.depth_at(Side::Sell, dec!(90.0)), dec!(1.5));

        let (_, _, _, taken, fully) = b.consume_front(Side::Buy, dec!(5.0)).unwrap();
        assert_eq!(taken, dec!(1.5));
        assert!(fully);
        assert!(b.is_empty());
        assert_eq!(b.best_ask(), None);
    }

    #[test]
    fn peek_opposite_picks_best() {
        let mut b = OrderBook::new();
        b.insert(Side::Sell, dec!(90.0), r(1, dec!(1.0)));
        b.insert(Side::Sell, dec!(91.0), r(2, dec!(1.0)));
        b.insert(Side::Buy, dec!(89.0), r(3, dec!(1.0)));
        b.insert(Side::Buy, dec!(88.5), r(4, dec!(1.0)));
        assert_eq!(b.peek_opposite(Side::Buy), Some(dec!(90.0)));
        assert_eq!(b.peek_opposite(Side::Sell), Some(dec!(89.0)));
    }

    #[test]
    fn consume_front_on_empty_side_returns_none() {
        let mut b = OrderBook::new();
        assert!(b.consume_front(Side::Buy, dec!(1.0)).is_none());
    }
}
