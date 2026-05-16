//! One module per panel. Re-exported flatly so the Shell can
//! reach them as `panels::PublicTrades` without naming the file.

pub mod chart;
pub mod filter_bar;
pub mod gridpools;
pub mod public_trades;
pub mod pulse;
pub mod scenarios;
pub mod weather;

pub use chart::PriceChart;
pub use filter_bar::{FilterBar, load_filter};
pub use gridpools::Gridpools;
pub use public_trades::{PublicTrades, TRADES_BUFFER_CAP};
pub use pulse::{PulseBar, SparkState, TzMode, load_tz_mode};
pub use scenarios::Scenarios;
pub use weather::Weather;
