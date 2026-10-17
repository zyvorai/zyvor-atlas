// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Lifecycle operations on a Mac's external disks. Local to the Mac the gateway runs on. The hard
//! refusals live in `atlas_driver_macos::ops` and are re-checked here, right before executing,
//! because the job may run long after the route accepted it. A final discovery refreshes inventory.

use anyhow::Result;
use sqlx::AnyPool;

use crate::spec::JobSpec;

const BACKEND_ID: &str = "bkd_macos";

pub(crate) async fn dispatch_macos(pool: &AnyPool, spec: JobSpec) -> Result<serde_json::Value> {
    let settings = atlas_driver_macos::EnvSettings::from_env();
    anyhow::ensure!(
        settings.enabled,
        "the macOS backend is not enabled (ATLAS_MACOS_ENABLE=1)"
    );
    let ops = settings.ops();
    let (op, target, output) = match spec {
        JobSpec::MacosDiskErase { device, confirmed_device, name, .. } => {
            anyhow::ensure!(device == confirmed_device, "device confirmation mismatch");
            let out = ops.erase_disk(&device, &name).await?;
            ("erase", device, out)
        }
        JobSpec::MacosVolumeAdd { container, name, quota_bytes, reserve_bytes, .. } => {
            let out = ops.add_volume(&container, &name, quota_bytes, reserve_bytes).await?;
            ("add_volume", container, out)
        }
        JobSpec::MacosVolumeDelete { device, confirmed_device, .. } => {
            anyhow::ensure!(device == confirmed_device, "device confirmation mismatch");
            let out = ops.delete_volume(&device).await?;
            ("delete_volume", device, out)
        }
        JobSpec::MacosDiskEject { device, .. } => {
            let out = ops.eject(&device).await?;
            ("eject", device, out)
        }
        other => anyhow::bail!("not a macOS job: {}", other.job_type()),
    };
    // Real discovery prunes what vanished (erased disk, deleted volume, ejected drive).
    let refreshed = atlas_discovery::run_discovery(pool, settings.driver(BACKEND_ID), None, None)
        .await
        .is_ok();
    Ok(serde_json::json!({
        "operation": op,
        "target": target,
        "output": output,
        "inventory_refreshed": refreshed,
    }))
}
