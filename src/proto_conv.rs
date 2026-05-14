//! Bridges between proto-generated types (`crate::proto::*`) and the
//! internal sim composite types (`crate::sim::{Order,Trade,...}`).
//! Enum types are re-exported by the sim modules — there are no
//! enum bridges here; this file only handles Decimal, Timestamp, and
//! the composite messages that flatten differently between proto and
//! sim (Price → (Decimal, Currency); Power → Decimal; Order has
//! all-Option proto fields but unwrap-required sim fields).

use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;

use crate::proto::common::{grid as proto_grid, market as proto_market, types as proto_types};
use crate::proto::trading as proto_trading;
use crate::sim::market::{Area, CodeType, Currency, DeliveryDuration, DeliveryPeriod};
use crate::sim::order::{
    ExecutionOption, MarketActor, Order, OrderDetail, OrderId, OrderState, OrderType, Side,
    StateDetail, StateReason,
};
use crate::sim::trade::{PublicTrade, Trade, TradeId, TradeState};

/// Errors that can occur converting from the wire into sim types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvError {
    /// A submessage field was None where the sim requires a value.
    MissingField(&'static str),
    /// An enumeration value didn't map to a known variant — typically
    /// `0` (the proto's `_UNSPECIFIED` sentinel) or a future variant
    /// from a wire newer than the build.
    UnknownEnum { field: &'static str, value: i32 },
    /// A `Decimal { value: string }` could not be parsed.
    InvalidDecimal(String),
    /// A Timestamp had nanos / seconds out of the chrono range.
    InvalidTimestamp,
}

impl std::fmt::Display for ConvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing required field: {name}"),
            Self::UnknownEnum { field, value } => {
                write!(f, "unknown enum value {value} for {field}")
            }
            Self::InvalidDecimal(s) => write!(f, "invalid decimal: {s:?}"),
            Self::InvalidTimestamp => write!(f, "invalid timestamp"),
        }
    }
}

impl std::error::Error for ConvError {}

