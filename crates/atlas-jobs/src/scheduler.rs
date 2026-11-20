// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use sqlx::AnyPool;

use crate::engine::JobEngine;
use crate::spec::JobSpec;

/// Spawn the protection-schedule worker: every `tick_secs`, run any due `snapshot_schedules` by
/// enqueueing a snapshot job for their volume, advance `next_run_at`, and prune the volume's
/// scheduled snapshots to the schedule's `keep`. `tick_secs == 0` disables it.
pub fn spawn_scheduler(
    pool: AnyPool,
    jobs: JobEngine,
    tick_secs: u64,
    is_leader: Arc<AtomicBool>,
) {
    if tick_secs == 0 {
        tracing::info!("snapshot scheduler disabled (tick = 0)");
        return;
    }
    tokio::spawn(async move {
        tracing::info!(tick_secs, "snapshot scheduler started");
        let mut tick = tokio::time::interval(Duration::from_secs(tick_secs));
        loop {
            tick.tick().await;
            // HA: only the leader replica enqueues scheduled snapshots/backups.
            if !is_leader.load(Ordering::Relaxed) {
                continue;
            }
            if let Err(e) = run_due_schedules(&pool, &jobs).await {
                tracing::warn!("snapshot scheduler tick failed: {e:#}");
            }
        }
    });
}

/// Marker embedded in scheduled snapshot names so retention only prunes scheduler-created snapshots.
const SCHED_MARKER: &str = "-sched-";

async fn run_due_schedules(pool: &AnyPool, jobs: &JobEngine) -> Result<()> {
    let due = atlas_inventory::schedules::due(pool).await?;
    for sched in due {
        // Advance the clock first so a slow/failed run doesn't hot-loop this schedule.
        if let Err(e) = atlas_inventory::schedules::mark_ran(pool, &sched.id).await {
            tracing::warn!("schedule {}: mark_ran failed: {e:#}", sched.id);
            continue;
        }
        let vol = match atlas_inventory::get_volume(pool, &sched.volume_id).await {
            Ok(Some(v)) => v,
            _ => {
                tracing::warn!("schedule {}: volume {} gone", sched.id, sched.volume_id);
                continue;
            }
        };
        let (Some(namespace), Some(pvc_name)) =
            (vol.kubernetes_namespace.clone(), vol.pvc_name.clone())
        else {
            continue;
        };
        if sched.kind == "backup" {
            if let Err(e) = run_backup_schedule(pool, jobs, &sched, &namespace, &pvc_name).await {
                tracing::warn!("schedule {}: backup failed: {e:#}", sched.id);
            }
            continue;
        }
        let snapshot_id = atlas_common::ids::snapshot_id();
        let name = format!("{}{SCHED_MARKER}{}", vol.name, &snapshot_id[5..]);
        let spec = JobSpec::SnapshotCreate {
            snapshot_id,
            volume_id: sched.volume_id.clone(),
            name,
            namespace,
            pvc_name,
            snapshot_class: "zyvor-rbd-snapclass".into(),
        };
        let job_id = atlas_common::ids::job_id();
        if let Err(e) = jobs
            .enqueue(&job_id, &sched.tenant_id, "scheduler", spec, None)
            .await
        {
            tracing::warn!("schedule {}: enqueue snapshot failed: {e:#}", sched.id);
            continue;
        }
        prune_scheduled_snapshots(pool, jobs, &sched.volume_id, sched.keep).await;
    }
    Ok(())
}

/// Delete the volume's scheduler-created snapshots beyond the newest `keep` (enqueues delete jobs).
async fn prune_scheduled_snapshots(
    pool: &AnyPool,
    jobs: &JobEngine,
    volume_id: &str,
    keep: i64,
) {
    if keep <= 0 {
        return;
    }
    let snaps = match atlas_inventory::snapshots::list_snapshots(pool, Some(volume_id)).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let namespace = atlas_inventory::get_volume(pool, volume_id)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.kubernetes_namespace)
        .unwrap_or_else(|| "default".into());
    // Newest-first. This runs right after enqueuing a new snapshot that isn't persisted yet, so we
    // retain `keep - 1` already-stored snapshots; together with the in-flight one the steady state
    // is exactly `keep`.
    let scheduled: Vec<_> = snaps
        .into_iter()
        .filter(|s| s.name.contains(SCHED_MARKER))
        .collect();
    let retain = (keep - 1).max(0) as usize;
    for old in scheduled.into_iter().skip(retain) {
        let spec = JobSpec::SnapshotDelete {
            snapshot_id: old.id.clone(),
            namespace: namespace.clone(),
            name: old.name,
        };
        let job_id = atlas_common::ids::job_id();
        let _ = jobs
            .enqueue(&job_id, &old.tenant_id, "scheduler", spec, None)
            .await;
    }
}

