//! Build script for `aerosync-proto`: invokes `prost-build` to compile
//! `proto/aerosync/wire/v1.proto` into Rust types under `OUT_DIR`.
//!
//! We use a vendored `protoc` (via `protoc-bin-vendored`) so contributors
//! never have to install Protocol Buffers system-wide. We also map
//! `google.protobuf.Timestamp` to `::prost_types::Timestamp` so the
//! generated code re-uses the upstream prost-types implementation
//! instead of regenerating a private copy.
//!
//! `cargo:rerun-if-changed` is set on the proto source AND on this build
//! script itself, so editing either triggers a rebuild.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let proto_root = manifest_dir.join("proto");
    let proto_file = proto_root.join("aerosync").join("wire").join("v1.proto");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", proto_file.display());

    // Point prost-build at the bundled `protoc`. Doing this every build
    // is harmless (it's just an env var on this process).
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("vendored protoc binary should be available for current target");
    std::env::set_var("PROTOC", &protoc);

    // Vendored include path provides the `google/protobuf/*.proto`
    // descriptor files referenced by `import` statements in our schema.
    let well_known_include = protoc_bin_vendored::include_path()
        .expect("vendored protoc include path should be available");

    let mut config = prost_build::Config::new();

    // Map every protobuf `bytes` field to `bytes::Bytes` instead of
    // `Vec<u8>`. This avoids needless allocation when frames are
    // forwarded between async tasks (Bytes is Arc-backed and cheap to
    // clone).
    config.bytes(["."]);

    // Use upstream prost-types for the well-known `Timestamp` so we
    // don't ship two copies of the same struct.
    config.extern_path(".google.protobuf.Timestamp", "::prost_types::Timestamp");

    // ControlFrame.body is a oneof whose largest variant
    // (TransferStart, post-RFC-003) now embeds the Metadata envelope.
    // The size delta vs other variants (Heartbeat, Cancel) trips
    // clippy::large_enum_variant. Boxing inside generated code is not
    // free for prost users (forces double-deref on every access) and
    // ControlFrame is only ever moved between async tasks, where the
    // size cost is dominated by the payload itself. Annotate the
    // generated oneof to silence the lint cleanly.
    config.type_attribute(
        ".aerosync.wire.v1.ControlFrame.body",
        "#[allow(clippy::large_enum_variant)]",
    );

    config
        .compile_protos(&[proto_file], &[proto_root, well_known_include])
        .expect("failed to compile aerosync.wire.v1 protobuf schema");
}
