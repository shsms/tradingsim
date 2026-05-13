//! Headless tradingsim simulator. Phase 1: parses the config path and
//! logs; the actual ElectricityTradingService gRPC server, World tick,
//! and UI are wired in over the next phases.

use std::path::PathBuf;

use simplelog::{
    ColorChoice, Config as LogConfig, LevelFilter, TermLogger, TerminalMode,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    TermLogger::init(
        LevelFilter::Info,
        LogConfig::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.lisp".to_string());
    let cfg_path = PathBuf::from(cfg_path);

    log::info!("tradingsim v{} starting", env!("CARGO_PKG_VERSION"));
    log::info!("config path: {}", cfg_path.display());
    log::warn!("server + world not yet wired — exiting (Phase 1 skeleton)");
}
