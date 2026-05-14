//! Market identity + per-area rules. Independent of order/trade state
//! — the matcher reads `MarketRules` for validation, but never mutates
//! it. All knobs are settable from lisp via `(%make-market …)` (next
//! phase).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::sim::decimal::{DEFAULT_PRICE_TICK, DEFAULT_QTY_STEP};

// Both enums come straight from the proto — duplicating them in sim
// only added boilerplate. `Currency` brings in some variants we don't
// support yet (CAD/CNY/JPY/AUD/NZD/SGD); admit-time validation gates
// those out per market via `MarketRules.currency`. `CodeType`'s
// `Unspecified` variant is never constructed by the sim.
pub use crate::proto::common::grid::{DeliveryDuration, EnergyMarketCodeType as CodeType};
pub use crate::proto::common::market::price::Currency;

/// A delivery area, identified by its market code (e.g. EIC). Cheap
/// to clone — the inner string is short and rarely changes.
#[derive(Clone, Debug)]
pub struct Area {
    pub code: String,
    pub code_type: CodeType,
}

impl Area {
    pub fn eic(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            code_type: CodeType::EuropeEic,
        }
    }
}

impl PartialEq for Area {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.code_type == other.code_type
    }
}
impl Eq for Area {}
impl std::hash::Hash for Area {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.code.hash(state);
        (self.code_type as i32).hash(state);
    }
}

impl DeliveryDuration {
    /// Length in minutes. Unspecified panics — it shouldn't reach
    /// the alignment math; admit validation rejects it first.
    pub fn as_minutes(self) -> i64 {
        match self {
            Self::DeliveryDuration5 => 5,
            Self::DeliveryDuration15 => 15,
            Self::DeliveryDuration30 => 30,
            Self::DeliveryDuration60 => 60,
            Self::Unspecified => panic!("DeliveryDuration::Unspecified has no minute length"),
        }
    }

    pub fn from_minutes(m: i64) -> Option<Self> {
        match m {
            5 => Some(Self::DeliveryDuration5),
            15 => Some(Self::DeliveryDuration15),
            30 => Some(Self::DeliveryDuration30),
            60 => Some(Self::DeliveryDuration60),
            _ => None,
        }
    }
}

/// (start, duration) — what an Order or Trade is for. Start must align
/// to the duration grid (see `is_aligned`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeliveryPeriod {
    pub start: DateTime<Utc>,
    pub duration: DeliveryDuration,
}

impl DeliveryPeriod {
    /// True iff `start` lies on a multiple-of-duration minute boundary
    /// past the hour. The proto comment specifies exactly this
    /// alignment for each duration.
    pub fn is_aligned(&self) -> bool {
        let m = self.start.timestamp() / 60;
        m % self.duration.as_minutes() == 0 && self.start.timestamp() % 60 == 0
    }

    pub fn end(&self) -> DateTime<Utc> {
        self.start + chrono::Duration::minutes(self.duration.as_minutes())
    }
}

/// Per-area knobs. Defaults reproduce DE-LU as of 2026: 0.01
/// EUR/MWh tick, 0.1 MW step, 60-min only (the wider product mix
/// arrives in Phase 7).
#[derive(Clone, Debug)]
pub struct MarketRules {
    pub area: Area,
    pub currency: Currency,
    pub price_tick: Decimal,
    pub qty_step: Decimal,
    pub durations: Vec<DeliveryDuration>,
}

impl MarketRules {
    pub fn de_lu() -> Self {
        Self::for_area(Area::eic("10Y1001A1001A82H"), Currency::Eur)
    }

    /// Generic constructor — same defaults across EU bidding zones:
    /// 0.01 EUR/MWh tick, 0.1 MW step, 15-min delivery duration.
    /// 30/60-min and 5-min products are nice-to-have; for
    /// now any order with a non-15-min duration is rejected at
    /// admit time as UnsupportedDurationForMarket.
    pub fn for_area(area: Area, currency: Currency) -> Self {
        Self {
            area,
            currency,
            price_tick: DEFAULT_PRICE_TICK,
            qty_step: DEFAULT_QTY_STEP,
            durations: vec![DeliveryDuration::DeliveryDuration15],
        }
    }

    pub fn allows(&self, duration: DeliveryDuration) -> bool {
        self.durations.contains(&duration)
    }
}

/// Lookup table from Area to its rules. Lisp `(%make-market …)` calls
/// insert entries here; pre-trade validation calls `get`.
#[derive(Default, Clone, Debug)]
pub struct MarketRegistry {
    by_area: HashMap<Area, MarketRules>,
}

impl MarketRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, rules: MarketRules) {
        self.by_area.insert(rules.area.clone(), rules);
    }

    pub fn get(&self, area: &Area) -> Option<&MarketRules> {
        self.by_area.get(area)
    }

    pub fn len(&self) -> usize {
        self.by_area.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_area.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::dec;

    #[test]
    fn duration_minutes_roundtrip() {
        for d in [
            DeliveryDuration::DeliveryDuration5,
            DeliveryDuration::DeliveryDuration15,
            DeliveryDuration::DeliveryDuration30,
            DeliveryDuration::DeliveryDuration60,
        ] {
            assert_eq!(DeliveryDuration::from_minutes(d.as_minutes()), Some(d));
        }
        assert_eq!(DeliveryDuration::from_minutes(7), None);
    }

    #[test]
    fn period_alignment_for_each_duration() {
        let on_hour = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let on_quarter = Utc.with_ymd_and_hms(2026, 5, 13, 12, 15, 0).unwrap();
        let off_grid = Utc.with_ymd_and_hms(2026, 5, 13, 12, 7, 0).unwrap();

        assert!(
            DeliveryPeriod {
                start: on_hour,
                duration: DeliveryDuration::DeliveryDuration60
            }
            .is_aligned()
        );
        assert!(
            DeliveryPeriod {
                start: on_hour,
                duration: DeliveryDuration::DeliveryDuration15
            }
            .is_aligned()
        );
        assert!(
            DeliveryPeriod {
                start: on_quarter,
                duration: DeliveryDuration::DeliveryDuration15
            }
            .is_aligned()
        );
        assert!(
            !DeliveryPeriod {
                start: on_quarter,
                duration: DeliveryDuration::DeliveryDuration60
            }
            .is_aligned()
        );
        assert!(
            !DeliveryPeriod {
                start: off_grid,
                duration: DeliveryDuration::DeliveryDuration15
            }
            .is_aligned()
        );
    }

    #[test]
    fn period_end_adds_duration() {
        let p = DeliveryPeriod {
            start: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            duration: DeliveryDuration::DeliveryDuration15,
        };
        assert_eq!(
            p.end(),
            Utc.with_ymd_and_hms(2026, 5, 13, 12, 15, 0).unwrap()
        );
    }

    #[test]
    fn registry_round_trips_rules() {
        let mut reg = MarketRegistry::new();
        let rules = MarketRules::de_lu();
        let area = rules.area.clone();
        reg.insert(rules);
        let got = reg.get(&area).unwrap();
        assert_eq!(got.currency, Currency::Eur);
        assert_eq!(got.price_tick, dec!(0.01));
        assert!(got.allows(DeliveryDuration::DeliveryDuration15));
        assert!(!got.allows(DeliveryDuration::DeliveryDuration60));
        assert!(!got.allows(DeliveryDuration::DeliveryDuration5));
    }
}
