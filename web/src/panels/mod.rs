//! One module per panel. Re-exported flatly so the Shell can
//! reach them as `panels::PublicTrades` without naming the file.

pub mod public_trades;
pub mod scenarios;
pub mod weather;

pub use public_trades::{PublicTrades, TRADES_BUFFER_CAP};
pub use scenarios::Scenarios;
pub use weather::Weather;
