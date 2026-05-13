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

use crate::proto::common::types as proto_types;

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
