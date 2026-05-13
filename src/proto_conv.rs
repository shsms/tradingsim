//! Bridges between proto-generated types (`crate::proto::*`) and the
//! internal sim types (`crate::sim::*`). This file is the only place
//! that knows about both — every other module talks in either-pure-
//! sim or pure-proto.
//!
//! Conventions:
//!   - Lossless conversions use `From`.
//!   - Fallible conversions use `TryFrom` with [`ConvError`].
//!   - `proto::T::Unspecified` always errors out; the server is
//!     responsible for translating that into an InvalidArgument /
//!     ValidationFail at the gRPC boundary.

use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;

use crate::proto::common::{grid as proto_grid, market as proto_market, types as proto_types};
use crate::proto::trading as proto_trading;
use crate::sim::market::{Area, CodeType, Currency, DeliveryDuration, DeliveryPeriod};
use crate::sim::order::{
    ExecutionOption, MarketActor, OrderState, OrderType, Side, StateReason,
};
use crate::sim::trade::TradeState;

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

/// `prost_types::Timestamp` -> `DateTime<Utc>`. Rejects negative
/// nanos / out-of-range seconds rather than silently clamping.
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

/// Helper for the struct-conversion layer: take the raw i32 the proto
/// stores for an enum field, lift it to the typed proto enum, then
/// translate to the sim enum. The two failure modes both surface as
/// `ConvError::UnknownEnum`.
pub fn sim_enum_from_i32<P, S>(value: i32, field: &'static str) -> Result<S, ConvError>
where
    P: TryFrom<i32>,
    S: TryFrom<P, Error = ConvError>,
{
    let proto = P::try_from(value).map_err(|_| ConvError::UnknownEnum { field, value })?;
    S::try_from(proto)
}

// ---------------------------------------------------------------------------
// Enum bridges. Pattern for each pair:
//   - From<sim_enum> for proto_enum     — lossless, doesn't yield Unspecified
//   - TryFrom<proto_enum> for sim_enum  — Unspecified / unknown -> ConvError
// ---------------------------------------------------------------------------

impl From<Side> for proto_trading::MarketSide {
    fn from(s: Side) -> Self {
        match s {
            Side::Buy => Self::Buy,
            Side::Sell => Self::Sell,
        }
    }
}

impl TryFrom<proto_trading::MarketSide> for Side {
    type Error = ConvError;

    fn try_from(p: proto_trading::MarketSide) -> Result<Self, Self::Error> {
        match p {
            proto_trading::MarketSide::Buy => Ok(Self::Buy),
            proto_trading::MarketSide::Sell => Ok(Self::Sell),
            proto_trading::MarketSide::Unspecified => Err(ConvError::UnknownEnum {
                field: "MarketSide",
                value: p as i32,
            }),
        }
    }
}

impl From<OrderType> for proto_trading::OrderType {
    fn from(t: OrderType) -> Self {
        match t {
            OrderType::Limit => Self::Limit,
            OrderType::StopLimit => Self::StopLimit,
            OrderType::Iceberg => Self::Iceberg,
            OrderType::Block => Self::Block,
            OrderType::Balance => Self::Balance,
            OrderType::Prearranged => Self::Prearranged,
            OrderType::Private => Self::Private,
        }
    }
}

impl TryFrom<proto_trading::OrderType> for OrderType {
    type Error = ConvError;

    fn try_from(p: proto_trading::OrderType) -> Result<Self, Self::Error> {
        match p {
            proto_trading::OrderType::Limit => Ok(Self::Limit),
            proto_trading::OrderType::StopLimit => Ok(Self::StopLimit),
            proto_trading::OrderType::Iceberg => Ok(Self::Iceberg),
            proto_trading::OrderType::Block => Ok(Self::Block),
            proto_trading::OrderType::Balance => Ok(Self::Balance),
            proto_trading::OrderType::Prearranged => Ok(Self::Prearranged),
            proto_trading::OrderType::Private => Ok(Self::Private),
            proto_trading::OrderType::Unspecified => Err(ConvError::UnknownEnum {
                field: "OrderType",
                value: p as i32,
            }),
        }
    }
}

impl From<ExecutionOption> for proto_trading::OrderExecutionOption {
    fn from(e: ExecutionOption) -> Self {
        match e {
            ExecutionOption::Aon => Self::Aon,
            ExecutionOption::Fok => Self::Fok,
            ExecutionOption::Ioc => Self::Ioc,
        }
    }
}

impl TryFrom<proto_trading::OrderExecutionOption> for ExecutionOption {
    type Error = ConvError;

    fn try_from(p: proto_trading::OrderExecutionOption) -> Result<Self, Self::Error> {
        match p {
            proto_trading::OrderExecutionOption::Aon => Ok(Self::Aon),
            proto_trading::OrderExecutionOption::Fok => Ok(Self::Fok),
            proto_trading::OrderExecutionOption::Ioc => Ok(Self::Ioc),
            proto_trading::OrderExecutionOption::Unspecified => Err(ConvError::UnknownEnum {
                field: "OrderExecutionOption",
                value: p as i32,
            }),
        }
    }
}

impl From<OrderState> for proto_trading::OrderState {
    fn from(s: OrderState) -> Self {
        match s {
            OrderState::Pending => Self::Pending,
            OrderState::Active => Self::Active,
            OrderState::Filled => Self::Filled,
            OrderState::Canceled => Self::Canceled,
            OrderState::Expired => Self::Expired,
            OrderState::Failed => Self::Failed,
            OrderState::Hibernate => Self::Hibernate,
        }
    }
}

impl TryFrom<proto_trading::OrderState> for OrderState {
    type Error = ConvError;

    fn try_from(p: proto_trading::OrderState) -> Result<Self, Self::Error> {
        match p {
            proto_trading::OrderState::Pending => Ok(Self::Pending),
            proto_trading::OrderState::Active => Ok(Self::Active),
            proto_trading::OrderState::Filled => Ok(Self::Filled),
            proto_trading::OrderState::Canceled => Ok(Self::Canceled),
            proto_trading::OrderState::Expired => Ok(Self::Expired),
            proto_trading::OrderState::Failed => Ok(Self::Failed),
            proto_trading::OrderState::Hibernate => Ok(Self::Hibernate),
            proto_trading::OrderState::Unspecified => Err(ConvError::UnknownEnum {
                field: "OrderState",
                value: p as i32,
            }),
        }
    }
}

impl From<StateReason> for proto_trading::order_detail::state_detail::StateReason {
    fn from(r: StateReason) -> Self {
        use proto_trading::order_detail::state_detail::StateReason as P;
        match r {
            StateReason::Add => P::Add,
            StateReason::Modify => P::Modify,
            StateReason::Delete => P::Delete,
            StateReason::Deactivate => P::Deactivate,
            StateReason::Reject => P::Reject,
            StateReason::FullExecution => P::FullExecution,
            StateReason::PartialExecution => P::PartialExecution,
            StateReason::IcebergSliceAdd => P::IcebergSliceAdd,
            StateReason::ValidationFail => P::ValidationFail,
            StateReason::UnknownState => P::UnknownState,
            StateReason::QuoteAdd => P::QuoteAdd,
            StateReason::QuoteFullExecution => P::QuoteFullExecution,
            StateReason::QuotePartialExecution => P::QuotePartialExecution,
        }
    }
}

impl TryFrom<proto_trading::order_detail::state_detail::StateReason> for StateReason {
    type Error = ConvError;

    fn try_from(
        p: proto_trading::order_detail::state_detail::StateReason,
    ) -> Result<Self, Self::Error> {
        use proto_trading::order_detail::state_detail::StateReason as P;
        match p {
            P::Add => Ok(Self::Add),
            P::Modify => Ok(Self::Modify),
            P::Delete => Ok(Self::Delete),
            P::Deactivate => Ok(Self::Deactivate),
            P::Reject => Ok(Self::Reject),
            P::FullExecution => Ok(Self::FullExecution),
            P::PartialExecution => Ok(Self::PartialExecution),
            P::IcebergSliceAdd => Ok(Self::IcebergSliceAdd),
            P::ValidationFail => Ok(Self::ValidationFail),
            P::UnknownState => Ok(Self::UnknownState),
            P::QuoteAdd => Ok(Self::QuoteAdd),
            P::QuoteFullExecution => Ok(Self::QuoteFullExecution),
            P::QuotePartialExecution => Ok(Self::QuotePartialExecution),
            P::Unspecified => Err(ConvError::UnknownEnum {
                field: "StateReason",
                value: p as i32,
            }),
        }
    }
}

impl From<MarketActor> for proto_trading::order_detail::state_detail::MarketActor {
    fn from(a: MarketActor) -> Self {
        use proto_trading::order_detail::state_detail::MarketActor as P;
        match a {
            MarketActor::User => P::User,
            MarketActor::MarketOperator => P::MarketOperator,
            MarketActor::System => P::System,
        }
    }
}

impl TryFrom<proto_trading::order_detail::state_detail::MarketActor> for MarketActor {
    type Error = ConvError;

    fn try_from(
        p: proto_trading::order_detail::state_detail::MarketActor,
    ) -> Result<Self, Self::Error> {
        use proto_trading::order_detail::state_detail::MarketActor as P;
        match p {
            P::User => Ok(Self::User),
            P::MarketOperator => Ok(Self::MarketOperator),
            P::System => Ok(Self::System),
            P::Unspecified => Err(ConvError::UnknownEnum {
                field: "MarketActor",
                value: p as i32,
            }),
        }
    }
}

impl From<TradeState> for proto_trading::TradeState {
    fn from(s: TradeState) -> Self {
        match s {
            TradeState::Active => Self::Active,
            TradeState::CancelRequested => Self::CancelRequested,
            TradeState::CancelRejected => Self::CancelRejected,
            TradeState::Canceled => Self::Canceled,
            TradeState::Recalled => Self::Recalled,
            TradeState::RecallRequested => Self::RecallRequested,
            TradeState::RecallRejected => Self::RecallRejected,
            TradeState::ApprovalRequested => Self::ApprovalRequested,
        }
    }
}

impl TryFrom<proto_trading::TradeState> for TradeState {
    type Error = ConvError;

    fn try_from(p: proto_trading::TradeState) -> Result<Self, Self::Error> {
        match p {
            proto_trading::TradeState::Active => Ok(Self::Active),
            proto_trading::TradeState::CancelRequested => Ok(Self::CancelRequested),
            proto_trading::TradeState::CancelRejected => Ok(Self::CancelRejected),
            proto_trading::TradeState::Canceled => Ok(Self::Canceled),
            proto_trading::TradeState::Recalled => Ok(Self::Recalled),
            proto_trading::TradeState::RecallRequested => Ok(Self::RecallRequested),
            proto_trading::TradeState::RecallRejected => Ok(Self::RecallRejected),
            proto_trading::TradeState::ApprovalRequested => Ok(Self::ApprovalRequested),
            proto_trading::TradeState::Unspecified => Err(ConvError::UnknownEnum {
                field: "TradeState",
                value: p as i32,
            }),
        }
    }
}

impl From<Currency> for proto_market::price::Currency {
    fn from(c: Currency) -> Self {
        match c {
            Currency::Eur => Self::Eur,
            Currency::Usd => Self::Usd,
            Currency::Gbp => Self::Gbp,
            Currency::Chf => Self::Chf,
        }
    }
}

impl TryFrom<proto_market::price::Currency> for Currency {
    type Error = ConvError;

    fn try_from(p: proto_market::price::Currency) -> Result<Self, Self::Error> {
        match p {
            proto_market::price::Currency::Eur => Ok(Self::Eur),
            proto_market::price::Currency::Usd => Ok(Self::Usd),
            proto_market::price::Currency::Gbp => Ok(Self::Gbp),
            proto_market::price::Currency::Chf => Ok(Self::Chf),
            // CAD/CNY/JPY/AUD/NZD/SGD are valid proto values but the
            // sim doesn't model them yet; treat as unknown so the
            // server returns InvalidArgument rather than silently
            // mismapping.
            other => Err(ConvError::UnknownEnum {
                field: "Currency",
                value: other as i32,
            }),
        }
    }
}

impl From<CodeType> for proto_grid::EnergyMarketCodeType {
    fn from(c: CodeType) -> Self {
        match c {
            CodeType::EuropeEic => Self::EuropeEic,
            CodeType::UsNerc => Self::UsNerc,
        }
    }
}

impl TryFrom<proto_grid::EnergyMarketCodeType> for CodeType {
    type Error = ConvError;

    fn try_from(p: proto_grid::EnergyMarketCodeType) -> Result<Self, Self::Error> {
        match p {
            proto_grid::EnergyMarketCodeType::EuropeEic => Ok(Self::EuropeEic),
            proto_grid::EnergyMarketCodeType::UsNerc => Ok(Self::UsNerc),
            proto_grid::EnergyMarketCodeType::Unspecified => Err(ConvError::UnknownEnum {
                field: "EnergyMarketCodeType",
                value: p as i32,
            }),
        }
    }
}

impl From<DeliveryDuration> for proto_grid::DeliveryDuration {
    fn from(d: DeliveryDuration) -> Self {
        match d {
            DeliveryDuration::FiveMin => Self::DeliveryDuration5,
            DeliveryDuration::QuarterHour => Self::DeliveryDuration15,
            DeliveryDuration::HalfHour => Self::DeliveryDuration30,
            DeliveryDuration::Hour => Self::DeliveryDuration60,
        }
    }
}

impl TryFrom<proto_grid::DeliveryDuration> for DeliveryDuration {
    type Error = ConvError;

    fn try_from(p: proto_grid::DeliveryDuration) -> Result<Self, Self::Error> {
        match p {
            proto_grid::DeliveryDuration::DeliveryDuration5 => Ok(Self::FiveMin),
            proto_grid::DeliveryDuration::DeliveryDuration15 => Ok(Self::QuarterHour),
            proto_grid::DeliveryDuration::DeliveryDuration30 => Ok(Self::HalfHour),
            proto_grid::DeliveryDuration::DeliveryDuration60 => Ok(Self::Hour),
            proto_grid::DeliveryDuration::Unspecified => Err(ConvError::UnknownEnum {
                field: "DeliveryDuration",
                value: p as i32,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Common-package struct bridges. Area / DeliveryPeriod are full
// From / TryFrom pairs; Price and Power are flat helpers because the
// sim flattens Price into (Decimal, Currency) and Power into Decimal
// inside the order/trade structs.
// ---------------------------------------------------------------------------

impl From<&Area> for proto_grid::DeliveryArea {
    fn from(a: &Area) -> Self {
        Self {
            code: a.code.clone(),
            code_type: proto_grid::EnergyMarketCodeType::from(a.code_type) as i32,
        }
    }
}

impl TryFrom<&proto_grid::DeliveryArea> for Area {
    type Error = ConvError;

    fn try_from(p: &proto_grid::DeliveryArea) -> Result<Self, Self::Error> {
        Ok(Self {
            code: p.code.clone(),
            code_type: sim_enum_from_i32::<proto_grid::EnergyMarketCodeType, CodeType>(
                p.code_type,
                "EnergyMarketCodeType",
            )?,
        })
    }
}

impl From<DeliveryPeriod> for proto_grid::DeliveryPeriod {
    fn from(p: DeliveryPeriod) -> Self {
        Self {
            start: Some(timestamp_to_proto(p.start)),
            duration: proto_grid::DeliveryDuration::from(p.duration) as i32,
        }
    }
}

impl TryFrom<&proto_grid::DeliveryPeriod> for DeliveryPeriod {
    type Error = ConvError;

    fn try_from(p: &proto_grid::DeliveryPeriod) -> Result<Self, Self::Error> {
        let start = p.start.as_ref().ok_or(ConvError::MissingField("DeliveryPeriod.start"))?;
        Ok(Self {
            start: timestamp_from_proto(start)?,
            duration: sim_enum_from_i32::<proto_grid::DeliveryDuration, DeliveryDuration>(
                p.duration,
                "DeliveryDuration",
            )?,
        })
    }
}

/// `proto::market::Price` -> (amount, currency).
pub fn price_from_proto(p: &proto_market::Price) -> Result<(Decimal, Currency), ConvError> {
    let amount = p
        .amount
        .as_ref()
        .ok_or(ConvError::MissingField("Price.amount"))?;
    Ok((
        decimal_from_proto(amount)?,
        sim_enum_from_i32::<proto_market::price::Currency, Currency>(p.currency, "Currency")?,
    ))
}

/// (amount, currency) -> `proto::market::Price`.
pub fn price_to_proto(amount: Decimal, currency: Currency) -> proto_market::Price {
    proto_market::Price {
        amount: Some(decimal_to_proto(amount)),
        currency: proto_market::price::Currency::from(currency) as i32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn decimal_string_round_trip() {
        for raw in ["0", "1", "-12.345", "85.50", "0.00001"] {
            let proto = proto_types::Decimal {
                value: raw.to_string(),
            };
            let sim = decimal_from_proto(&proto).unwrap();
            let back = decimal_to_proto(sim);
            // Equality on parsed value, not on string — "85.50" -> 85.50
            // -> "85.5" is acceptable round-tripping.
            assert_eq!(Decimal::from_str(&back.value).unwrap(), sim);
        }
    }

    #[test]
    fn decimal_to_proto_preserves_significant_digits() {
        // The display formatting of normalized Decimal should match.
        assert_eq!(decimal_to_proto(dec!(85.50)).value, "85.5");
        assert_eq!(decimal_to_proto(dec!(0)).value, "0");
        assert_eq!(decimal_to_proto(dec!(-12.345)).value, "-12.345");
    }

    #[test]
    fn decimal_invalid_string_errors() {
        let p = proto_types::Decimal {
            value: "not-a-decimal".to_string(),
        };
        let err = decimal_from_proto(&p).unwrap_err();
        assert!(matches!(err, ConvError::InvalidDecimal(_)));
    }

    #[test]
    fn timestamp_round_trip() {
        let dt = Utc.with_ymd_and_hms(2026, 5, 13, 12, 34, 56).unwrap()
            + chrono::Duration::nanoseconds(789);
        let proto = timestamp_to_proto(dt);
        assert_eq!(proto.seconds, dt.timestamp());
        assert_eq!(proto.nanos, 789);
        let back = timestamp_from_proto(&proto).unwrap();
        assert_eq!(back, dt);
    }

    #[test]
    fn enum_round_trips_via_i32_helper() {
        // Every sim variant -> proto enum -> i32 -> back through the
        // i32 helper yields the same sim variant. Catches dropped
        // arms in any of the impls above.
        fn check<S, P>(s: S, field: &'static str)
        where
            S: Copy + PartialEq + std::fmt::Debug + Into<P> + TryFrom<P, Error = ConvError>,
            P: Into<i32> + TryFrom<i32> + Copy,
        {
            let p: P = s.into();
            let back: S = sim_enum_from_i32::<P, S>(p.into(), field).unwrap();
            assert_eq!(back, s);
        }

        check::<Side, proto_trading::MarketSide>(Side::Buy, "MarketSide");
        check::<Side, proto_trading::MarketSide>(Side::Sell, "MarketSide");
        for t in [
            OrderType::Limit,
            OrderType::StopLimit,
            OrderType::Iceberg,
            OrderType::Block,
            OrderType::Balance,
            OrderType::Prearranged,
            OrderType::Private,
        ] {
            check::<OrderType, proto_trading::OrderType>(t, "OrderType");
        }
        for e in [ExecutionOption::Aon, ExecutionOption::Fok, ExecutionOption::Ioc] {
            check::<ExecutionOption, proto_trading::OrderExecutionOption>(e, "OrderExecutionOption");
        }
        for s in [
            OrderState::Pending,
            OrderState::Active,
            OrderState::Filled,
            OrderState::Canceled,
            OrderState::Expired,
            OrderState::Failed,
            OrderState::Hibernate,
        ] {
            check::<OrderState, proto_trading::OrderState>(s, "OrderState");
        }
        for t in [
            TradeState::Active,
            TradeState::CancelRequested,
            TradeState::CancelRejected,
            TradeState::Canceled,
            TradeState::Recalled,
            TradeState::RecallRequested,
            TradeState::RecallRejected,
            TradeState::ApprovalRequested,
        ] {
            check::<TradeState, proto_trading::TradeState>(t, "TradeState");
        }
        for c in [Currency::Eur, Currency::Usd, Currency::Gbp, Currency::Chf] {
            check::<Currency, proto_market::price::Currency>(c, "Currency");
        }
        for c in [CodeType::EuropeEic, CodeType::UsNerc] {
            check::<CodeType, proto_grid::EnergyMarketCodeType>(c, "EnergyMarketCodeType");
        }
        for d in [
            DeliveryDuration::FiveMin,
            DeliveryDuration::QuarterHour,
            DeliveryDuration::HalfHour,
            DeliveryDuration::Hour,
        ] {
            check::<DeliveryDuration, proto_grid::DeliveryDuration>(d, "DeliveryDuration");
        }
    }

    #[test]
    fn unspecified_proto_enum_errors() {
        let err = Side::try_from(proto_trading::MarketSide::Unspecified).unwrap_err();
        assert!(matches!(err, ConvError::UnknownEnum { field: "MarketSide", .. }));
        let err = OrderState::try_from(proto_trading::OrderState::Unspecified).unwrap_err();
        assert!(matches!(err, ConvError::UnknownEnum { field: "OrderState", .. }));
    }

    #[test]
    fn unmodelled_currency_errors() {
        // CAD is in the proto but not in the sim's Currency.
        let err = Currency::try_from(proto_market::price::Currency::Cad).unwrap_err();
        assert!(matches!(err, ConvError::UnknownEnum { field: "Currency", .. }));
    }

    #[test]
    fn area_round_trip() {
        let sim = Area::eic("10Y1001A1001A82H");
        let proto: proto_grid::DeliveryArea = (&sim).into();
        assert_eq!(proto.code, "10Y1001A1001A82H");
        assert_eq!(
            proto.code_type,
            proto_grid::EnergyMarketCodeType::EuropeEic as i32
        );
        let back = Area::try_from(&proto).unwrap();
        assert_eq!(back, sim);
    }

    #[test]
    fn area_rejects_unspecified_code_type() {
        let bad = proto_grid::DeliveryArea {
            code: "X".into(),
            code_type: 0,
        };
        let err = Area::try_from(&bad).unwrap_err();
        assert!(matches!(err, ConvError::UnknownEnum { field: "EnergyMarketCodeType", .. }));
    }

    #[test]
    fn delivery_period_round_trip() {
        let sim = DeliveryPeriod {
            start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            duration: DeliveryDuration::QuarterHour,
        };
        let proto: proto_grid::DeliveryPeriod = sim.into();
        let back = DeliveryPeriod::try_from(&proto).unwrap();
        assert_eq!(back, sim);
    }

    #[test]
    fn delivery_period_missing_start_errors() {
        let bad = proto_grid::DeliveryPeriod {
            start: None,
            duration: proto_grid::DeliveryDuration::DeliveryDuration60 as i32,
        };
        let err = DeliveryPeriod::try_from(&bad).unwrap_err();
        assert_eq!(err, ConvError::MissingField("DeliveryPeriod.start"));
    }

    #[test]
    fn price_helpers_round_trip() {
        let proto = price_to_proto(dec!(85.50), Currency::Eur);
        let (amount, currency) = price_from_proto(&proto).unwrap();
        assert_eq!(amount, dec!(85.5));
        assert_eq!(currency, Currency::Eur);
    }

    #[test]
    fn price_missing_amount_errors() {
        let bad = proto_market::Price {
            amount: None,
            currency: proto_market::price::Currency::Eur as i32,
        };
        assert_eq!(
            price_from_proto(&bad).unwrap_err(),
            ConvError::MissingField("Price.amount")
        );
    }

    #[test]
    fn power_helpers_round_trip() {
        let proto = power_to_proto(dec!(1.5));
        assert_eq!(power_from_proto(&proto).unwrap(), dec!(1.5));
    }

    #[test]
    fn timestamp_rejects_invalid_nanos() {
        let bad_negative = prost_types::Timestamp { seconds: 0, nanos: -1 };
        let bad_overflow = prost_types::Timestamp {
            seconds: 0,
            nanos: 1_000_000_000,
        };
        assert_eq!(
            timestamp_from_proto(&bad_negative).unwrap_err(),
            ConvError::InvalidTimestamp
        );
        assert_eq!(
            timestamp_from_proto(&bad_overflow).unwrap_err(),
            ConvError::InvalidTimestamp
        );
    }
}
