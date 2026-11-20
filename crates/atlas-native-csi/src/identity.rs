// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The CSI identity service.

use tonic::{Request, Response, Status};

use crate::{
    proto::{
        identity_server::Identity, plugin_capability, BoolValue, GetPluginCapabilitiesRequest,
        GetPluginCapabilitiesResponse, GetPluginInfoRequest, GetPluginInfoResponse,
        PluginCapability, ProbeRequest, ProbeResponse,
    },
    DRIVER_NAME,
};

pub struct IdentityService {
    /// Whether this process also serves the controller service.
    pub controller: bool,
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: DRIVER_NAME.into(),
            vendor_version: env!("CARGO_PKG_VERSION").into(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        let capabilities = if self.controller {
            vec![
                PluginCapability {
                    r#type: Some(plugin_capability::Type::Service(
                        plugin_capability::Service {
                            r#type: plugin_capability::service::Type::ControllerService as i32,
                        },
                    )),
                },
                // Expansion raises the filesystem's quota, which mounted pods see at once.
                PluginCapability {
                    r#type: Some(plugin_capability::Type::VolumeExpansion(
                        plugin_capability::VolumeExpansion {
                            r#type: plugin_capability::volume_expansion::Type::Online as i32,
                        },
                    )),
                },
            ]
        } else {
            Vec::new()
        };
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn probe(&self, _: Request<ProbeRequest>) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse {
            ready: Some(BoolValue { value: true }),
        }))
    }
}