/// Decode a wire-level i32 into its typed proto enum. The proto's
/// `Unspecified` variant survives the trip — call sites that want to
/// reject it pattern-match on the result. Unknown values (future
/// variants) become `UnknownEnum`.
fn decode_enum<E>(value: i32, field: &'static str) -> Result<E, ConvError>
where
    E: TryFrom<i32>,
{
    E::try_from(value).map_err(|_| ConvError::UnknownEnum { field, value })
}

/// Like `decode_enum` but also rejects the proto's `Unspecified`
/// variant. Most callers want this — `Unspecified` is the wire's
/// "field not set" sentinel and gets validation-failed at the
/// boundary.
fn decode_enum_no_unspecified<E>(
    value: i32,
    field: &'static str,
    is_unspecified: impl FnOnce(&E) -> bool,
) -> Result<E, ConvError>
where
    E: TryFrom<i32>,
{
    let e = decode_enum(value, field)?;
    if is_unspecified(&e) {
        Err(ConvError::UnknownEnum { field, value })
    } else {
        Ok(e)
    }
}

/// `proto::Decimal` (string-encoded) -> `rust_decimal::Decimal`.
pub fn decimal_from_proto(d: &proto_types::Decimal) -> Result<Decimal, ConvError> {
    Decimal::from_str(&d.value).map_err(|_| ConvError::InvalidDecimal(d.value.clone()))
}

/// `rust_decimal::Decimal` -> `proto::Decimal`.
pub fn decimal_to_proto(d: Decimal) -> proto_types::Decimal {
    proto_types::Decimal {
        value: d.normalize().to_string(),
    }
}

/// `prost_types::Timestamp` -> `DateTime<Utc>`. Rejects out-of-range
/// nanos rather than silently clamping.
pub fn timestamp_from_proto(ts: &prost_types::Timestamp) -> Result<DateTime<Utc>, ConvError> {
    if ts.nanos < 0 || ts.nanos >= 1_000_000_000 {
        return Err(ConvError::InvalidTimestamp);
    }
    Utc.timestamp_opt(ts.seconds, ts.nanos as u32)
        .single()
        .ok_or(ConvError::InvalidTimestamp)
}

/// `DateTime<Utc>` -> `prost_types::Timestamp`.
pub fn timestamp_to_proto(dt: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

// ---------------------------------------------------------------------------
// Area / DeliveryPeriod — sim shapes them with parsed types; proto
// keeps timestamps + i32 enums.
// ---------------------------------------------------------------------------

impl From<&Area> for proto_grid::DeliveryArea {
    fn from(a: &Area) -> Self {
        Self {
            code: a.code.clone(),
            code_type: a.code_type as i32,
        }
    }
}

impl TryFrom<&proto_grid::DeliveryArea> for Area {
    type Error = ConvError;

    fn try_from(p: &proto_grid::DeliveryArea) -> Result<Self, Self::Error> {
        let code_type =
            decode_enum_no_unspecified::<CodeType>(p.code_type, "EnergyMarketCodeType", |e| {
                matches!(e, CodeType::Unspecified)
            })?;
        Ok(Self {
            code: p.code.clone(),
            code_type,
        })
    }
}

impl From<DeliveryPeriod> for proto_grid::DeliveryPeriod {
    fn from(p: DeliveryPeriod) -> Self {
        Self {
            start: Some(timestamp_to_proto(p.start)),
            duration: p.duration as i32,
        }
    }
}

impl TryFrom<&proto_grid::DeliveryPeriod> for DeliveryPeriod {
    type Error = ConvError;

    fn try_from(p: &proto_grid::DeliveryPeriod) -> Result<Self, Self::Error> {
        let start = p
            .start
            .as_ref()
            .ok_or(ConvError::MissingField("DeliveryPeriod.start"))?;
        let duration =
            decode_enum_no_unspecified::<DeliveryDuration>(p.duration, "DeliveryDuration", |d| {
                matches!(d, DeliveryDuration::Unspecified)
            })?;
        Ok(Self {
            start: timestamp_from_proto(start)?,
            duration,
        })
    }
}

/// `proto::market::Price` -> (amount, currency).
pub fn price_from_proto(p: &proto_market::Price) -> Result<(Decimal, Currency), ConvError> {
    let amount = p
        .amount
        .as_ref()
        .ok_or(ConvError::MissingField("Price.amount"))?;
    let currency = decode_enum_no_unspecified::<Currency>(p.currency, "Currency", |c| {
        matches!(c, Currency::Unspecified)
    })?;
    Ok((decimal_from_proto(amount)?, currency))
}

/// (amount, currency) -> `proto::market::Price`.
pub fn price_to_proto(amount: Decimal, currency: Currency) -> proto_market::Price {
    proto_market::Price {
        amount: Some(decimal_to_proto(amount)),
        currency: currency as i32,
    }
}

/// `proto::market::Power` -> MW amount.
pub fn power_from_proto(p: &proto_market::Power) -> Result<Decimal, ConvError> {
    let mw = p.mw.as_ref().ok_or(ConvError::MissingField("Power.mw"))?;
    decimal_from_proto(mw)
}

/// MW amount -> `proto::market::Power`.
pub fn power_to_proto(mw: Decimal) -> proto_market::Power {
    proto_market::Power {
        mw: Some(decimal_to_proto(mw)),
    }
}

// ---------------------------------------------------------------------------
// Order / OrderDetail / StateDetail
// Payload (proto Struct <-> serde_json::Value) is deferred; conversion
// drops it in both directions.
// ---------------------------------------------------------------------------

impl From<&Order> for proto_trading::Order {
    fn from(o: &Order) -> Self {
        Self {
            delivery_area: Some((&o.area).into()),
            delivery_period: Some(o.period.into()),
            r#type: o.order_type as i32,
            side: o.side as i32,
            price: Some(price_to_proto(o.price, o.currency)),
            quantity: Some(power_to_proto(o.quantity)),
            stop_price: o.stop_price.map(|p| price_to_proto(p, o.currency)),
            peak_price_delta: o.peak_price_delta.map(|p| price_to_proto(p, o.currency)),
            display_quantity: o.display_quantity.map(power_to_proto),
            execution_option: o.execution_option.map(|e| e as i32),
            valid_until: o.valid_until.map(timestamp_to_proto),
            payload: None,
            tag: o.tag.clone(),
        }
    }
}

impl TryFrom<&proto_trading::Order> for Order {
    type Error = ConvError;

    fn try_from(p: &proto_trading::Order) -> Result<Self, Self::Error> {
        let area = Area::try_from(
            p.delivery_area
                .as_ref()
                .ok_or(ConvError::MissingField("Order.delivery_area"))?,
        )?;
        let period = DeliveryPeriod::try_from(
            p.delivery_period
                .as_ref()
                .ok_or(ConvError::MissingField("Order.delivery_period"))?,
        )?;
        let price_proto = p
            .price
            .as_ref()
            .ok_or(ConvError::MissingField("Order.price"))?;
        let qty_proto = p
            .quantity
            .as_ref()
            .ok_or(ConvError::MissingField("Order.quantity"))?;
        let (price, currency) = price_from_proto(price_proto)?;

        let order_type = decode_enum_no_unspecified::<OrderType>(p.r#type, "OrderType", |t| {
            matches!(t, OrderType::Unspecified)
        })?;
        let side = decode_enum_no_unspecified::<Side>(p.side, "MarketSide", |s| {
            matches!(s, Side::Unspecified)
        })?;

        // Optional Price fields must share the order's currency.
        let same_currency_or_err =
            |o: &Option<proto_market::Price>, field| -> Result<Option<Decimal>, ConvError> {
                match o {
                    Some(p) => {
                        let (amt, c) = price_from_proto(p)?;
                        if c != currency {
                            return Err(ConvError::UnknownEnum {
                                field,
                                value: c as i32,
                            });
                        }
                        Ok(Some(amt))
                    }
                    None => Ok(None),
                }
            };

        let execution_option = match p.execution_option {
            Some(raw) => Some(decode_enum_no_unspecified::<ExecutionOption>(
                raw,
                "OrderExecutionOption",
                |e| matches!(e, ExecutionOption::Unspecified),
            )?),
            None => None,
        };

        Ok(Self {
            area,
            period,
            order_type,
            side,
            price,
            currency,
            quantity: power_from_proto(qty_proto)?,
            stop_price: same_currency_or_err(&p.stop_price, "Order.stop_price.currency")?,
            peak_price_delta: same_currency_or_err(
                &p.peak_price_delta,
                "Order.peak_price_delta.currency",
            )?,
            display_quantity: p
                .display_quantity
                .as_ref()
                .map(power_from_proto)
                .transpose()?,
            execution_option,
            valid_until: p
                .valid_until
                .as_ref()
                .map(timestamp_from_proto)
                .transpose()?,
            payload: None,
            tag: p.tag.clone(),
        })
    }
}

impl From<StateDetail> for proto_trading::order_detail::StateDetail {
    fn from(s: StateDetail) -> Self {
        Self {
            state: s.state as i32,
            state_reason: s.reason as i32,
            market_actor: s.actor as i32,
        }
    }
}

impl TryFrom<&proto_trading::order_detail::StateDetail> for StateDetail {
    type Error = ConvError;

    fn try_from(p: &proto_trading::order_detail::StateDetail) -> Result<Self, Self::Error> {
        let state = decode_enum_no_unspecified::<OrderState>(p.state, "OrderState", |s| {
            matches!(s, OrderState::Unspecified)
        })?;
        let reason =
            decode_enum_no_unspecified::<StateReason>(p.state_reason, "StateReason", |s| {
                matches!(s, StateReason::Unspecified)
            })?;
        let actor =
            decode_enum_no_unspecified::<MarketActor>(p.market_actor, "MarketActor", |a| {
                matches!(a, MarketActor::Unspecified)
            })?;
        Ok(Self {
            state,
            reason,
            actor,
        })
    }
}

impl From<&OrderDetail> for proto_trading::OrderDetail {
    fn from(o: &OrderDetail) -> Self {
        Self {
            order_id: o.id.0,
            order: Some((&o.order).into()),
            state_detail: Some(o.state.into()),
            open_quantity: Some(power_to_proto(o.open_quantity)),
            filled_quantity: Some(power_to_proto(o.filled_quantity)),
            create_time: Some(timestamp_to_proto(o.create_time)),
            modification_time: Some(timestamp_to_proto(o.modification_time)),
        }
    }
}

impl TryFrom<&proto_trading::OrderDetail> for OrderDetail {
    type Error = ConvError;

    fn try_from(p: &proto_trading::OrderDetail) -> Result<Self, Self::Error> {
        let order_proto = p
            .order
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.order"))?;
        let state_proto = p
            .state_detail
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.state_detail"))?;
        let open_qty_proto = p
            .open_quantity
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.open_quantity"))?;
        let filled_qty_proto = p
            .filled_quantity
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.filled_quantity"))?;
        let create_proto = p
            .create_time
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.create_time"))?;
        let mod_proto = p
            .modification_time
            .as_ref()
            .ok_or(ConvError::MissingField("OrderDetail.modification_time"))?;

        Ok(Self {
            id: OrderId(p.order_id),
            order: Order::try_from(order_proto)?,
            state: StateDetail::try_from(state_proto)?,
            open_quantity: power_from_proto(open_qty_proto)?,
            filled_quantity: power_from_proto(filled_qty_proto)?,
            create_time: timestamp_from_proto(create_proto)?,
            modification_time: timestamp_from_proto(mod_proto)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Trade / PublicTrade
// ---------------------------------------------------------------------------

impl From<&Trade> for proto_trading::Trade {
    fn from(t: &Trade) -> Self {
        Self {
            id: t.id.0,
            order_id: t.order_id.0,
            side: t.side as i32,
            delivery_area: Some((&t.area).into()),
            delivery_period: Some(t.period.into()),
            execution_time: Some(timestamp_to_proto(t.execution_time)),
            price: Some(price_to_proto(t.price, t.currency)),
            quantity: Some(power_to_proto(t.quantity)),
            state: t.state as i32,
        }
    }
}

impl From<&PublicTrade> for proto_trading::PublicTrade {
    fn from(t: &PublicTrade) -> Self {
        Self {
            id: t.id.0,
            buy_delivery_area: Some((&t.buy_area).into()),
            sell_delivery_area: Some((&t.sell_area).into()),
            delivery_period: Some(t.period.into()),
            execution_time: Some(timestamp_to_proto(t.execution_time)),
            price: Some(price_to_proto(t.price, t.currency)),
            quantity: Some(power_to_proto(t.quantity)),
            state: t.state as i32,
        }
    }
}

// Inbound Trade / PublicTrade conversions are only used by tests
// today; keep them so the round-trip property tests still pass.

impl TryFrom<&proto_trading::Trade> for Trade {
    type Error = ConvError;

    fn try_from(p: &proto_trading::Trade) -> Result<Self, Self::Error> {
        let area_proto = p
            .delivery_area
            .as_ref()
            .ok_or(ConvError::MissingField("Trade.delivery_area"))?;
        let period_proto = p
            .delivery_period
            .as_ref()
            .ok_or(ConvError::MissingField("Trade.delivery_period"))?;
        let exec_proto = p
            .execution_time
            .as_ref()
            .ok_or(ConvError::MissingField("Trade.execution_time"))?;
        let price_proto = p
            .price
            .as_ref()
            .ok_or(ConvError::MissingField("Trade.price"))?;
        let qty_proto = p
            .quantity
            .as_ref()
            .ok_or(ConvError::MissingField("Trade.quantity"))?;
        let (price, currency) = price_from_proto(price_proto)?;
        Ok(Self {
            id: TradeId(p.id),
            order_id: OrderId(p.order_id),
            side: decode_enum_no_unspecified::<Side>(p.side, "MarketSide", |s| {
                matches!(s, Side::Unspecified)
            })?,
            area: Area::try_from(area_proto)?,
            period: DeliveryPeriod::try_from(period_proto)?,
            execution_time: timestamp_from_proto(exec_proto)?,
            price,
            currency,
            quantity: power_from_proto(qty_proto)?,
            state: decode_enum_no_unspecified::<TradeState>(p.state, "TradeState", |s| {
                matches!(s, TradeState::Unspecified)
            })?,
        })
    }
}

impl TryFrom<&proto_trading::PublicTrade> for PublicTrade {
    type Error = ConvError;

    fn try_from(p: &proto_trading::PublicTrade) -> Result<Self, Self::Error> {
        let buy = p
            .buy_delivery_area
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.buy_delivery_area"))?;
        let sell = p
            .sell_delivery_area
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.sell_delivery_area"))?;
        let period_proto = p
            .delivery_period
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.delivery_period"))?;
        let exec_proto = p
            .execution_time
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.execution_time"))?;
        let price_proto = p
            .price
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.price"))?;
        let qty_proto = p
            .quantity
            .as_ref()
            .ok_or(ConvError::MissingField("PublicTrade.quantity"))?;
        let (price, currency) = price_from_proto(price_proto)?;
        Ok(Self {
            id: TradeId(p.id),
            buy_area: Area::try_from(buy)?,
            sell_area: Area::try_from(sell)?,
            period: DeliveryPeriod::try_from(period_proto)?,
            execution_time: timestamp_from_proto(exec_proto)?,
            price,
            currency,
            quantity: power_from_proto(qty_proto)?,
            state: decode_enum_no_unspecified::<TradeState>(p.state, "TradeState", |s| {
                matches!(s, TradeState::Unspecified)
            })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::dec;

    #[test]
    fn decimal_round_trip() {
        for raw in ["0", "1", "-12.345", "85.50", "0.00001"] {
            let proto = proto_types::Decimal {
                value: raw.to_string(),
            };
            let sim = decimal_from_proto(&proto).unwrap();
            let back = decimal_to_proto(sim);
            assert_eq!(Decimal::from_str(&back.value).unwrap(), sim);
        }
    }

    #[test]
    fn timestamp_round_trip() {
        let dt = Utc.with_ymd_and_hms(2026, 5, 13, 12, 34, 56).unwrap()
            + chrono::Duration::nanoseconds(789);
        let proto = timestamp_to_proto(dt);
        assert_eq!(proto.nanos, 789);
        assert_eq!(timestamp_from_proto(&proto).unwrap(), dt);
    }

    #[test]
    fn timestamp_rejects_invalid_nanos() {
        let bad = prost_types::Timestamp {
            seconds: 0,
            nanos: -1,
        };
        assert_eq!(
            timestamp_from_proto(&bad).unwrap_err(),
            ConvError::InvalidTimestamp
        );
    }

    #[test]
    fn area_round_trip() {
        let sim = Area::eic("10YDE-EON------1");
        let p: proto_grid::DeliveryArea = (&sim).into();
        assert_eq!(p.code, sim.code);
        assert_eq!(p.code_type, CodeType::EuropeEic as i32);
        let back = Area::try_from(&p).unwrap();
        assert_eq!(back, sim);
    }

    #[test]
    fn area_rejects_unspecified() {
        let bad = proto_grid::DeliveryArea {
            code: "X".into(),
            code_type: 0,
        };
        let err = Area::try_from(&bad).unwrap_err();
        assert!(matches!(
            err,
            ConvError::UnknownEnum {
                field: "EnergyMarketCodeType",
                ..
            }
        ));
    }

    #[test]
    fn delivery_period_round_trip() {
        let sim = DeliveryPeriod {
            start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            duration: DeliveryDuration::DeliveryDuration15,
        };
        let p: proto_grid::DeliveryPeriod = sim.into();
        assert_eq!(DeliveryPeriod::try_from(&p).unwrap(), sim);
    }

    fn sample_order() -> Order {
        Order {
            area: Area::eic("10YDE-EON------1"),
            period: DeliveryPeriod {
                start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
                duration: DeliveryDuration::DeliveryDuration60,
            },
            order_type: OrderType::Limit,
            side: Side::Buy,
            price: dec!(85.50),
            currency: Currency::Eur,
            quantity: dec!(1.5),
            stop_price: None,
            peak_price_delta: None,
            display_quantity: None,
            execution_option: None,
            valid_until: None,
            payload: None,
            tag: Some("strategy=arb".into()),
        }
    }

    #[test]
    fn order_round_trip_minimal() {
        let sim = sample_order();
        let proto = proto_trading::Order::from(&sim);
        let back = Order::try_from(&proto).unwrap();
        assert_eq!(back, sim);
    }

    #[test]
    fn order_currency_mismatch_on_stop_price() {
        let proto = proto_trading::Order {
            stop_price: Some(price_to_proto(dec!(80.0), Currency::Usd)),
            ..proto_trading::Order::from(&sample_order())
        };
        assert!(matches!(
            Order::try_from(&proto).unwrap_err(),
            ConvError::UnknownEnum {
                field: "Order.stop_price.currency",
                ..
            }
        ));
    }
}
