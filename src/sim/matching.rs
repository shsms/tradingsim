//! Continuous matching engine. Phase 3 implements LIMIT-only price-
//! time priority: an incoming order sweeps the opposite half-book in
//! best-price-first order, FIFO within a level, until its quantity is
//! exhausted or the next level no longer crosses the limit. Whatever
//! quantity remains rests at the incoming price.
//!
//! AON / FOK / IOC, iceberg slicing, and STOP_LIMIT activation come
//! in Phase 6 — every entry point here takes a plain `IncomingLimit`,
//! not a full `Order`, so adding the variants is additive.

use rust_decimal::Decimal;

use crate::sim::book::{OrderBook, Resting};
use crate::sim::order::{OrderId, Side};

/// What the matcher takes in. Pre-validated by the server layer:
/// price on grid, qty on grid, qty > 0, currency matches market.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncomingLimit {
    pub id: OrderId,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
}

/// How the matcher treats unmatched quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecMode {
    /// LIMIT without restriction — rest any leftover on the book.
    Resting,
    /// IOC — take what crosses immediately, kill the rest.
    ImmediateOrCancel,
    /// FOK — must match the full quantity, or take none.
    FillOrKill,
}

/// One match event the World layer turns into Trade + PublicTrade
/// records (with timestamps, gridpool ids, etc.). The matcher itself
/// is timeless — it produces only the trade primitives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub taker_id: OrderId,
    pub maker_id: OrderId,
    /// Match price = the resting (maker) order's limit price.
    /// Price-time priority guarantees this is at least as good as
    /// the taker's limit.
    pub price: Decimal,
    pub quantity: Decimal,
}

/// Outcome of a single `match_limit` call.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct LimitMatchOutcome {
    pub fills: Vec<Fill>,
    /// True iff the leftover (if any) was inserted onto the book at
    /// the incoming limit price. Absent => the order didn't fully
    /// fill but couldn't rest — currently unreachable for plain
    /// LIMIT but reserved for IOC/FOK once they land.
    pub rested: Option<Resting>,
}

/// True iff `level_price` is a marketable price for a taker at
/// `taker_price`. A buyer takes an ask priced ≤ its bid; a seller
/// takes a bid priced ≥ its ask.
fn crosses(taker: Side, taker_price: Decimal, level_price: Decimal) -> bool {
    match taker {
        Side::Buy => level_price <= taker_price,
        Side::Sell => level_price >= taker_price,
        Side::Unspecified => false,
    }
}

