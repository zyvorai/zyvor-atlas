// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Compile the gRPC proto. Requires `protoc` (system package `protobuf-compiler`) at build time —
//! installed in the Docker builder stages and present on dev machines.
fn main() {
    println!("cargo:rerun-if-changed=proto/atlas.proto");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    tonic_prost_build::configure()
        .file_descriptor_set_path(out.join("atlas_descriptor.bin"))
        .compile_protos(&["proto/atlas.proto"], &["proto"])
        .expect("compile atlas.proto");

    // Ensure `ui/dist/index.html` exists so `rust-embed` compiles even without a UI build.
    // `make ui` / the Docker node stage produce the real bundle, which overwrites this placeholder.
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        let _ = std::fs::create_dir_all(&dist);
        let _ = std::fs::write(
            &index,
            "<!doctype html><meta charset=\"utf-8\"><title>Atlas Storage Center</title>\
             <body style=\"font-family:system-ui;background:#070B14;color:#e6edf6;padding:48px\">\
             <h2>Atlas Storage Center</h2><p>The web UI has not been built. \
             Run <code>make ui</code> (or build the Docker image) to bundle it.</p></body>",
        );
    }
    println!("cargo:rerun-if-changed=ui/dist");
}
