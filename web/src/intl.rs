//! Sim-tz aware time formatting via JS `Intl.DateTimeFormat` (called
//! through `Date.toLocaleTimeString` with options). All panels that
//! display a wallclock or a contract delivery time should route
//! through here so the local / UTC toggle stays a single switch.

use js_sys::{Date, Object, Reflect};
use wasm_bindgen::JsValue;

const LOCALE: &str = "en-GB";

fn opts(tz: &str, with_secs: bool) -> Object {
    let o = Object::new();
    let _ = Reflect::set(&o, &"timeZone".into(), &tz.into());
    let _ = Reflect::set(&o, &"hour".into(), &"2-digit".into());
    let _ = Reflect::set(&o, &"minute".into(), &"2-digit".into());
    if with_secs {
        let _ = Reflect::set(&o, &"second".into(), &"2-digit".into());
    }
    let _ = Reflect::set(&o, &"hour12".into(), &JsValue::FALSE);
    o
}

/// Format an ISO-8601 timestamp as `HH:MM` in the given IANA tz.
/// Invalid input returns `--:--`.
pub fn short_time(iso: &str, tz: &str) -> String {
    let d = Date::new(&JsValue::from_str(iso));
    if d.get_time().is_nan() {
        return "--:--".into();
    }
    String::from(d.to_locale_time_string_with_options(LOCALE, &opts(tz, false)))
}

/// Format an ISO-8601 timestamp as `HH:MM:SS` in the given IANA tz.
/// Invalid input returns `--:--:--`.
pub fn short_time_sec(iso: &str, tz: &str) -> String {
    let d = Date::new(&JsValue::from_str(iso));
    if d.get_time().is_nan() {
        return "--:--:--".into();
    }
    String::from(d.to_locale_time_string_with_options(LOCALE, &opts(tz, true)))
}

/// Wallclock `HH:MM:SS` for the current instant in the given tz.
pub fn now_hms(tz: &str) -> String {
    let d = Date::new_0();
    String::from(d.to_locale_time_string_with_options(LOCALE, &opts(tz, true)))
}

/// Current hour-of-day in the given tz, as a float (e.g. 14h30m → 14.5).
/// Used by the scenarios timeline to anchor the "now" marker against
/// stage hour_from/hour_to (which are sim-local).
pub fn now_hour_in_tz(tz: &str) -> f64 {
    let hms = now_hms(tz);
    let mut parts = hms.split(':');
    let h: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let m: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let s: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    h + m / 60.0 + s / 3600.0
}

/// Short zone tag (`CEST`, `CET`, …) for the *current* instant in the
/// given tz. Used as the suffix on the wallclock display so the user
/// can tell at a glance whether they're looking at 14:00 CEST vs UTC.
pub fn zone_label(tz: &str) -> String {
    let d = Date::new_0();
    let o = Object::new();
    let _ = Reflect::set(&o, &"timeZone".into(), &tz.into());
    let _ = Reflect::set(&o, &"timeZoneName".into(), &"short".into());
    let _ = Reflect::set(&o, &"hour".into(), &"2-digit".into());
    let parts: JsValue = d.to_locale_time_string_with_options(LOCALE, &o).into();
    // `to_locale_time_string` returns e.g. "21:30 CEST"; pluck the
    // last whitespace-separated token. Falling back to the input tz
    // leaves the display sensible if Intl ever surprises us.
    let s = parts.as_string().unwrap_or_default();
    s.split_whitespace()
        .next_back()
        .map(str::to_string)
        .unwrap_or_else(|| tz.to_string())
}
