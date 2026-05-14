//! Trade types. `Trade` is the per-gridpool, one-side view (a fill
//! generates one `Trade` per side, each visible only to its
//! gridpool's owner). `PublicTrade` is the globally-visible match
//! event — exactly one per fill, even when the two sides are in
//! different delivery areas (cross-area / SIDC coupling, Phase 7).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use crate::proto::trading::TradeState;

use crate::sim::market::{Area, Currency, DeliveryPeriod};
use crate::sim::order::{OrderId, Side};

/// Monotonic server-assigned trade id. Independent of `OrderId` so
/// callers can't infer order counts from trade counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TradeId(pub u64);

impl TradeState {
    /// True iff the trade has settled into a terminal state (no
    /// further transitions possible).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Canceled | Self::Recalled)
    }
}

/// Per-gridpool, one-side trade record — `proto::trading::Trade`.
#[derive(Clone, Debug)]
pub struct Trade {
    pub id: TradeId,
    pub order_id: OrderId,
    pub side: Side,
    pub area: Area,
    pub period: DeliveryPeriod,
    pub execution_time: DateTime<Utc>,
    pub price: Decimal,
    pub currency: Currency,
    pub quantity: Decimal,
    pub state: TradeState,
}

/// Globally-visible match event — `proto::trading::PublicTrade`. The
/// two area fields differ only on cross-area SIDC matches; in the
/// single-area case both equal the trade's delivery area.
#[derive(Clone, Debug)]
pub struct PublicTrade {
    pub id: TradeId,
    pub buy_area: Area,
    pub sell_area: Area,
    pub period: DeliveryPeriod,
    pub execution_time: DateTime<Utc>,
    pub price: Decimal,
    pub currency: Currency,
    pub quantity: Decimal,
    pub state: TradeState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_trade_states() {
        for s in [TradeState::Canceled, TradeState::Recalled] {
            assert!(s.is_terminal(), "{s:?} should be terminal");
        }
        for s in [
            TradeState::Active,
            TradeState::CancelRequested,
            TradeState::CancelRejected,
            TradeState::RecallRequested,
            TradeState::RecallRejected,
            TradeState::ApprovalRequested,
        ] {
            assert!(!s.is_terminal(), "{s:?} should NOT be terminal");
        }
    }
}
