//! Shared helpers across panels: EIC area tags, RFC-3339 slicing.

/// Short tag for an EIC area code — `10YDE-EON------1` → `TN`, etc.
/// Mirrors the JS UI's ALL_AREAS table.
pub fn area_tag(code: &str) -> &'static str {
    match code {
        "10YDE-EON------1" => "TN",
        "10YDE-RWENET---I" => "AM",
        "10YDE-VE-------2" => "HZ",
        "10YDE-ENBW-----N" => "BW",
        "10YFR-RTE------C" => "FR",
        "10YNL----------L" => "NL",
        "10YBE----------2" => "BE",
        "10YAT-APG------L" => "AT",
        _ => "?",
    }
}

/// Slice HH:MM out of an RFC-3339 UTC timestamp. The JS UI uses
/// `Intl.DateTimeFormat` keyed on the configured sim tz; that
/// arrives with the pulse-bar commit. Until then, prints carry
/// the wire-side UTC time.
pub fn short_time_utc(iso: &str) -> String {
    iso.get(11..16).unwrap_or("--:--").to_string()
}

pub fn short_time_sec_utc(iso: &str) -> String {
    iso.get(11..19).unwrap_or("--:--:--").to_string()
}
