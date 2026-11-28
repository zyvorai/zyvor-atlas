// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `atlas-native-node --config <file.json>`: runs a native storage node (see `docs/NATIVE_NODE.md`).

use std::{process::ExitCode, thread, time::Duration};

use atlas_native::node::{NativeNode, NodeConfig};

fn main() -> ExitCode {
    // With the `s3` feature reqwest also links aws-lc-rs, and rustls refuses to pick a provider
    // on its own when two are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    let path = match args.as_slice() {
        [_, flag, path] if flag == "--config" => path,
        _ => {
            eprintln!("usage: atlas-native-node --config <file.json>");
            return ExitCode::from(2);
        }
    };
    let cfg = match NodeConfig::from_file(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("atlas-native-node: invalid config {path}: {e}");
            return ExitCode::from(2);
        }
    };
    atlas_native::tls::install_cert_reload_sighup();
    // Only then is there something to do before exiting; otherwise the signals keep their
    // default effect.
    let preallocating = cfg
        .data_node
        .as_ref()
        .is_some_and(|d| d.preallocate.enabled);
    if preallocating {
        atlas_native::prealloc::install_stop_signals();
    }
    let node = match NativeNode::start(cfg) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("atlas-native-node: failed to start: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "atlas-native-node {}: http={} data_node={:?} metadata={:?}",
        node.id(),
        node.http_addr(),
        node.data_node_addr(),
        node.metadata_addr()
    );
    if std::env::var("ATLAS_NATIVE_RDMA").is_ok_and(|v| v == "1") {
        if let Some(w) = atlas_native::rdma_link::memlock_warning() {
            eprintln!("atlas-native-node {}: warning: RDMA is enabled but {w}", node.id());
        }
    }
    // Raft state and data are crash-safe on disk, so termination needs no graceful path (with
    // preallocation, a requested stop first returns the unused device space); a storage failure
    // that a restart may cure exits non-zero so the supervisor restarts the node from disk. How
    // often is bounded (docs/NATIVE_NODE.md, "Failed data syncs").
    let poll = Duration::from_millis(if preallocating { 100 } else { 1000 });
    loop {
        if atlas_native::prealloc::stop_requested() {
            node.seal_devices();
            // Without the destructors, as when the signal ends the process itself.
            std::process::exit(0);
        }
        if let Some(reason) = node.restart_reason() {
            eprintln!(
                "atlas-native-node {}: exiting to be restarted: {reason}",
                node.id()
            );
            return ExitCode::FAILURE;
        }
        thread::sleep(poll);
    }
}
