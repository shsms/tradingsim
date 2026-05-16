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
