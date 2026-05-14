//! Simulation clock. Owns a [`chrono_tz::Tz`] the physics layer
//! (solar elevation, duck curve, temperature / wind cycles) and the
//! scenario tick interpret UTC instants through. The gRPC wire
//! boundary stays UTC; everything below it reasons in local civil
//! time so a "13:00 belly" stage actually fires at 13:00 in the
//! configured zone (Europe/Berlin by default, matching DE-LU).
//!
//! The tz is mutable behind an [`RwLock`] so `config.lisp` can call
//! `(set-timezone "Europe/Berlin")` to redirect a sim aimed at a
//! different bidding zone (FR, NL, AT, …) without recompiling.

use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use parking_lot::RwLock;

/// Default zone for the canonical DE-LU sim. CET in winter, CEST in
/// summer; DST transitions follow the IANA database.
pub const DEFAULT_TZ: Tz = chrono_tz::Europe::Berlin;

#[derive(Clone, Debug)]
pub struct Clock {
    pub tz: Tz,
}

impl Clock {
    pub fn new(tz: Tz) -> Self {
        Self { tz }
    }

    /// Local civil hour-of-day (0.0–24.0) at `utc`. Subhour precision
    /// matches the existing `wallclock_hour` helper this replaces,
    /// so scenario stage matches and duck-curve lookups keep their
    /// per-second resolution.
    pub fn local_hour(&self, utc: DateTime<Utc>) -> f64 {
        let l = utc.with_timezone(&self.tz);
        l.hour() as f64 + l.minute() as f64 / 60.0 + l.second() as f64 / 3600.0
    }

    /// Local civil calendar date for `utc`. The solar-elevation
    /// model reads day-of-year off this, so a "summer evening" stage
    /// running at 23:00 local on June 30 doesn't silently shift to
    /// July 1's day-of-year because UTC has already rolled over.
    pub fn local_date(&self, utc: DateTime<Utc>) -> NaiveDate {
        utc.with_timezone(&self.tz).date_naive()
    }

    pub fn local_day_of_year(&self, utc: DateTime<Utc>) -> u32 {
        self.local_date(utc).ordinal()
    }

    /// IANA name, e.g. "Europe/Berlin". UI fetches this to format
    /// timestamps in the same zone the physics is using.
    pub fn tz_name(&self) -> &'static str {
        self.tz.name()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(DEFAULT_TZ)
    }
}

pub type SharedClock = Arc<RwLock<Clock>>;

pub fn new_clock() -> SharedClock {
    Arc::new(RwLock::new(Clock::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn local_hour_applies_summer_dst_offset() {
        // 2026-05-14 12:00 UTC = 14:00 CEST (UTC+2)
        let utc = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let clock = Clock::default();
        assert!((clock.local_hour(utc) - 14.0).abs() < 1e-9);
    }

    #[test]
    fn local_hour_applies_winter_standard_offset() {
        // 2026-01-15 12:00 UTC = 13:00 CET (UTC+1)
        let utc = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let clock = Clock::default();
        assert!((clock.local_hour(utc) - 13.0).abs() < 1e-9);
    }

    #[test]
    fn local_date_can_differ_from_utc_date() {
        // 2026-05-14 23:30 CEST = 21:30 UTC same day; but 2026-05-15
        // 00:30 CEST = 22:30 UTC previous day → local date is
        // 05-15 even though utc is 05-14.
        let utc = Utc.with_ymd_and_hms(2026, 5, 14, 22, 30, 0).unwrap();
        let clock = Clock::default();
        assert_eq!(clock.local_date(utc).day(), 15);
    }

    #[test]
    fn alternate_tz_parses() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let clock = Clock::new(tz);
        // 2026-05-14 16:00 UTC = 12:00 EDT
        let utc = Utc.with_ymd_and_hms(2026, 5, 14, 16, 0, 0).unwrap();
        assert!((clock.local_hour(utc) - 12.0).abs() < 1e-9);
    }
}
