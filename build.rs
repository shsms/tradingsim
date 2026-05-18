//! Host crate build script: drives both the protobuf codegen and
//! the Leptos/WASM browser bundle.
//!
//! Proto: the electricity_trading and weather protos both pull in
//! frequenz-api-common; we use the electricity_trading submodule's
//! pinned common (location.proto matches byte-for-byte across both
//! submodule revisions).
//!
//! Web bundle: src/ui/mod.rs rust-embeds web/dist/, the output of
//! `trunk build` on the web/ subcrate (a Leptos SPA compiled to
//! WebAssembly). We invoke trunk here so the bundle is always in
//! sync with the host binary — no separate `trunk build` step in
//! the developer's workflow. `trunk` must be on PATH; install with
//! `cargo install --locked trunk`.

use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), std::io::Error> {
    build_web_bundle()?;
    compile_protos()
}

fn build_web_bundle() -> Result<(), std::io::Error> {
    // Re-run trunk whenever the web crate's inputs change. The
    // host crate's src/ already triggers a host-only rerun via
    // cargo's default behaviour; we only need to add web/ here.
    for path in [
        "web/src",
        "web/index.html",
        "web/style.css",
        "web/Cargo.toml",
        "web/Trunk.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let mut cmd = Command::new("trunk");
    cmd.arg("build").current_dir("web");
    // Trunk shells out to `cargo build --target=wasm32-unknown-unknown`.
    // Without a distinct CARGO_TARGET_DIR that subprocess would
    // contend with the outer cargo (which is currently holding the
    // workspace target-dir lock to run *this* build script) and
    // deadlock. web/target/ keeps trunk's wasm artefacts off the
    // host's tree.
    cmd.env("CARGO_TARGET_DIR", "target");
    if release {
        cmd.arg("--release");
    }

    let status = cmd.status().map_err(|e| {
        std::io::Error::other(format!(
            "could not invoke `trunk` ({e}). Install with `cargo install --locked trunk`."
        ))
    })?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "`trunk build` in web/ exited with {status}. If the failure mentions `wasm32-unknown-unknown`, install the target with `rustup target add wasm32-unknown-unknown` (rust-toolchain.toml should auto-install it under rustup; non-rustup setups need this manually)."
        )));
    }
    Ok(())
}

fn compile_protos() -> Result<(), std::io::Error> {
    let trading_root = std::env::var("TRADINGSIM_PROTO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("submodules/frequenz-api-electricity-trading"));
    let weather_root = PathBuf::from("submodules/frequenz-api-weather");

    let trading_proto =
        trading_root.join("proto/frequenz/api/electricity_trading/v1/electricity_trading.proto");
    let weather_proto = weather_root.join("proto/frequenz/api/weather/v1/weather.proto");
    let common_root = trading_root.join("submodules/frequenz-api-common/proto");

    println!("cargo:rerun-if-env-changed=TRADINGSIM_PROTO_ROOT");
    println!("cargo:rerun-if-changed={}", trading_proto.display());
    println!("cargo:rerun-if-changed={}", weather_proto.display());

    tonic_prost_build::configure()
        .disable_comments(["."])
        .include_file("proto_v1.rs")
        .compile_well_known_types(false)
        .compile_protos(
            &[trading_proto.as_path(), weather_proto.as_path()],
            &[
                trading_root.join("proto").as_path(),
                weather_root.join("proto").as_path(),
                common_root.as_path(),
            ],
        )
        .inspect_err(|e| eprintln!("Could not compile protobuf files. Error: {e:?}"))
}
