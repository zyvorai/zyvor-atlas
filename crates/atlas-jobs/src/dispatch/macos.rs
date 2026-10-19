// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Lifecycle operations on a Mac's external disks: the Mac the gateway runs on, or another Mac through
//! its agent (the job's `backend_id`). The hard
//! refusals live in `atlas_driver_macos::ops` and are re-checked here, right before executing,
//! because the job may run long after the route accepted it. A final discovery refreshes inventory.

use anyhow::Result;
use atlas_driver_core::StorageDriver as _;
use sqlx::AnyPool;

use crate::spec::JobSpec;

const BACKEND_ID: &str = "bkd_macos";

fn backend_of(spec: &JobSpec) -> Option<&str> {
    match spec {
        JobSpec::MacosDiskErase { backend_id, .. }
        | JobSpec::MacosVolumeAdd { backend_id, .. }
        | JobSpec::MacosVolumeDelete { backend_id, .. }
        | JobSpec::MacosDiskEject { backend_id, .. } => Some(backend_id),
        _ => None,
    }
}

pub(crate) async fn dispatch_macos(pool: &AnyPool, spec: JobSpec) -> Result<serde_json::Value> {
    // The job names its Mac: this one (`bkd_macos`) or an agent (`bkd_macos_<name>`), where the
    // agent runs the same refusals against its own live layout.
    let backend = backend_of(&spec).unwrap_or(BACKEND_ID).to_string();
    let settings = atlas_driver_macos::EnvSettings::from_env();
    let (ops, driver): (
        Box<dyn atlas_driver_macos::Lifecycle>,
        std::sync::Arc<dyn atlas_driver_core::StorageDriver>,
    ) = if backend == BACKEND_ID {
        anyhow::ensure!(
            settings.enabled,
            "the macOS backend is not enabled (ATLAS_MACOS_ENABLE=1)"
        );
        (Box::new(settings.ops()), settings.driver(BACKEND_ID))
    } else {
        let agent = atlas_driver_macos::agents_from_env()
            .map_err(|e| anyhow::anyhow!(e))?
            .into_iter()
            .find(|d| d.backend_id() == backend)
            .ok_or_else(|| anyhow::anyhow!("no macOS agent backend {backend} (ATLAS_MACOS_AGENTS)"))?;
        (Box::new(agent.ops()), std::sync::Arc::new(agent))
    };
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
    let refreshed = atlas_discovery::run_discovery(pool, driver, None, None)
        .await
        .is_ok();
    Ok(serde_json::json!({
        "operation": op,
        "target": target,
        "output": output,
        "inventory_refreshed": refreshed,
    }))
}
