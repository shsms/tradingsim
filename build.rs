//! tonic-prost-build wiring. The electricity_trading.proto pulls in
//! frequenz-api-common via the nested git submodule; both roots are
//! passed as include paths.

use std::path::PathBuf;

fn main() -> Result<(), std::io::Error> {
    let proto_root = std::env::var("TRADINGSIM_PROTO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("submodules/frequenz-api-electricity-trading"));

    let api_proto =
        proto_root.join("proto/frequenz/api/electricity_trading/v1/electricity_trading.proto");
    let common_proto_root = proto_root.join("submodules/frequenz-api-common/proto");

    println!("cargo:rerun-if-env-changed=TRADINGSIM_PROTO_ROOT");
    println!("cargo:rerun-if-changed={}", api_proto.display());

    tonic_prost_build::configure()
        .disable_comments(["."])
        .include_file("proto_v1.rs")
        .compile_well_known_types(false)
        .compile_protos(
            &[api_proto.as_path()],
            &[
                proto_root.join("proto").as_path(),
                common_proto_root.as_path(),
            ],
        )
        .inspect_err(|e| eprintln!("Could not compile protobuf files. Error: {e:?}"))
}
