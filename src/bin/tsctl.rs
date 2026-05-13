//! Client CLI for tradingsim. Phase 1: argument scaffolding only;
//! sub-command handlers fill in alongside the matching server RPCs.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tsctl", version, about = "tradingsim client")]
struct Cli {
    /// gRPC endpoint for the tradingsim server.
    #[arg(long, default_value = "http://[::1]:8810")]
    addr: String,
}

fn main() {
    let cli = Cli::parse();
    eprintln!("tsctl v{} (Phase 1 skeleton)", env!("CARGO_PKG_VERSION"));
    eprintln!("would connect to: {}", cli.addr);
}