/// Run one backup schedule: snapshot + write a backup to the target bucket, then prune old backups.
async fn run_backup_schedule(
    pool: &AnyPool,
    jobs: &JobEngine,
    sched: &atlas_api_types::SnapshotSchedule,
    namespace: &str,
    pvc_name: &str,
) -> Result<()> {
    let bucket_id = sched
        .bucket_id
        .clone()
        .ok_or_else(|| anyhow!("backup schedule {} has no bucket", sched.id))?;
    let bucket = atlas_inventory::buckets::get_bucket(pool, &bucket_id)
        .await?
        .ok_or_else(|| anyhow!("bucket {bucket_id} not found"))?;
    if bucket.state != "bound" {
        anyhow::bail!("bucket {bucket_id} not bound (state {})", bucket.state);
    }
    let bucket_endpoint = bucket
        .endpoint
        .ok_or_else(|| anyhow!("bucket has no endpoint"))?;
    let bucket_name = bucket
        .bucket_name
        .ok_or_else(|| anyhow!("bucket has no name"))?;
    let bucket_secret_ref = bucket
        .secret_ref
        .ok_or_else(|| anyhow!("bucket has no secret"))?;
    let bucket_namespace = bucket.namespace.unwrap_or_else(|| "rook-ceph".into());
    let bucket_region = bucket.region.unwrap_or_else(|| "us-east-1".into());

    let backup_id = atlas_common::ids::stable_id(
        "bkp",
        &format!("{}-{}", sched.volume_id, atlas_common::ids::job_id()),
    );
    let snapshot_id = atlas_common::ids::snapshot_id();
    let snapshot_name = format!("{pvc_name}-bkp-{}", &backup_id[4..]);
    let object_key = format!("backups/{}/{}.manifest.json", sched.volume_id, backup_id);
    let manifest = serde_json::json!({
        "backup_id": backup_id, "source_volume": sched.volume_id, "source_snapshot": snapshot_id,
        "pvc": format!("{namespace}/{pvc_name}"), "object_key": object_key,
        "format": "manifest-v1", "scheduled": sched.id,
    });
    atlas_inventory::snapshots::insert_snapshot(
        pool,
        &snapshot_id,
        &sched.tenant_id,
        &sched.volume_id,
        &snapshot_name,
        None,
        "app",
        "creating",
    )
    .await?;
    atlas_inventory::backups::insert_backup(
        pool,
        &backup_id,
        &sched.tenant_id,
        &sched.volume_id,
        Some(&snapshot_id),
        &bucket_id,
        &object_key,
        "manifest-v1",
        &manifest,
    )
    .await?;
    let spec = JobSpec::BackupCreate {
        backup_id: backup_id.clone(),
        snapshot_id,
        volume_namespace: namespace.to_string(),
        pvc_name: pvc_name.to_string(),
        snapshot_name,
        snapshot_class: "zyvor-rbd-snapclass".into(),
        object_key,
        manifest_json: manifest.to_string(),
        bucket_namespace,
        bucket_secret_ref,
        bucket_endpoint,
        bucket_name,
        bucket_region,
        mode: sched.mode.clone(),
    };
    let job_id = atlas_common::ids::job_id();
    jobs.enqueue(&job_id, &sched.tenant_id, "scheduler", spec, None)
        .await?;
    prune_scheduled_backups(pool, jobs, &sched.volume_id, sched.keep).await;
    Ok(())
}

/// Delete the volume's backups beyond the newest `keep` (enqueues backup-delete jobs). Retains
/// `keep - 1` persisted backups since one is being created this tick.
async fn prune_scheduled_backups(pool: &AnyPool, jobs: &JobEngine, volume_id: &str, keep: i64) {
    if keep <= 0 {
        return;
    }
    let all = match atlas_inventory::backups::list_backups(pool, Some(volume_id)).await {
        Ok(a) => a,
        Err(_) => return,
    };
    let retain = (keep - 1).max(0) as usize;
    for old in all.into_iter().skip(retain) {
        let bucket = match atlas_inventory::buckets::get_bucket(pool, &old.bucket_id).await {
            Ok(Some(b)) if b.state == "bound" => b,
            _ => continue,
        };
        let vol = atlas_inventory::get_volume(pool, &old.volume_id)
            .await
            .ok()
            .flatten();
        let ns = vol
            .as_ref()
            .and_then(|v| v.kubernetes_namespace.clone())
            .unwrap_or_default();
        let pvc = vol.and_then(|v| v.pvc_name).unwrap_or_default();
        let spec = JobSpec::BackupDelete {
            backup_id: old.id.clone(),
            manifest_key: old.object_key.clone(),
            data_key: format!("{}.rbd-diff", old.object_key),
            volume_namespace: ns,
            // Same derivation `create_backup` used for the CSI VolumeSnapshot name
            // (routes/object_store.rs) — recomputed here since it isn't persisted anywhere this
            // pruning path can read back.
            snapshot_name: format!("{pvc}-bkp-{}", &old.id[4..]),
            pvc_name: pvc,
            rbd_snap: format!("atlasbkp-{}", &old.id[4..]),
            bucket_namespace: bucket.namespace.unwrap_or_else(|| "rook-ceph".into()),
            bucket_secret_ref: bucket.secret_ref.unwrap_or_default(),
            bucket_endpoint: bucket.endpoint.unwrap_or_default(),
            bucket_name: bucket.bucket_name.unwrap_or_default(),
            bucket_region: bucket.region.unwrap_or_else(|| "us-east-1".into()),
        };
        let job_id = atlas_common::ids::job_id();
        let _ = jobs
            .enqueue(&job_id, &old.tenant_id, "scheduler", spec, None)
            .await;
    }
}
