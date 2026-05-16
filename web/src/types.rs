//! Wire types the Leptos shell deserializes from the host server's
//! /api/* endpoints. Shapes mirror `src/ui/mod.rs`'s response structs
//! — kept in sync by hand for now; a shared `tradingsim-api` crate
//! is the natural extraction once the port settles.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct InfoResp {
    pub version: String,
    pub gridpools: usize,
    pub markets: usize,
    pub couplings: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClockResp {
    /// IANA timezone the sim runs in (e.g. `Europe/Berlin`). The
    /// header's "local" / UTC toggle keys off this so a remote
    /// operator still sees the simulator's home zone.
    pub tz: String,
}

/// One print off the /ws/public-trades broadcast. Prices + quantities
/// arrive as the host's `Decimal::to_string()` output; parsed lazily
/// only when a panel needs the numeric value. Fields beyond `id` /
/// `price` are unused until the trades / chart panels port — keep
/// them deserialized so the wire shape stays a single source of
/// truth.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PublicTrade {
    pub id: u64,
    pub buy_area: String,
    pub sell_area: String,
    pub period: String,
    pub price: String,
    pub quantity: String,
    pub execution_time: String,
}
