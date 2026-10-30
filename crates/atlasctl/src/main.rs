// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `atlasctl` — a thin REST client for the Atlas gateway.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "atlasctl", about = "Atlas storage control-plane CLI")]
struct Cli {
    /// Gateway base URL.
    #[arg(long, env = "ATLAS_BASE_URL", default_value = "http://127.0.0.1:5110")]
    base_url: String,

    /// Optional bearer token (when the gateway has auth enabled).
    #[arg(long, env = "ATLAS_TOKEN")]
    token: Option<String>,

    /// Observe-first I/O agent base URL (`atlas-io-agent`, not the gateway).
    #[arg(long, env = "ATLAS_IO_URL", default_value = "http://127.0.0.1:5111")]
    io_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// GET /health
    Health,
    /// GET /readyz — readiness deep-check (DB + driver + k8s)
    Ready,
    /// GET /version
    Version,
    /// GET /api/atlas/v1/backends
    Backends,
    /// GET /api/atlas/v1/backends/summary — per-backend breakdown (type, counts, capacity)
    BackendsSummary,
    /// POST /api/atlas/v1/backends/{id}/discover (default backend: bkd_ceph_lab)
    Discover {
        #[arg(default_value = "bkd_ceph_lab")]
        backend: String,
    },
    /// GET /api/atlas/v1/clusters
    Clusters,
    /// GET /api/atlas/v1/pools
    Pools,
    /// GET /api/atlas/v1/osds
    Osds,
    /// GET /api/atlas/v1/volumes
    Volumes,
    /// GET /api/atlas/v1/storage-classes (live from Kubernetes)
    StorageClasses,
    /// GET /api/atlas/v1/metrics/summary
    Metrics,
    /// GET /api/atlas/v1/alerts (optionally filter by --state open|resolved)
    Alerts {
        #[arg(long)]
        state: Option<String>,
    },
    /// GET /api/atlas/v1/metrics/ceph (latest scraped Ceph metrics; --prefix to filter)
    CephMetrics {
        #[arg(long)]
        prefix: Option<String>,
    },
    /// GET /api/atlas/v1/ceph/status — live `ceph status` (health, quorum, osdmap, pgmap, I/O)
    CephStatus,
    /// GET /api/atlas/v1/ceph/osd-tree — the CRUSH hierarchy (roots → hosts → OSDs)
    CephOsdTree,
    /// GET /api/atlas/v1/ceph/osd-df — per-OSD utilization (size/used/avail/%, PG count)
    CephOsdDf,
    /// GET /api/atlas/v1/ceph/devices — drives Ceph tracks and the ones predicted to fail soon
    CephDevices,
    /// GET /api/atlas/v1/ceph/nvme — NVMe wear, media errors and spare from SMART scrapes
    CephNvme,
    /// GET /api/atlas/v1/ceph/nvmeof — Rook NVMe-oF gateways, their pods and NVMe-oF StorageClasses
    CephNvmeof,
    /// POST /api/atlas/v1/ceph/nvmeof/namespaces — provision a new NVMe-oF namespace: a PVC from an
    /// NVMe-oF StorageClass (admin; `--yes` confirms)
    CephNvmeofCreate {
        name: String,
        /// An `nvmeof.csi.ceph.com` StorageClass (see `ceph-nvmeof`).
        #[arg(long)]
        storage_class: String,
        /// Size in GiB.
        #[arg(long, default_value_t = 2)]
        size_gib: i64,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, default_value = "tenant_default")]
        tenant: String,
        #[arg(long)]
        yes: bool,
    },
    /// GET /api/atlas/v1/ceph/osd-objectstore — each OSD's object store and Ceph version
    CephOsdObjectstore,
    /// GET /api/atlas/v1/ceph/sizing — NVMe hosts against the CPU/RAM rule of thumb
    CephSizing,
    /// GET /api/atlas/v1/io/ai — heuristic AI I/O shape per device, via the gateway's atlas-io agent
    IoAi,
    /// GET /api/atlas/v1/ceph/df — cluster + per-pool capacity/usage/objects
    CephDf,
    /// GET /api/atlas/v1/metrics/history — persisted capacity/IO/job time-series (--minutes)
    History {
        #[arg(long, default_value_t = 60)]
        minutes: i64,
    },
    /// GET /api/atlas/v1/metrics/forecast — least-squares days-until-full projection (--minutes)
    Forecast {
        #[arg(long, default_value_t = 1440)]
        minutes: i64,
    },
    /// GET /metrics — Atlas's own state in Prometheus text-exposition format
    SelfMetrics,
    /// GET /api/atlas/v1/policies
    Policies,
    /// POST /api/atlas/v1/volumes/{id}/schedule — periodic snapshots for a volume
    ScheduleSnapshots {
        volume_id: String,
        #[arg(long)]
        interval_secs: i64,
        #[arg(long, default_value_t = 0)]
        keep: i64,
    },
    /// POST /api/atlas/v1/volumes/{id}/schedule — periodic backups to a bucket
    ScheduleBackups {
        volume_id: String,
        #[arg(long)]
        bucket_id: String,
        #[arg(long)]
        interval_secs: i64,
        #[arg(long, default_value_t = 0)]
        keep: i64,
        #[arg(long, default_value = "manifest")]
        mode: String,
    },
    /// GET /api/atlas/v1/schedules (optionally --volume-id)
    Schedules {
        #[arg(long)]
        volume_id: Option<String>,
    },
    /// DELETE /api/atlas/v1/schedules/{id}
    DeleteSchedule { id: String },
    /// POST /api/atlas/v1/auth/tokens — mint a service-account JWT (admin)
    IssueToken {
        subject: String,
        #[arg(long, default_value = "viewer")]
        role: String,
        #[arg(long, default_value_t = 3600)]
        ttl_secs: u64,
    },
    /// POST /api/atlas/v1/rbd-usage/refresh — recompute used_bytes via rbd du
    RefreshUsage,
    /// GET /api/atlas/v1/rbd-images?pool= — list raw RBD images in a pool
    RbdImages {
        #[arg(long)]
        pool: Option<String>,
    },
    /// POST /api/atlas/v1/rbd-images — provision a raw RBD image (bypassing CSI)
    CreateRbdImage {
        name: String,
        #[arg(long, default_value_t = 1)]
        size_gib: i64,
        #[arg(long)]
        pool: Option<String>,
    },
    /// DELETE /api/atlas/v1/rbd-images/{pool}/{image}
    DeleteRbdImage { pool: String, image: String },
    /// POST /api/atlas/v1/rbd-images/{pool}/{image}/clone — COW clone (golden image)
    CloneRbdImage {
        pool: String,
        image: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        snap: Option<String>,
    },
    /// POST /api/atlas/v1/rbd-images/{pool}/{image}/resize — grow a raw RBD image
    ResizeRbdImage {
        pool: String,
        image: String,
        #[arg(long)]
        size_gib: i64,
    },
    /// POST /api/atlas/v1/rbd-images/{pool}/{image}/flatten — detach a clone from its parent
    FlattenRbdImage { pool: String, image: String },
    /// GET/POST /api/atlas/v1/rbd-images/{pool}/{image}/snapshots (--name to create)
    RbdSnaps {
        pool: String,
        image: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// POST /api/atlas/v1/rbd-images/{pool}/{image}/rollback — roll back to a snapshot
    RollbackRbdImage {
        pool: String,
        image: String,
        #[arg(long)]
        snap: String,
    },
    /// GET /api/atlas/v1/buckets/{id}/stats — RGW usage + quota
    BucketStats { id: String },
    /// GET /api/atlas/v1/buckets/{id}/objects (optional --prefix)
    BucketObjects {
        id: String,
        #[arg(long)]
        prefix: Option<String>,
    },
    /// GET /api/atlas/v1/tenants — overview of tenants (usage + quota)
    Tenants,
    /// GET /api/atlas/v1/volumes/{id}/bindings — product ownership for a volume
    VolumeBindings { id: String },
    /// GET /api/atlas/v1/volumes/{id}/labels
    VolumeLabels { id: String },
    /// PUT /api/atlas/v1/volumes/{id}/labels — merge one label (--key --value)
    SetVolumeLabel {
        id: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
    },
    /// GET /api/atlas/v1/audit — query the audit trail (filters + --limit)
    Audit {
        #[arg(long)]
        actor: Option<String>,
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        resource_type: Option<String>,
        #[arg(long)]
        resource_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// GET /api/atlas/v1/tenants/{id}/policies — a tenant's intent overrides
    TenantPolicies { tenant_id: String },
    /// PUT /api/atlas/v1/tenants/{id}/policies/{intent} — override an intent's placement (admin)
    SetTenantPolicy {
        tenant_id: String,
        intent: String,
        #[arg(long)]
        storage_class: String,
        #[arg(long, default_value = "ReadWriteOnce")]
        access_mode: String,
        #[arg(long, default_value = "Filesystem")]
        volume_mode: String,
    },
    /// GET /api/atlas/v1/tenants/{id}/quota — a tenant's quota + usage
    Quota { tenant_id: String },
    /// PUT /api/atlas/v1/tenants/{id}/quota — set a tenant's quota (0 = unlimited)
    SetQuota {
        tenant_id: String,
        #[arg(long, default_value_t = 0)]
        max_bytes: i64,
        #[arg(long, default_value_t = 0)]
        max_volumes: i64,
    },
    /// GET /api/atlas/v1/jobs  (or a single job with an id)
    Jobs { id: Option<String> },
    /// POST /api/atlas/v1/jobs/{id}/cancel — admin escape hatch for a wedged job (409 if terminal)
    CancelJob { id: String },
    /// GET /api/atlas/v1/snapshots
    Snapshots,
    /// POST /api/atlas/v1/volumes — create a volume: a Ceph-backed PVC by default, or directly on
    /// another backend with `--backend` (e.g. `bkd_native`)
    CreateVolume {
        name: String,
        /// Size in GiB (atlas-native filesystems have no size limit and ignore it).
        #[arg(long, default_value_t = 2)]
        size_gib: i64,
        /// `block` or `filesystem`.
        #[arg(long, default_value = "block")]
        kind: String,
        /// Create on this backend instead of provisioning a PVC.
        #[arg(long)]
        backend: Option<String>,
        #[arg(long, default_value = "database")]
        policy: String,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, default_value = "tenant_default")]
        tenant: String,
    },
    /// POST /api/atlas/v1/volumes/{id}/snapshots
    SnapshotVolume {
        volume_id: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// DELETE /api/atlas/v1/volumes/{id}
    DeleteVolume { id: String },
    /// POST /api/atlas/v1/snapshots/{id}/clone
    CloneSnapshot {
        snapshot_id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        namespace: Option<String>,
    },
    /// POST /api/atlas/v1/snapshots/{id}/restore
    RestoreSnapshot {
        snapshot_id: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// DELETE /api/atlas/v1/snapshots/{id}  (use --force if it has dependents)
    DeleteSnapshot {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// POST /api/atlas/v1/snapshots/{id}/lock  (WORM: cannot be deleted until it expires; atlas-native)
    LockSnapshot {
        id: String,
        /// Retention in seconds from now; a lock can be extended, never shortened.
        #[arg(long)]
        retain_secs: u64,
    },
    /// GET /api/atlas/v1/native/reduction  (atlas-native compression and sharing savings)
    NativeReduction,
    /// GET /api/atlas/v1/pools/{id}/dedup-estimate  (what dedup would save; reads data, admin only)
    DedupEstimate {
        pool_id: String,
        /// Stop after examining this many bytes (block-scan backends such as Ceph).
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Seconds before giving up (ZFS's `zdb -S` reads every block).
        #[arg(long)]
        max_secs: Option<u64>,
        /// Queue it as a background job (POST) and print the job; poll it with `atlasctl jobs <id>`.
        #[arg(long)]
        background: bool,
    },
    /// GET /api/atlas/v1/buckets
    Buckets,
    /// DELETE /api/atlas/v1/buckets/{id}  (--force if it still holds backups)
    DeleteBucket {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// POST /api/atlas/v1/buckets — provision an RGW bucket
    CreateBucket {
        name: String,
        #[arg(long)]
        namespace: Option<String>,
        /// RGW quota: max object count.
        #[arg(long)]
        max_objects: Option<i64>,
        /// RGW quota: max size (e.g. 2G).
        #[arg(long)]
        max_size: Option<String>,
    },
    /// GET /api/atlas/v1/ceph/pools — live CephBlockPool CRs
    CephPools,
    /// POST /api/atlas/v1/ceph/pools — create a CephBlockPool + StorageClass
    CreateCephPool {
        name: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        storage_class: Option<String>,
        #[arg(long)]
        replicated_size: Option<i64>,
        #[arg(long)]
        failure_domain: Option<String>,
        #[arg(long)]
        device_class: Option<String>,
    },
    /// DELETE /api/atlas/v1/ceph/pools/{name}  (--force if volumes still reference it)
    DeleteCephPool {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// GET /api/atlas/v1/ceph/filesystems — live CephFilesystem CRs
    CephFilesystems,
    /// POST /api/atlas/v1/ceph/filesystems — create a CephFilesystem (RWX) + StorageClass
    CreateCephFilesystem {
        name: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        storage_class: Option<String>,
        #[arg(long)]
        data_pool_name: Option<String>,
        #[arg(long)]
        replicated_size: Option<i64>,
    },
    /// DELETE /api/atlas/v1/ceph/filesystems/{name}  (--force if volumes still reference it)
    DeleteCephFilesystem {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// GET /api/atlas/v1/ceph/object-stores — live CephObjectStore CRs
    CephObjectStores,
    /// POST /api/atlas/v1/ceph/object-stores — create a CephObjectStore (RGW) + StorageClass
    CreateCephObjectStore {
        name: String,
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        storage_class: Option<String>,
        #[arg(long)]
        replicated_size: Option<i64>,
        #[arg(long)]
        gateway_port: Option<i64>,
        #[arg(long)]
        gateway_instances: Option<i64>,
    },
    /// DELETE /api/atlas/v1/ceph/object-stores/{name}  (--force if buckets still reference it)
    DeleteCephObjectStore {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// POST /api/atlas/v1/backup-jobs — back up a volume to a bucket
    BackupVolume {
        volume_id: String,
        #[arg(long)]
        bucket_id: String,
        /// "manifest" (default) or "data" (also exports RBD image data to S3).
        #[arg(long, default_value = "manifest")]
        mode: String,
        /// Retain only the most recent N backups for this volume (0 = config default).
        #[arg(long, default_value_t = 0)]
        keep: i64,
        /// Prune backups older than this many seconds (0 = config default).
        #[arg(long, default_value_t = 0)]
        max_age_secs: i64,
    },
    /// GET /api/atlas/v1/backups
    Backups,
    /// DELETE /api/atlas/v1/backups/{id}
    DeleteBackup { id: String },
    /// GET /api/atlas/v1/backups/{id}/download — presigned URL (--what data|manifest)
    BackupDownload {
        id: String,
        #[arg(long, default_value = "manifest")]
        what: String,
    },
    /// POST /api/atlas/v1/restore-jobs — restore a volume from a backup
    RestoreBackup {
        backup_id: String,
        #[arg(long)]
        name: Option<String>,
        /// "snapshot" (default) or "data" (reconstruct from the RBD diff in S3).
        #[arg(long, default_value = "snapshot")]
        mode: String,
    },

    // ---- Day-2 / DR / DataBridge (parity with REST) ----
    /// GET /api/atlas/v1/maintenance — worker pause flag
    Maintenance,
    /// POST /api/atlas/v1/maintenance — pause or resume the job worker
    SetMaintenance {
        #[arg(long, action = clap::ArgAction::Set)]
        paused: bool,
    },
    /// POST /api/atlas/v1/backends/{id}/cordon — stop new provisioning
    CordonBackend { id: String },
    /// POST /api/atlas/v1/backends/{id}/uncordon
    UncordonBackend { id: String },
    /// GET /api/atlas/v1/upgrade/preflight
    UpgradePreflight,
    /// GET /api/atlas/v1/maintenance/orphans
    Orphans,
    /// GET /api/atlas/v1/dr/peers
    DrPeers,
    /// POST /api/atlas/v1/dr/peers — register a mirroring peer
    DrRegisterPeer {
        name: String,
        #[arg(long)]
        cluster_fsid: Option<String>,
        #[arg(long)]
        secret_ref: Option<String>,
    },
    /// GET /api/atlas/v1/dr/preflight
    DrPreflight,
    /// GET /api/atlas/v1/dr/status
    DrStatus,
    /// GET /api/atlas/v1/dr/mirrors
    DrMirrors,
    /// GET /api/atlas/v1/dr/mirrors/{id}/status — live rbd status and whether a promote is clean
    DrMirrorStatus { id: String },
    /// GET /api/atlas/v1/dr/pools/{pool}/mirroring — pool mirroring mode and peers
    DrPoolMirroring { pool: String },
    /// PUT /api/atlas/v1/dr/pools/{pool}/mirroring — set the pool mode (`image` or `pool`)
    DrSetPoolMirroring { pool: String, mode: String },
    /// GET /api/atlas/v1/volume-groups
    VolumeGroups,
    /// POST /api/atlas/v1/volume-groups — consistency group from direct-RBD volumes
    VolumeGroupCreate {
        name: String,
        #[arg(required = true)]
        volume_ids: Vec<String>,
    },
    /// GET /api/atlas/v1/volume-groups/{id}
    VolumeGroup { id: String },
    /// DELETE /api/atlas/v1/volume-groups/{id} (the volumes stay)
    VolumeGroupDelete { id: String },
    /// POST /api/atlas/v1/volume-groups/{id}/snapshots — crash-consistent snapshot of every member
    VolumeGroupSnapshot { id: String, name: String },
    /// DELETE /api/atlas/v1/volume-groups/{id}/snapshots/{name}
    VolumeGroupSnapshotDelete { id: String, name: String },
    /// POST /api/atlas/v1/volume-groups/{id}/snapshots/{name}/rollback — destructive
    VolumeGroupRollback { id: String, name: String },
    /// POST /api/atlas/v1/dr/mirrors/{id}/promote [--force] [--peer-replayed]
    DrPromote {
        id: String,
        #[arg(long)]
        force: bool,
        /// Journal mode: the peer reported journal_peers_replayed: true
        #[arg(long)]
        peer_replayed: bool,
    },
    /// POST /api/atlas/v1/dr/mirrors/{id}/demote
    DrDemote { id: String },
    /// POST /api/atlas/v1/dr/failover — confirm-gated failover runbook
    DrFailover {
        mirror_id: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        peer_replayed: bool,
    },
    /// POST /api/atlas/v1/dr/mirrors/{id}/rpo
    DrSetRpo {
        id: String,
        #[arg(long)]
        rpo_seconds: i64,
    },
    /// POST /api/atlas/v1/volumes/{id}/mirror — enable RBD mirroring
    VolumeMirrorEnable {
        id: String,
        #[arg(long)]
        peer: String,
        #[arg(long, default_value = "snapshot")]
        mode: String,
    },
    /// DELETE /api/atlas/v1/volumes/{id}/mirror
    VolumeMirrorDisable { id: String },
    /// GET /api/atlas/v1/databridge/sources
    DatabridgeSources,
    /// GET /api/atlas/v1/databridge/plans
    DatabridgePlans,
    /// POST /api/atlas/v1/databridge/plans/{id}/{stage} — stage ∈ assess|provision|full-load|cdc-start|cdc-stop|cdc-restart|validate|cutover|rollback
    DatabridgeStage { plan_id: String, stage: String },
    /// Talk to atlas-io-agent (eBPF storage sensor)
    Io {
        #[command(subcommand)]
        cmd: IoCommand,
    },
}

#[derive(Subcommand)]
enum IoCommand {
    /// GET /io/health
    Health,
    /// GET /io/summary
    Summary,
    /// GET /io/histograms
    Histograms,
    /// GET /io/workloads
    Workloads,
    /// GET /io/rca
    Rca {
        #[arg(long)]
        volume: Option<String>,
    },
    /// GET /io/ai — heuristic I/O shape per device (checkpoint-like writes, bulk reads, small reads)
    Ai,
    /// GET /io/coverage
    Coverage,
    /// GET /io/native — kernel-aggregated Atlas Native I/O and recent slow requests
    Native,
    /// GET /io/leases
    Leases,
    /// GET /metrics
    Metrics,
}

async fn io_request(
    client: &reqwest::Client,
    io_url: &str,
    token: Option<&str>,
    cmd: &IoCommand,
) -> Result<()> {
    let base = io_url.trim_end_matches('/');
    let path = match cmd {
        IoCommand::Health => "/io/health",
        IoCommand::Summary => "/io/summary",
        IoCommand::Histograms => "/io/histograms",
        IoCommand::Workloads => "/io/workloads",
        IoCommand::Rca { volume } => {
            return get_print(
                client,
                token,
                &match volume {
                    Some(v) => format!("{base}/io/rca?volume={v}"),
                    None => format!("{base}/io/rca"),
                },
            )
            .await;
        }
        IoCommand::Ai => "/io/ai",
        IoCommand::Coverage => "/io/coverage",
        IoCommand::Native => "/io/native",
        IoCommand::Leases => "/io/leases",
        IoCommand::Metrics => "/metrics",
    };
    get_print(client, token, &format!("{base}{path}")).await
}

async fn get_print(client: &reqwest::Client, token: Option<&str>, url: &str) -> Result<()> {
    let mut req = client.get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("request to {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{body}"),
    }
    if !status.is_success() {
        anyhow::bail!("request failed: HTTP {status}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let base = cli.base_url.trim_end_matches('/');

    if let Command::Io { cmd } = &cli.command {
        return io_request(&client, &cli.io_url, cli.token.as_deref(), cmd).await;
    }

    let (method, path, body): (&str, String, Option<serde_json::Value>) = match &cli.command {
        Command::Health => ("GET", "/health".to_string(), None),
        Command::Ready => ("GET", "/readyz".to_string(), None),
        Command::Version => ("GET", "/version".to_string(), None),
        Command::Backends => ("GET", "/api/atlas/v1/backends".to_string(), None),
        Command::BackendsSummary => ("GET", "/api/atlas/v1/backends/summary".to_string(), None),
        Command::Discover { backend } => (
            "POST",
            format!("/api/atlas/v1/backends/{backend}/discover"),
            None,
        ),
        Command::Clusters => ("GET", "/api/atlas/v1/clusters".to_string(), None),
        Command::Pools => ("GET", "/api/atlas/v1/pools".to_string(), None),
        Command::Osds => ("GET", "/api/atlas/v1/osds".to_string(), None),
        Command::Volumes => ("GET", "/api/atlas/v1/volumes".to_string(), None),
        Command::StorageClasses => ("GET", "/api/atlas/v1/storage-classes".to_string(), None),
        Command::Metrics => ("GET", "/api/atlas/v1/metrics/summary".to_string(), None),
        Command::Alerts { state } => (
            "GET",
            match state {
                Some(st) => format!("/api/atlas/v1/alerts?state={st}"),
                None => "/api/atlas/v1/alerts".to_string(),
            },
            None,
        ),
        Command::CephMetrics { prefix } => (
            "GET",
            match prefix {
                Some(p) => format!("/api/atlas/v1/metrics/ceph?prefix={p}"),
                None => "/api/atlas/v1/metrics/ceph".to_string(),
            },
            None,
        ),
        Command::History { minutes } => (
            "GET",
            format!("/api/atlas/v1/metrics/history?minutes={minutes}"),
            None,
        ),
        Command::Forecast { minutes } => (
            "GET",
            format!("/api/atlas/v1/metrics/forecast?minutes={minutes}"),
            None,
        ),
        Command::SelfMetrics => ("GET", "/metrics".to_string(), None),
        Command::CephStatus => ("GET", "/api/atlas/v1/ceph/status".to_string(), None),
        Command::CephOsdTree => ("GET", "/api/atlas/v1/ceph/osd-tree".to_string(), None),
        Command::CephOsdDf => ("GET", "/api/atlas/v1/ceph/osd-df".to_string(), None),
        Command::CephDevices => ("GET", "/api/atlas/v1/ceph/devices".to_string(), None),
        Command::CephNvme => ("GET", "/api/atlas/v1/ceph/nvme".to_string(), None),
        Command::CephNvmeof => ("GET", "/api/atlas/v1/ceph/nvmeof".to_string(), None),
        Command::CephNvmeofCreate {
            name,
            storage_class,
            size_gib,
            namespace,
            tenant,
            yes,
        } => (
            "POST",
            "/api/atlas/v1/ceph/nvmeof/namespaces".to_string(),
            Some(serde_json::json!({
                "tenant_id": tenant,
                "name": name,
                "namespace": namespace,
                "storage_class": storage_class,
                "size_bytes": size_gib * 1024 * 1024 * 1024,
                "confirm": yes,
            })),
        ),
        Command::CephOsdObjectstore => ("GET", "/api/atlas/v1/ceph/osd-objectstore".to_string(), None),
        Command::CephSizing => ("GET", "/api/atlas/v1/ceph/sizing".to_string(), None),
        Command::IoAi => ("GET", "/api/atlas/v1/io/ai".to_string(), None),
        Command::CephDf => ("GET", "/api/atlas/v1/ceph/df".to_string(), None),
        Command::Policies => ("GET", "/api/atlas/v1/policies".to_string(), None),
        Command::ScheduleSnapshots {
            volume_id,
            interval_secs,
            keep,
        } => (
            "POST",
            format!("/api/atlas/v1/volumes/{volume_id}/schedule"),
            Some(serde_json::json!({ "interval_secs": interval_secs, "keep": keep })),
        ),
        Command::ScheduleBackups {
            volume_id,
            bucket_id,
            interval_secs,
            keep,
            mode,
        } => (
            "POST",
            format!("/api/atlas/v1/volumes/{volume_id}/schedule"),
            Some(serde_json::json!({
                "kind": "backup", "bucket_id": bucket_id,
                "interval_secs": interval_secs, "keep": keep, "mode": mode
            })),
        ),
        Command::Schedules { volume_id } => (
            "GET",
            match volume_id {
                Some(v) => format!("/api/atlas/v1/schedules?volume_id={v}"),
                None => "/api/atlas/v1/schedules".to_string(),
            },
            None,
        ),
        Command::DeleteSchedule { id } => ("DELETE", format!("/api/atlas/v1/schedules/{id}"), None),
        Command::IssueToken {
            subject,
            role,
            ttl_secs,
        } => (
            "POST",
            "/api/atlas/v1/auth/tokens".to_string(),
            Some(serde_json::json!({ "subject": subject, "role": role, "ttl_secs": ttl_secs })),
        ),
        Command::RefreshUsage => ("POST", "/api/atlas/v1/rbd-usage/refresh".to_string(), None),
        Command::RbdImages { pool } => (
            "GET",
            match pool {
                Some(p) => format!("/api/atlas/v1/rbd-images?pool={p}"),
                None => "/api/atlas/v1/rbd-images".to_string(),
            },
            None,
        ),
        Command::CreateRbdImage {
            name,
            size_gib,
            pool,
        } => (
            "POST",
            "/api/atlas/v1/rbd-images".to_string(),
            Some(serde_json::json!({
                "name": name, "size_bytes": size_gib * 1024 * 1024 * 1024, "pool": pool
            })),
        ),
        Command::DeleteRbdImage { pool, image } => (
            "DELETE",
            format!("/api/atlas/v1/rbd-images/{pool}/{image}"),
            None,
        ),
        Command::CloneRbdImage {
            pool,
            image,
            name,
            snap,
        } => (
            "POST",
            format!("/api/atlas/v1/rbd-images/{pool}/{image}/clone"),
            Some(serde_json::json!({ "name": name, "snap": snap })),
        ),
        Command::ResizeRbdImage {
            pool,
            image,
            size_gib,
        } => (
            "POST",
            format!("/api/atlas/v1/rbd-images/{pool}/{image}/resize"),
            Some(serde_json::json!({ "size_bytes": size_gib * 1024 * 1024 * 1024 })),
        ),
        Command::FlattenRbdImage { pool, image } => (
            "POST",
            format!("/api/atlas/v1/rbd-images/{pool}/{image}/flatten"),
            None,
        ),
        Command::RbdSnaps { pool, image, name } => match name {
            Some(n) => (
                "POST",
                format!("/api/atlas/v1/rbd-images/{pool}/{image}/snapshots"),
                Some(serde_json::json!({ "name": n })),
            ),
            None => (
                "GET",
                format!("/api/atlas/v1/rbd-images/{pool}/{image}/snapshots"),
                None,
            ),
        },
        Command::RollbackRbdImage { pool, image, snap } => (
            "POST",
            format!("/api/atlas/v1/rbd-images/{pool}/{image}/rollback"),
            Some(serde_json::json!({ "name": snap })),
        ),
        Command::BucketStats { id } => ("GET", format!("/api/atlas/v1/buckets/{id}/stats"), None),
        Command::BucketObjects { id, prefix } => (
            "GET",
            match prefix {
                Some(p) => format!("/api/atlas/v1/buckets/{id}/objects?prefix={p}"),
                None => format!("/api/atlas/v1/buckets/{id}/objects"),
            },
            None,
        ),
        Command::Tenants => ("GET", "/api/atlas/v1/tenants".to_string(), None),
        Command::VolumeBindings { id } => {
            ("GET", format!("/api/atlas/v1/volumes/{id}/bindings"), None)
        }
        Command::VolumeLabels { id } => ("GET", format!("/api/atlas/v1/volumes/{id}/labels"), None),
        Command::SetVolumeLabel { id, key, value } => (
            "PUT",
            format!("/api/atlas/v1/volumes/{id}/labels"),
            Some(serde_json::json!({ key: value })),
        ),
        Command::Audit {
            actor,
            action,
            resource_type,
            resource_id,
            limit,
        } => {
            let mut qs = vec![format!("limit={limit}")];
            if let Some(a) = actor {
                qs.push(format!("actor={a}"));
            }
            if let Some(a) = action {
                qs.push(format!("action={a}"));
            }
            if let Some(t) = resource_type {
                qs.push(format!("resource_type={t}"));
            }
            if let Some(r) = resource_id {
                qs.push(format!("resource_id={r}"));
            }
            ("GET", format!("/api/atlas/v1/audit?{}", qs.join("&")), None)
        }
        Command::TenantPolicies { tenant_id } => (
            "GET",
            format!("/api/atlas/v1/tenants/{tenant_id}/policies"),
            None,
        ),
        Command::SetTenantPolicy {
            tenant_id,
            intent,
            storage_class,
            access_mode,
            volume_mode,
        } => (
            "PUT",
            format!("/api/atlas/v1/tenants/{tenant_id}/policies/{intent}"),
            Some(serde_json::json!({
                "storage_class": storage_class, "access_mode": access_mode, "volume_mode": volume_mode
            })),
        ),
        Command::Quota { tenant_id } => (
            "GET",
            format!("/api/atlas/v1/tenants/{tenant_id}/quota"),
            None,
        ),
        Command::SetQuota {
            tenant_id,
            max_bytes,
            max_volumes,
        } => (
            "PUT",
            format!("/api/atlas/v1/tenants/{tenant_id}/quota"),
            Some(serde_json::json!({ "max_bytes": max_bytes, "max_volumes": max_volumes })),
        ),
        Command::Jobs { id } => match id {
            Some(id) => ("GET", format!("/api/atlas/v1/jobs/{id}"), None),
            None => ("GET", "/api/atlas/v1/jobs".to_string(), None),
        },
        Command::CancelJob { id } => ("POST", format!("/api/atlas/v1/jobs/{id}/cancel"), None),
        Command::Snapshots => ("GET", "/api/atlas/v1/snapshots".to_string(), None),
        Command::CreateVolume {
            name,
            size_gib,
            kind,
            backend,
            policy,
            namespace,
            tenant,
        } => (
            "POST",
            "/api/atlas/v1/volumes".to_string(),
            Some(serde_json::json!({
                "tenant_id": tenant,
                "name": name,
                "size_bytes": size_gib * 1024 * 1024 * 1024,
                "kind": kind,
                "policy": policy,
                "kubernetes": {
                    "namespace": namespace,
                    "create_pvc": backend.is_none(),
                    "backend_id": backend,
                }
            })),
        ),
        Command::SnapshotVolume { volume_id, name } => (
            "POST",
            format!("/api/atlas/v1/volumes/{volume_id}/snapshots"),
            Some(serde_json::json!({ "name": name })),
        ),
        Command::DeleteVolume { id } => (
            "DELETE",
            format!("/api/atlas/v1/volumes/{id}?confirm=true"),
            None,
        ),
        Command::CloneSnapshot {
            snapshot_id,
            name,
            namespace,
        } => (
            "POST",
            format!("/api/atlas/v1/snapshots/{snapshot_id}/clone"),
            Some(serde_json::json!({ "name": name, "namespace": namespace })),
        ),
        Command::RestoreSnapshot { snapshot_id, name } => (
            "POST",
            format!("/api/atlas/v1/snapshots/{snapshot_id}/restore"),
            Some(serde_json::json!({ "name": name })),
        ),
        Command::DeleteSnapshot { id, force } => (
            "DELETE",
            format!("/api/atlas/v1/snapshots/{id}?force={force}"),
            None,
        ),
        Command::LockSnapshot { id, retain_secs } => (
            "POST",
            format!("/api/atlas/v1/snapshots/{id}/lock"),
            Some(serde_json::json!({ "retain_secs": retain_secs })),
        ),
        Command::NativeReduction => ("GET", "/api/atlas/v1/native/reduction".to_string(), None),
        Command::DedupEstimate { pool_id, max_bytes, max_secs, background: true } => (
            "POST",
            format!("/api/atlas/v1/pools/{pool_id}/dedup-estimate"),
            Some(serde_json::json!({ "max_bytes": max_bytes, "max_secs": max_secs })),
        ),
        Command::DedupEstimate { pool_id, max_bytes, max_secs, background: false } => {
            let mut params = Vec::new();
            if let Some(b) = max_bytes {
                params.push(format!("max_bytes={b}"));
            }
            if let Some(t) = max_secs {
                params.push(format!("max_secs={t}"));
            }
            let qs = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
            ("GET", format!("/api/atlas/v1/pools/{pool_id}/dedup-estimate{qs}"), None)
        }
        Command::Buckets => ("GET", "/api/atlas/v1/buckets".to_string(), None),
        Command::DeleteBucket { id, force } => (
            "DELETE",
            format!("/api/atlas/v1/buckets/{id}?force={force}"),
            None,
        ),
        Command::CreateBucket {
            name,
            namespace,
            max_objects,
            max_size,
        } => (
            "POST",
            "/api/atlas/v1/buckets".to_string(),
            Some(serde_json::json!({
                "name": name, "namespace": namespace,
                "max_objects": max_objects, "max_size": max_size
            })),
        ),
        Command::CephPools => ("GET", "/api/atlas/v1/ceph/pools".to_string(), None),
        Command::CreateCephPool {
            name,
            namespace,
            storage_class,
            replicated_size,
            failure_domain,
            device_class,
        } => (
            "POST",
            "/api/atlas/v1/ceph/pools".to_string(),
            Some(serde_json::json!({
                "name": name, "namespace": namespace, "storage_class": storage_class,
                "replicated_size": replicated_size, "failure_domain": failure_domain,
                "device_class": device_class
            })),
        ),
        Command::DeleteCephPool { name, force } => (
            "DELETE",
            format!("/api/atlas/v1/ceph/pools/{name}?force={force}"),
            None,
        ),
        Command::CephFilesystems => ("GET", "/api/atlas/v1/ceph/filesystems".to_string(), None),
        Command::CreateCephFilesystem {
            name,
            namespace,
            storage_class,
            data_pool_name,
            replicated_size,
        } => (
            "POST",
            "/api/atlas/v1/ceph/filesystems".to_string(),
            Some(serde_json::json!({
                "name": name, "namespace": namespace, "storage_class": storage_class,
                "data_pool_name": data_pool_name, "replicated_size": replicated_size
            })),
        ),
        Command::DeleteCephFilesystem { name, force } => (
            "DELETE",
            format!("/api/atlas/v1/ceph/filesystems/{name}?force={force}"),
            None,
        ),
        Command::CephObjectStores => ("GET", "/api/atlas/v1/ceph/object-stores".to_string(), None),
        Command::CreateCephObjectStore {
            name,
            namespace,
            storage_class,
            replicated_size,
            gateway_port,
            gateway_instances,
        } => (
            "POST",
            "/api/atlas/v1/ceph/object-stores".to_string(),
            Some(serde_json::json!({
                "name": name, "namespace": namespace, "storage_class": storage_class,
                "replicated_size": replicated_size, "gateway_port": gateway_port,
                "gateway_instances": gateway_instances
            })),
        ),
        Command::DeleteCephObjectStore { name, force } => (
            "DELETE",
            format!("/api/atlas/v1/ceph/object-stores/{name}?force={force}"),
            None,
        ),
        Command::BackupVolume {
            volume_id,
            bucket_id,
            mode,
            keep,
            max_age_secs,
        } => (
            "POST",
            "/api/atlas/v1/backup-jobs".to_string(),
            Some(serde_json::json!({
                "volume_id": volume_id, "bucket_id": bucket_id, "mode": mode,
                "keep": keep, "max_age_secs": max_age_secs
            })),
        ),
        Command::Backups => ("GET", "/api/atlas/v1/backups".to_string(), None),
        Command::DeleteBackup { id } => ("DELETE", format!("/api/atlas/v1/backups/{id}"), None),
        Command::BackupDownload { id, what } => (
            "GET",
            format!("/api/atlas/v1/backups/{id}/download?what={what}"),
            None,
        ),
        Command::RestoreBackup {
            backup_id,
            name,
            mode,
        } => (
            "POST",
            "/api/atlas/v1/restore-jobs".to_string(),
            Some(serde_json::json!({ "backup_id": backup_id, "name": name, "mode": mode })),
        ),
        Command::Maintenance => ("GET", "/api/atlas/v1/maintenance".to_string(), None),
        Command::SetMaintenance { paused } => (
            "POST",
            "/api/atlas/v1/maintenance".to_string(),
            Some(serde_json::json!({ "paused": paused })),
        ),
        Command::CordonBackend { id } => {
            ("POST", format!("/api/atlas/v1/backends/{id}/cordon"), None)
        }
        Command::UncordonBackend { id } => (
            "POST",
            format!("/api/atlas/v1/backends/{id}/uncordon"),
            None,
        ),
        Command::UpgradePreflight => ("GET", "/api/atlas/v1/upgrade/preflight".to_string(), None),
        Command::Orphans => ("GET", "/api/atlas/v1/maintenance/orphans".to_string(), None),
        Command::DrPeers => ("GET", "/api/atlas/v1/dr/peers".to_string(), None),
        Command::DrRegisterPeer {
            name,
            cluster_fsid,
            secret_ref,
        } => (
            "POST",
            "/api/atlas/v1/dr/peers".to_string(),
            Some(serde_json::json!({
                "name": name, "cluster_fsid": cluster_fsid, "secret_ref": secret_ref
            })),
        ),
        Command::DrPreflight => ("GET", "/api/atlas/v1/dr/preflight".to_string(), None),
        Command::DrStatus => ("GET", "/api/atlas/v1/dr/status".to_string(), None),
        Command::DrMirrors => ("GET", "/api/atlas/v1/dr/mirrors".to_string(), None),
        Command::DrMirrorStatus { id } => {
            ("GET", format!("/api/atlas/v1/dr/mirrors/{id}/status"), None)
        }
        Command::DrPoolMirroring { pool } => (
            "GET",
            format!("/api/atlas/v1/dr/pools/{pool}/mirroring"),
            None,
        ),
        Command::DrSetPoolMirroring { pool, mode } => (
            "PUT",
            format!("/api/atlas/v1/dr/pools/{pool}/mirroring"),
            Some(serde_json::json!({ "mode": mode })),
        ),
        Command::VolumeGroups => ("GET", "/api/atlas/v1/volume-groups".to_string(), None),
        Command::VolumeGroupCreate { name, volume_ids } => (
            "POST",
            "/api/atlas/v1/volume-groups".to_string(),
            Some(serde_json::json!({ "name": name, "volume_ids": volume_ids })),
        ),
        Command::VolumeGroup { id } => ("GET", format!("/api/atlas/v1/volume-groups/{id}"), None),
        Command::VolumeGroupDelete { id } => {
            ("DELETE", format!("/api/atlas/v1/volume-groups/{id}"), None)
        }
        Command::VolumeGroupSnapshot { id, name } => (
            "POST",
            format!("/api/atlas/v1/volume-groups/{id}/snapshots"),
            Some(serde_json::json!({ "name": name })),
        ),
        Command::VolumeGroupSnapshotDelete { id, name } => (
            "DELETE",
            format!("/api/atlas/v1/volume-groups/{id}/snapshots/{name}"),
            None,
        ),
        Command::VolumeGroupRollback { id, name } => (
            "POST",
            format!("/api/atlas/v1/volume-groups/{id}/snapshots/{name}/rollback"),
            Some(serde_json::json!({ "confirm": true })),
        ),
        Command::DrPromote {
            id,
            force,
            peer_replayed,
        } => (
            "POST",
            format!(
                "/api/atlas/v1/dr/mirrors/{id}/promote?force={force}&peer_replayed={peer_replayed}"
            ),
            None,
        ),
        Command::DrDemote { id } => (
            "POST",
            format!("/api/atlas/v1/dr/mirrors/{id}/demote"),
            None,
        ),
        Command::DrFailover {
            mirror_id,
            force,
            peer_replayed,
        } => (
            "POST",
            "/api/atlas/v1/dr/failover".to_string(),
            Some(serde_json::json!({
                "mirror_id": mirror_id, "confirm": true, "force": force,
                "peer_replayed": peer_replayed
            })),
        ),
        Command::DrSetRpo { id, rpo_seconds } => (
            "POST",
            format!("/api/atlas/v1/dr/mirrors/{id}/rpo"),
            Some(serde_json::json!({ "rpo_seconds": rpo_seconds })),
        ),
        Command::VolumeMirrorEnable { id, peer, mode } => (
            "POST",
            format!("/api/atlas/v1/volumes/{id}/mirror?mode={mode}&peer={peer}"),
            None,
        ),
        Command::VolumeMirrorDisable { id } => {
            ("DELETE", format!("/api/atlas/v1/volumes/{id}/mirror"), None)
        }
        Command::DatabridgeSources => ("GET", "/api/atlas/v1/databridge/sources".to_string(), None),
        Command::DatabridgePlans => ("GET", "/api/atlas/v1/databridge/plans".to_string(), None),
        Command::Io { .. } => unreachable!("io handled above"),
        Command::DatabridgeStage { plan_id, stage } => {
            let path = match stage.as_str() {
                "assess" => format!("/api/atlas/v1/databridge/plans/{plan_id}/assess"),
                "provision" => format!("/api/atlas/v1/databridge/plans/{plan_id}/provision"),
                "full-load" | "fullload" => {
                    format!("/api/atlas/v1/databridge/plans/{plan_id}/full-load")
                }
                "cdc-start" | "cdc/start" => {
                    format!("/api/atlas/v1/databridge/plans/{plan_id}/cdc/start")
                }
                "cdc-stop" | "cdc/stop" => {
                    format!("/api/atlas/v1/databridge/plans/{plan_id}/cdc/stop")
                }
                "cdc-restart" | "cdc/restart" => {
                    format!("/api/atlas/v1/databridge/plans/{plan_id}/cdc/restart")
                }
                "validate" => format!("/api/atlas/v1/databridge/plans/{plan_id}/validate"),
                "cutover" => format!("/api/atlas/v1/databridge/plans/{plan_id}/cutover"),
                "rollback" => format!("/api/atlas/v1/databridge/plans/{plan_id}/rollback"),
                other => {
                    anyhow::bail!(
                        "unknown databridge stage '{other}' (assess|provision|full-load|cdc-start|cdc-stop|cdc-restart|validate|cutover|rollback)"
                    );
                }
            };
            ("POST", path, None)
        }
    };

    let url = format!("{base}{path}");
    let mut req = match method {
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    if let Some(b) = &body {
        req = req.json(b);
    }
    if let Some(token) = &cli.token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("request to {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    // Pretty-print JSON when possible.
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{body}"),
    }

    if !status.is_success() {
        anyhow::bail!("request failed: HTTP {status}");
    }
    Ok(())
}
