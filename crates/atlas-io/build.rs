// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! With the `bpf` feature on Linux, compile `bpf/atlas_bio.bpf.c` into `$OUT_DIR/atlas_bio.bpf.o`
//! for `src/bpf.rs` to embed. `CLANG` overrides the compiler.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=bpf/atlas_bio.bpf.c");
    println!("cargo:rerun-if-changed=bpf/atlas_native_io.bpf.c");
    println!("cargo:rerun-if-changed=bpf/atlas_cgroup.bpf.c");
    println!("cargo:rerun-if-changed=bpf/atlas_nfs.bpf.c");
    println!("cargo:rerun-if-changed=bpf/atlas_zfs.bpf.c");
    println!("cargo:rerun-if-changed=bpf/atlas_uring.bpf.c");
    println!("cargo:rerun-if-env-changed=CLANG");
    if env::var_os("CARGO_FEATURE_BPF").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("atlas_bio.bpf.o");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let clang = env::var("CLANG").unwrap_or_else(|_| "clang".into());
    let status = Command::new(&clang)
        .args(["-O2", "-g", "-target", "bpf", "-Wall", "-Werror"])
        // <linux/types.h> pulls <asm/types.h>, which Debian/Ubuntu keep under the multiarch dir.
        .arg(format!("-I/usr/include/{arch}-linux-gnu"))
        .args(["-c", "bpf/atlas_bio.bpf.c", "-o"])
        .arg(&out)
        .status()
        .unwrap_or_else(|e| {
            panic!("run {clang} (the `bpf` feature needs clang + libbpf headers): {e}")
        });
    assert!(
        status.success(),
        "{clang} failed to compile bpf/atlas_bio.bpf.c"
    );
}
