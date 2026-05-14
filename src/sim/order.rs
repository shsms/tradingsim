//! Order data types and state machines.
//!
//! State-machine enums (Side, OrderType, ExecutionOption, OrderState,
//! StateReason, MarketActor) are the proto-generated types directly —
//! duplicating them in sim only paid the cost of ten near-identical
//! From/TryFrom pairs. Since the proto module lives inside our crate,
//! inherent impls (Side::opposite, OrderState::is_terminal) can be
//! attached here without an orphan-rule fight.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use crate::proto::trading::order_detail::state_detail::{MarketActor, StateReason};
pub use crate::proto::trading::{
    MarketSide as Side, OrderExecutionOption as ExecutionOption, OrderState, OrderType,
};

use crate::sim::market::{Area, Currency, DeliveryPeriod};

/// Monotonic server-assigned id. Allocated at admit-time, not at
/// validation, so a rejected order never burns an id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OrderId(pub u64);

/// Gridpool id from `CreateGridpoolOrderRequest.gridpool_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GridpoolId(pub u64);

impl Side {
    /// Buy ↔ Sell. Unspecified passes through to itself — the sim
    /// never constructs Unspecified, and the admit-time validator
    /// catches it before the matcher sees it.
    pub fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
            Self::Unspecified => Self::Unspecified,
        }
    }
}

impl OrderState {
    /// True iff no further matching, cancel, or modify can happen.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Expired | Self::Failed
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StateDetail {
    pub state: OrderState,
    pub reason: StateReason,
    pub actor: MarketActor,
}

/// The user-supplied half of an order — `Order` in the proto. Server
/// fields (id, fills, timestamps, state) live on `OrderDetail`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub area: Area,
    pub period: DeliveryPeriod,
    pub order_type: OrderType,
    pub side: Side,
    pub price: Decimal,
    pub currency: Currency,
    pub quantity: Decimal,

    /// STOP_LIMIT trigger price; required for STOP_LIMIT, otherwise None.
    pub stop_price: Option<Decimal>,

    /// ICEBERG: difference between peak slice price and limit price.
    pub peak_price_delta: Option<Decimal>,

    /// ICEBERG: per-slice quantity exposed on the public book.
    pub display_quantity: Option<Decimal>,

    pub execution_option: Option<ExecutionOption>,

    /// Auto-cancel if not filled by this UTC time. Must be in the
    /// future at admit time; mutually exclusive with FOK/IOC per the
    /// proto comment on `valid_until`.
    pub valid_until: Option<DateTime<Utc>>,

    /// Opaque user payload (proto's `google.protobuf.Struct`),
    /// preserved verbatim on output. Phase 4 stores raw JSON.
    pub payload: Option<serde_json::Value>,

    /// User-defined tag for grouping; matched in filters.
    pub tag: Option<String>,
}

/// The server-augmented view — `OrderDetail` in the proto. Combines
/// the user-submitted `Order` with id, fills, state, and timestamps.
#[derive(Clone, Debug)]
pub struct OrderDetail {
    pub id: OrderId,
    pub order: Order,
    pub state: StateDetail,
    pub open_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub create_time: DateTime<Utc>,
    pub modification_time: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_opposite_round_trips() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
        assert_eq!(Side::Buy.opposite().opposite(), Side::Buy);
    }

    #[test]
    fn terminal_states_match_proto_meaning() {
        for s in [
            OrderState::Filled,
            OrderState::Canceled,
            OrderState::Expired,
            OrderState::Failed,
        ] {
            assert!(s.is_terminal(), "{s:?} should be terminal");
        }
        for s in [
            OrderState::Pending,
            OrderState::Active,
            OrderState::Hibernate,
        ] {
            assert!(!s.is_terminal(), "{s:?} should NOT be terminal");
        }
    }
}
