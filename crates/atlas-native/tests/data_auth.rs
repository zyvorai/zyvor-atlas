// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Data-node client auth (`DataAuth`): a bearer or nothing. Certificate acceptance is covered
//! by `tests/tls.rs` (mutual TLS). Here the node has no TLS, so the bearer is the only proof.

use std::{net::TcpListener, time::Duration};

use atlas_native::{
    data_node::{DataAuth, DataNodeServer, DataServe},
    device::FileDevice, BlockStore, NativeError, RemoteDevice,
};

#[test]
fn unauthenticated_client_is_rejected_and_a_bearer_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let dev: std::sync::Arc<dyn BlockStore> =
        std::sync::Arc::new(FileDevice::open(dir.path().join("nvme0.data")).unwrap());
    let n = DataNodeServer::start_guarded(
        "n1",
        dir.path(),
        vec![dev],
        TcpListener::bind("127.0.0.1:0").unwrap(),
        DataServe {
            tls: None,
            grant_key: None,
            auth: DataAuth {
                required: true,
                token: Some("data-token".into()),
            },
            tls_paths: None,
        },
    )
    .unwrap();

    let bare = RemoteDevice::new(n.local_addr(), Duration::from_secs(2));
    match bare.len() {
        Err(NativeError::Remote(m)) => assert!(m.contains("unauthenticated"), "{m}"),
        other => panic!("expected unauthenticated, got {other:?}"),
    }

    let authed = RemoteDevice::new(n.local_addr(), Duration::from_secs(2)).with_bearer("data-token");
    assert_eq!(authed.len().unwrap(), 0);

    let wrong = RemoteDevice::new(n.local_addr(), Duration::from_secs(2)).with_bearer("nope");
    match wrong.len() {
        Err(NativeError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}")
        }
        other => panic!("expected a rejected bearer, got {other:?}"),
    }
}
