// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Compiles the CSI proto. Requires `protoc` (system package `protobuf-compiler`) at build time.
fn main() {
    println!("cargo:rerun-if-changed=proto/csi.proto");
    tonic_prost_build::configure()
        .compile_protos(&["proto/csi.proto"], &["proto"])
        .expect("compile csi.proto");
}
