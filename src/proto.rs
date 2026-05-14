//! tonic-generated proto types. Generated to OUT_DIR/proto_v1.rs by
//! build.rs; re-exported here under stable Rust paths so call sites
//! don't have to know the proto package layout.

#[allow(
    clippy::doc_lazy_continuation,
    clippy::module_inception,
    dead_code,
    clippy::enum_variant_names
)]
mod pb {
    tonic::include_proto!("proto_v1");
}

pub use pb::frequenz::api::common::v1 as common_v1;
pub use pb::frequenz::api::common::v1alpha8 as common;
pub use pb::frequenz::api::electricity_trading::electricity_trading::v1 as trading;
pub use pb::frequenz::api::weather::v1 as weather;
