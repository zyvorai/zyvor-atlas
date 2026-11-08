// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Atlas gateway library: state, auth, routes, and startup wiring shared by the binary and tests.

pub mod auth;
pub mod grpc;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod oauth_resource;
pub mod routes;
pub mod startup;
pub mod state;

pub use state::AppState;

/// Generated gRPC types + service stubs (package `atlas.v1`).
#[allow(clippy::result_large_err)] // tonic::Status (176B) in generated method signatures, not our code
pub mod proto {
    tonic::include_proto!("atlas.v1");

    /// Encoded FileDescriptorSet for gRPC server reflection (lets grpcurl work without the proto).
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/atlas_descriptor.bin"));
}