/// Match a LIMIT order against the book.
///
/// - `Resting`: sweep, then rest any leftover at the incoming price.
/// - `IOC`: sweep, drop any leftover (never rests).
/// - `FOK`: pre-checks marketable depth; only sweeps if the entire
///   quantity is reachable. Otherwise returns an empty outcome
///   (no fills, no rest).
pub fn match_limit(
    book: &mut OrderBook,
    mut taker: IncomingLimit,
    mode: ExecMode,
) -> LimitMatchOutcome {
    debug_assert!(taker.quantity > Decimal::ZERO, "qty must be positive");

    // FOK pre-check: if the book can't absorb the full quantity at
    // crossing prices, bail before any state mutation.
    if mode == ExecMode::FillOrKill {
        let depth = book.marketable_depth(taker.side, taker.price);
        if depth < taker.quantity {
            return LimitMatchOutcome::default();
        }
    }

    let mut fills = Vec::new();
    loop {
        let Some(level_price) = book.peek_opposite(taker.side) else {
            break;
        };
        if !crosses(taker.side, taker.price, level_price) {
            break;
        }
        let (price, maker_id, taken, _fully) = book
            .consume_front(taker.side, taker.quantity)
            .expect("peek_opposite said this side is non-empty");
        debug_assert_eq!(price, level_price);
        fills.push(Fill {
            taker_id: taker.id,
            maker_id,
            price,
            quantity: taken,
        });
        taker.quantity -= taken;
        if taker.quantity.is_zero() {
            break;
        }
    }

    let rested = if mode == ExecMode::Resting && taker.quantity > Decimal::ZERO {
        let resting = Resting {
            id: taker.id,
            open_qty: taker.quantity,
        };
        let inserted = book.insert(taker.side, taker.price, resting);
        debug_assert!(inserted, "taker id was already on the book");
        Some(resting)
    } else {
        None
    };

    LimitMatchOutcome { fills, rested }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn lim(id: u64, side: Side, price: Decimal, qty: Decimal) -> IncomingLimit {
        IncomingLimit {
            id: OrderId(id),
            side,
            price,
            quantity: qty,
        }
    }

    #[test]
    fn no_cross_just_rests() {
        let mut b = OrderBook::new();
        let out = match_limit(&mut b, lim(1, Side::Buy, dec!(85.0), dec!(1.0)), ExecMode::Resting);
        assert!(out.fills.is_empty());
        assert_eq!(out.rested.unwrap().id, OrderId(1));
        assert_eq!(b.best_bid(), Some(dec!(85.0)));
    }

    #[test]
    fn full_match_against_single_resting() {
        let mut b = OrderBook::new();
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(1.0)), ExecMode::Resting);
        let out = match_limit(&mut b, lim(2, Side::Buy, dec!(85.0), dec!(1.0)), ExecMode::Resting);

        assert_eq!(out.fills.len(), 1);
        assert_eq!(
            out.fills[0],
            Fill {
                taker_id: OrderId(2),
                maker_id: OrderId(1),
                price: dec!(85.0),
                quantity: dec!(1.0)
            }
        );
        assert!(out.rested.is_none());
        assert!(b.is_empty());
    }

    #[test]
    fn taker_larger_than_resting_partial_rest() {
        let mut b = OrderBook::new();
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(0.5)), ExecMode::Resting);
        let out = match_limit(&mut b, lim(2, Side::Buy, dec!(86.0), dec!(2.0)), ExecMode::Resting);

        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].quantity, dec!(0.5));
        // Leftover 1.5 rests at the taker's limit price.
        let rested = out.rested.unwrap();
        assert_eq!(rested.id, OrderId(2));
        assert_eq!(rested.open_qty, dec!(1.5));
        assert_eq!(b.best_bid(), Some(dec!(86.0)));
        assert_eq!(b.best_ask(), None);
    }

    #[test]
    fn sweeps_multiple_levels_in_price_order() {
        let mut b = OrderBook::new();
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(0.5)), ExecMode::Resting);
        match_limit(&mut b, lim(2, Side::Sell, dec!(85.5), dec!(0.5)), ExecMode::Resting);
        match_limit(&mut b, lim(3, Side::Sell, dec!(86.0), dec!(0.5)), ExecMode::Resting);

        let out = match_limit(&mut b, lim(4, Side::Buy, dec!(86.0), dec!(1.2)), ExecMode::Resting);
        assert_eq!(out.fills.len(), 3);
        // Best ask first.
        assert_eq!(out.fills[0].price, dec!(85.0));
        assert_eq!(out.fills[1].price, dec!(85.5));
        assert_eq!(out.fills[2].price, dec!(86.0));
        // Third fill partial: only 0.2 left.
        assert_eq!(out.fills[2].quantity, dec!(0.2));
        // Remaining 0.3 of order 3 rests.
        assert_eq!(b.depth_at(Side::Sell, dec!(86.0)), dec!(0.3));
        assert!(out.rested.is_none());
    }

    #[test]
    fn time_priority_at_same_level() {
        let mut b = OrderBook::new();
        // Three sells at 85.0, inserted in id order.
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(1.0)), ExecMode::Resting);
        match_limit(&mut b, lim(2, Side::Sell, dec!(85.0), dec!(1.0)), ExecMode::Resting);
        match_limit(&mut b, lim(3, Side::Sell, dec!(85.0), dec!(1.0)), ExecMode::Resting);

        // Buy 2.5 sweeps id 1 in full, id 2 in full, id 3 partially.
        let out = match_limit(&mut b, lim(4, Side::Buy, dec!(85.0), dec!(2.5)), ExecMode::Resting);
        let maker_ids: Vec<_> = out.fills.iter().map(|f| f.maker_id).collect();
        assert_eq!(maker_ids, vec![OrderId(1), OrderId(2), OrderId(3)]);
        let taken: Vec<_> = out.fills.iter().map(|f| f.quantity).collect();
        assert_eq!(taken, vec![dec!(1.0), dec!(1.0), dec!(0.5)]);
        assert_eq!(b.depth_at(Side::Sell, dec!(85.0)), dec!(0.5));
    }

    #[test]
    fn taker_stops_at_non_crossing_level() {
        let mut b = OrderBook::new();
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(0.3)), ExecMode::Resting);
        match_limit(&mut b, lim(2, Side::Sell, dec!(86.0), dec!(1.0)), ExecMode::Resting); // too expensive

        // Buyer limit at 85.0: only the 85.0 level is takeable.
        let out = match_limit(&mut b, lim(3, Side::Buy, dec!(85.0), dec!(2.0)), ExecMode::Resting);
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].quantity, dec!(0.3));
        // Leftover (1.7) rests at the buyer's 85.0.
        assert_eq!(out.rested.unwrap().open_qty, dec!(1.7));
        assert_eq!(b.best_bid(), Some(dec!(85.0)));
        assert_eq!(b.best_ask(), Some(dec!(86.0)));
    }

    #[test]
    fn match_price_is_maker_price_not_taker() {
        let mut b = OrderBook::new();
        match_limit(&mut b, lim(1, Side::Sell, dec!(85.0), dec!(1.0)), ExecMode::Resting);
        // Buyer willing to pay 90, but matches at 85.
        let out = match_limit(&mut b, lim(2, Side::Buy, dec!(90.0), dec!(1.0)), ExecMode::Resting);
        assert_eq!(out.fills[0].price, dec!(85.0));
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    /// Random order generator restricted to the price/qty grid: prices on
    /// 0.01 EUR/MWh, qtys on 0.1 MW, both bounded so the proptest
    /// shrinker has manageable values.
    fn arb_order() -> impl Strategy<Value = (Side, Decimal, Decimal)> {
        (
            prop::sample::select(vec![Side::Buy, Side::Sell]),
            // 100 prices: 80.00 .. 89.99 in 0.01 steps.
            (0i64..1000).prop_map(|n| Decimal::new(8000 + n, 2)),
            // 1 .. 30 qty steps of 0.1 MW.
            (1i64..30).prop_map(|n| Decimal::new(n, 1)),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// After every match step, best_bid < best_ask. A crossed
        /// book would mean a taker order rested at a price that
        /// still had a marketable opposite-side level — the matcher
        /// failed to sweep.
        #[test]
        fn book_never_crossed(orders in prop::collection::vec(arb_order(), 0..40)) {
            let mut book = OrderBook::new();
            for (i, (side, price, qty)) in orders.iter().enumerate() {
                match_limit(&mut book, IncomingLimit {
                    id: OrderId(i as u64),
                    side: *side,
                    price: *price,
                    quantity: *qty,
                }, ExecMode::Resting);
                if let (Some(bb), Some(ba)) = (book.best_bid(), book.best_ask()) {
                    prop_assert!(bb < ba, "crossed book after op {i}: bid {bb} >= ask {ba}");
                }
            }
        }

        /// Quantity conservation: every unit of qty that arrives is
        /// either filled twice over (once as taker, once as maker on
        /// the matched leg) or rests on the book. Concretely:
        ///   sum(incoming.qty) == 2 * sum(fills) + book.total_open_qty()
        #[test]
        fn quantity_conserved(orders in prop::collection::vec(arb_order(), 0..40)) {
            let mut book = OrderBook::new();
            let mut total_in = Decimal::ZERO;
            let mut total_fills = Decimal::ZERO;
            for (i, (side, price, qty)) in orders.iter().enumerate() {
                total_in += qty;
                let out = match_limit(&mut book, IncomingLimit {
                    id: OrderId(i as u64),
                    side: *side,
                    price: *price,
                    quantity: *qty,
                }, ExecMode::Resting);
                for f in &out.fills {
                    total_fills += f.quantity;
                }
            }
            prop_assert_eq!(total_in, dec!(2) * total_fills + book.total_open_qty());
        }

        /// Every fill matches at a price favorable to both sides:
        /// the maker's price is at-or-better than the taker's limit.
        #[test]
        fn fill_prices_obey_limits(orders in prop::collection::vec(arb_order(), 0..40)) {
            let mut book = OrderBook::new();
            for (i, (side, price, qty)) in orders.iter().enumerate() {
                let taker_limit = *price;
                let taker_side = *side;
                let out = match_limit(&mut book, IncomingLimit {
                    id: OrderId(i as u64),
                    side: taker_side,
                    price: taker_limit,
                    quantity: *qty,
                }, ExecMode::Resting);
                for f in &out.fills {
                    match taker_side {
                        Side::Buy => prop_assert!(f.price <= taker_limit),
                        Side::Sell => prop_assert!(f.price >= taker_limit),
                        Side::Unspecified => prop_assert!(false, "generator only emits Buy/Sell"),
                    }
                }
            }
        }
    }
}
