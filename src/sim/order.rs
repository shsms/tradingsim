//! Order data types and state machines. Pure data — the matcher,
//! validator, and proto bridge live elsewhere. Each enum mirrors a
//! proto enum 1:1 (proto bridging is the next commit).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::sim::market::{Area, Currency, DeliveryPeriod};

/// Monotonic server-assigned id. Allocated on admit-time, not at
/// validation, so a rejected order never burns an id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OrderId(pub u64);

/// Gridpool id from `CreateGridpoolOrderRequest.gridpool_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GridpoolId(pub u64);

/// `proto::trading::MarketSide`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// `proto::trading::OrderType`. Only LIMIT is wired in Phase 4; the
/// rest land in the phases noted in plan.org §Order types table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderType {
    Limit,
    StopLimit,
    Iceberg,
    Block,
    Balance,
    Prearranged,
    Private,
}

/// `proto::trading::OrderExecutionOption`. None means "rest on the
/// book until filled, cancelled, or expired" (the proto's implicit
/// default).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionOption {
    /// All-or-None: matches only against an aggregate ≥ full qty.
    Aon,
    /// Fill-or-Kill: match in full immediately or reject.
    Fok,
    /// Immediate-or-Cancel: match what's available, cancel the rest.
    Ioc,
}

/// `proto::trading::OrderState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Admitted, not yet seen by the matcher.
    Pending,
    /// On the book (visible portion, for iceberg).
    Active,
    /// Open quantity reached zero.
    Filled,
    /// Cancelled by user, system, or operator.
    Canceled,
    /// `valid_until` reached.
    Expired,
    /// Validation rejected; never made it to the book.
    Failed,
    /// Stop order armed, not yet triggered.
    Hibernate,
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

/// `proto::trading::OrderDetail.StateDetail.StateReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateReason {
    Add,
    Modify,
    Delete,
    Deactivate,
    Reject,
    FullExecution,
    PartialExecution,
    IcebergSliceAdd,
    ValidationFail,
    UnknownState,
    QuoteAdd,
    QuoteFullExecution,
    QuotePartialExecution,
}

/// `proto::trading::OrderDetail.StateDetail.MarketActor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarketActor {
    User,
    MarketOperator,
    System,
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
        for s in [OrderState::Pending, OrderState::Active, OrderState::Hibernate] {
            assert!(!s.is_terminal(), "{s:?} should NOT be terminal");
        }
    }
}
