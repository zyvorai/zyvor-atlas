// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Native Azure Blob Storage source for object migration (feature `azure-blob`).
//!
//! Azure Blob is NOT S3-compatible, so it cannot reuse [`crate::object::S3ObjectSource`].
//! This implements the [`ObjectSource`](crate::object::ObjectSource) trait directly on the
//! Azure SDK for Rust, mapping the migration model onto Azure's:
//!   * storage account name ← `access_key`
//!   * account key          ← `secret_key`
//!   * container name       ← `bucket`
//!
//! Like the `oracle`/`mongodb` connectors, this is gated behind a cargo feature and is NOT
//! part of the default build/CI (build with `--features atlas-databridge/azure-blob`).
//! Credentials never leave this process and are never logged.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use azure_storage::prelude::*;
use azure_storage_blobs::prelude::*;

use crate::object::ObjectSource;

/// Reads objects from an Azure Blob Storage container.
pub struct AzureBlobSource {
    container: ContainerClient,
}

impl AzureBlobSource {
    /// Build a source for `container` in storage `account`, authenticated with `access_key`
    /// (the account key).
    pub fn new(account: &str, access_key: &str, container: &str) -> Result<Self> {
        let creds = StorageCredentials::access_key(account.to_string(), access_key.to_string());
        let container =
            ClientBuilder::new(account.to_string(), creds).container_client(container.to_string());
        Ok(Self { container })
    }
}

#[async_trait]
impl ObjectSource for AzureBlobSource {
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
        let mut builder = self.container.list_blobs();
        if let Some(p) = prefix {
            builder = builder.prefix(p.to_string());
        }
        let mut stream = builder.into_stream();
        let mut out = Vec::new();
        while let Some(resp) = stream.next().await {
            let resp = resp.context("list azure blobs")?;
            for blob in resp.blobs.blobs() {
                out.push((blob.name.clone(), blob.properties.content_length));
            }
        }
        Ok(out)
    }

    async fn stream_to(
        &self,
        key: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<(u64, String)> {
        let blob_client = self.container.blob_client(key);
        let mut stream = blob_client.get().into_stream();
        let mut total: u64 = 0;
        let mut hasher = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("get azure blob {key}"))?;
            let body = chunk
                .data
                .collect()
                .await
                .with_context(|| format!("read azure blob body {key}"))?;
            hasher.update(&body);
            total += body.len() as u64;
            sink.write_all(&body)
                .await
                .with_context(|| format!("write azure blob {key} to sink"))?;
        }
        sink.flush().await.ok();
        Ok((total, hex::encode(hasher.finalize())))
    }
}
