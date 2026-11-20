// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Discovery worker: run a driver's discovery pass and normalize it into the SQLite inventory
//! (PDF §8.1). Emits a `storage.backend.discovered` log event on success.

use std::sync::Arc;

use anyhow::{Context, Result};
use atlas_driver_core::StorageDriver;
use sqlx::AnyPool;

/// Summary returned to the caller after a discovery pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoverySummary {
    pub backend_id: String,
    pub cluster_id: String,
    pub pools: usize,
    pub osds: usize,
    pub volumes: usize,
}

/// Maps a Ceph **RBD image name** → `(namespace, pvc_name, storage_class)` of the owning PVC.
/// Built by the gateway from cluster PersistentVolumes (see `K8sDriver::rbd_image_owners`) and
/// passed in so `atlas-discovery` stays decoupled from the Kubernetes driver.
pub type RbdOwners = std::collections::HashMap<String, (String, String, Option<String>)>;

/// Maps a Ceph **pool name** → its precise kind (`"rbd"`/`"cephfs_data"`/`"cephfs_metadata"`/
/// `"rgw"`), as read from live Rook CRs. Built by the gateway (see
/// `K8sDriver::known_rook_pool_kinds`) and passed in so `atlas-discovery` stays decoupled from
/// the Kubernetes driver — mirrors the `RbdOwners` enrichment below.
pub type RookPoolKinds = std::collections::HashMap<String, String>;

/// Run discovery for one driver and persist the result. When `rbd_owners` is provided, block
/// volumes that the driver couldn't attribute to Kubernetes are enriched with their owning PVC —
/// this is what lets raw `rbd ls` images show up as real VM disks (namespace/PVC) in inventory.
/// When `rook_pool_kinds` is provided, pools whose name matches a live Rook CR are reclassified
/// with the precise kind instead of the driver's own name-heuristic guess.
#[tracing::instrument(skip(pool, driver, rbd_owners, rook_pool_kinds), fields(backend_id = %driver.backend_id()))]
pub async fn run_discovery(
    pool: &AnyPool,
    driver: Arc<dyn StorageDriver>,
    rbd_owners: Option<&RbdOwners>,
    rook_pool_kinds: Option<&RookPoolKinds>,
) -> Result<DiscoverySummary> {
    let backend_id = driver.backend_id().to_string();
    let mut discovery = driver
        .discover()
        .await
        .with_context(|| format!("discovery failed for backend {backend_id}"))?;

    if let Some(owners) = rbd_owners {
        let mut enriched = 0usize;
        for v in discovery.volumes.iter_mut() {
            if v.kubernetes_namespace.is_some() {
                continue;
            }
            if let Some((ns, pvc, sc)) = owners.get(&v.name) {
                v.kubernetes_namespace = Some(ns.clone());
                v.pvc_name = Some(pvc.clone());
                if v.storage_class_name.is_none() {
                    v.storage_class_name = sc.clone();
                }
                enriched += 1;
            }
        }
        if enriched > 0 {
            tracing::info!(backend = %backend_id, enriched, "discovery.rbd_pvc_correlated");
        }
    }

    if let Some(kinds) = rook_pool_kinds {
        let mut reclassified = 0usize;
        for p in discovery.pools.iter_mut() {
            if let Some(kind) = kinds.get(&p.name) {
                if &p.kind != kind {
                    p.kind = kind.clone();
                    reclassified += 1;
                }
            }
        }
        if reclassified > 0 {
            tracing::info!(backend = %backend_id, reclassified, "discovery.pool_kind_via_rook");
        }
    }

    atlas_inventory::upsert_discovery(pool, &backend_id, &discovery, !driver.is_fixture())
        .await
        .context("persisting discovery result")?;

    let summary = DiscoverySummary {
        backend_id: backend_id.clone(),
        cluster_id: discovery.cluster.id.clone(),
        pools: discovery.pools.len(),
        osds: discovery.osds.len(),
        volumes: discovery.volumes.len(),
    };
    tracing::info!(
        backend = %summary.backend_id,
        cluster = %summary.cluster_id,
        pools = summary.pools,
        osds = summary.osds,
        volumes = summary.volumes,
        "storage.backend.discovered"
    );
    Ok(summary)
}
