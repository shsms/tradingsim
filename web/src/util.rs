//! Shared helpers across panels: EIC area registry, RFC-3339 slicing.

/// Whether an area belongs to the home market (always shown) or
/// the neighbouring zones (toggle-controlled in the filter bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaGroup {
    Home,
    Neighbour,
}

/// One configured area — EIC code + short tag for badges + group.
#[derive(Debug, Clone, Copy)]
pub struct AreaSpec {
    pub code: &'static str,
    pub tag: &'static str,
    pub group: AreaGroup,
}

/// All areas the UI knows about. Order matches the JS UI: home
/// zones first, then neighbours.
pub const ALL_AREAS: &[AreaSpec] = &[
    AreaSpec {
        code: "10YDE-EON------1",
        tag: "TN",
        group: AreaGroup::Home,
    },
    AreaSpec {
        code: "10YDE-RWENET---I",
        tag: "AM",
        group: AreaGroup::Home,
    },
    AreaSpec {
        code: "10YDE-VE-------2",
        tag: "HZ",
        group: AreaGroup::Home,
    },
    AreaSpec {
        code: "10YDE-ENBW-----N",
        tag: "BW",
        group: AreaGroup::Home,
    },
    AreaSpec {
        code: "10YFR-RTE------C",
        tag: "FR",
        group: AreaGroup::Neighbour,
    },
    AreaSpec {
        code: "10YNL----------L",
        tag: "NL",
        group: AreaGroup::Neighbour,
    },
    AreaSpec {
        code: "10YBE----------2",
        tag: "BE",
        group: AreaGroup::Neighbour,
    },
    AreaSpec {
        code: "10YAT-APG------L",
        tag: "AT",
        group: AreaGroup::Neighbour,
    },
];

/// Short tag for an EIC area code — `10YDE-EON------1` → `TN`, etc.
pub fn area_tag(code: &str) -> &'static str {
    ALL_AREAS
        .iter()
        .find(|a| a.code == code)
        .map(|a| a.tag)
        .unwrap_or("?")
}

/// Strip the proto's SCREAMING_SNAKE prefixes for display. Unknown
/// variants fall through unchanged so a future server addition shows
/// up legibly rather than blank.
pub fn short_side(s: &str) -> &str {
    match s {
        "MARKET_SIDE_BUY" => "buy",
        "MARKET_SIDE_SELL" => "sell",
        other => other,
    }
}

pub fn short_order_state(s: &str) -> &str {
    match s {
        "ORDER_STATE_PENDING" => "pending",
        "ORDER_STATE_ACTIVE" => "active",
        "ORDER_STATE_HIBERNATE" => "hibernate",
        "ORDER_STATE_FILLED" => "filled",
        "ORDER_STATE_CANCELED" => "canceled",
        "ORDER_STATE_EXPIRED" => "expired",
        "ORDER_STATE_FAILED" => "failed",
        other => other,
    }
}

pub fn short_trade_state(s: &str) -> &str {
    match s {
        "TRADE_STATE_ACTIVE" => "active",
        "TRADE_STATE_CANCEL_REQUESTED" => "cancel?",
        "TRADE_STATE_CANCEL_REJECTED" => "cancel✗",
        "TRADE_STATE_CANCELED" => "canceled",
        "TRADE_STATE_RECALL_REQUESTED" => "recall?",
        "TRADE_STATE_RECALL_REJECTED" => "recall✗",
        "TRADE_STATE_RECALLED" => "recalled",
        "TRADE_STATE_APPROVAL_REQUESTED" => "approval?",
        other => other,
    }
}
