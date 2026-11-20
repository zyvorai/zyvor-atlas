// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Container Storage Interface driver for atlas-native filesystems (binary `atlas-native-csi`).
//! The controller service maps CSI volumes and snapshots onto `/v1/fs` filesystems and
//! filesystem snapshots; the node service publishes a volume by running `atlas-native-mount`
//! on the pod's target path.

use std::{future::Future, path::Path};

use tonic::transport::Server;

pub mod controller;
pub mod identity;
pub mod mountinfo;
pub mod node;

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("csi.v1");
}

/// The CSI driver name (`CSIDriver` object and StorageClass `provisioner`).
pub const DRIVER_NAME: &str = "native.atlas.zyvor.ai";

/// The socket path of a `unix://` CSI endpoint (a bare path is accepted too).
pub fn socket_path(endpoint: &str) -> &str {
    endpoint.strip_prefix("unix://").unwrap_or(endpoint)
}

/// Serves the identity service plus whichever of the controller and node services are given on
/// the Unix socket `path` until `shutdown` resolves. A stale socket file is replaced.
pub async fn serve(
    path: &Path,
    identity: identity::IdentityService,
    controller: Option<controller::ControllerService>,
    node: Option<node::NodeService>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let incoming =
        tokio_stream::wrappers::UnixListenerStream::new(tokio::net::UnixListener::bind(path)?);
    Server::builder()
        .add_service(proto::identity_server::IdentityServer::new(identity))
        .add_optional_service(controller.map(proto::controller_server::ControllerServer::new))
        .add_optional_service(node.map(proto::node_server::NodeServer::new))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await?;
    Ok(())
}
