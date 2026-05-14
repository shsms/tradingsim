//! tonic-prost-build wiring. The electricity_trading and weather
//! protos both pull in frequenz-api-common; we use the
//! electricity_trading's pinned common (location.proto matches
//! byte-for-byte across both submodule revisions).

use std::path::PathBuf;

fn main() -> Result<(), std::io::Error> {
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
