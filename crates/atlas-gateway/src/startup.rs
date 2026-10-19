// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Startup wiring shared by the binary and integration tests: open the DB, register the Ceph
//! backend + driver, optionally attach a live Kubernetes driver, and optionally run an initial
//! discovery pass.

use std::sync::Arc;

use anyhow::{Context, Result};
use atlas_api_types::{BackendMode, BackendType, Capabilities, StorageBackend};
use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_driver_ceph::{FakeCephDriver, RealCephDriver};
use atlas_driver_core::{DriverRegistry, StorageDriver};
use atlas_driver_k8s::K8sDriver;
use atlas_driver_longhorn::LonghornDriver;
use atlas_driver_nfs::{FakeNfsDriver, RealNfsDriver};
use atlas_driver_zfs::{FakeZfsDriver, RealZfsDriver};

use crate::state::AppState;

/// The single Ceph backend id used by the MVP.
pub const CEPH_BACKEND_ID: &str = "bkd_ceph_lab";
/// Optional second NFS backend id (enabled via `ATLAS_NFS_ENABLE`).
pub const NFS_BACKEND_ID: &str = "bkd_nfs_lab";
/// Optional third ZFS backend id (enabled via `ATLAS_ZFS_ENABLE`).
pub const ZFS_BACKEND_ID: &str = "bkd_zfs_lab";
pub const LONGHORN_BACKEND_ID: &str = "bkd_longhorn";
/// atlas-native cluster backend id (enabled via `ATLAS_NATIVE_ENABLE`).
pub const NATIVE_BACKEND_ID: &str = "bkd_native";
pub const WEKA_BACKEND_ID: &str = "bkd_weka";
/// The Mac this gateway runs on (enabled via `ATLAS_MACOS_ENABLE`).
pub const MACOS_BACKEND_ID: &str = "bkd_macos";
/// Retired RustFS backend id. Kept so persisted inventory / jobs still parse.
/// New buckets default to [`CEPH_BACKEND_ID`] (RGW).
pub const RUSTFS_BACKEND_ID: &str = "bkd_rustfs_lab";

/// Builds the read-only WEKA driver when `ATLAS_WEKA_ENABLE=1`: `ATLAS_WEKA_DRIVER_MODE` (`fake`
/// default, `real`), `ATLAS_WEKA_ENDPOINT` (API base, e.g. `https://weka01:14000/api/v2`),
/// `ATLAS_WEKA_USERNAME`, `ATLAS_WEKA_PASSWORD_FILE`, `ATLAS_WEKA_ORG`, `ATLAS_WEKA_CA_CERT`,
/// `ATLAS_WEKA_TIMEOUT_SECS` (10).
pub fn weka_driver_from_env() -> Result<Option<Arc<dyn StorageDriver>>> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if !var("ATLAS_WEKA_ENABLE").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")) {
        return Ok(None);
    }
    let mode = var("ATLAS_WEKA_DRIVER_MODE").unwrap_or_default();
    if atlas_common::config::DriverMode::from_env_str(&mode)
        == atlas_common::config::DriverMode::Fake
    {
        return Ok(Some(Arc::new(atlas_driver_weka::WekaDriver::fake(WEKA_BACKEND_ID))));
    }
    let endpoint = var("ATLAS_WEKA_ENDPOINT").context("ATLAS_WEKA_ENDPOINT is required in real mode")?;
    let username = var("ATLAS_WEKA_USERNAME").context("ATLAS_WEKA_USERNAME is required in real mode")?;
    let password_file =
        var("ATLAS_WEKA_PASSWORD_FILE").context("ATLAS_WEKA_PASSWORD_FILE is required in real mode")?;
    let password = std::fs::read_to_string(&password_file)
        .with_context(|| format!("ATLAS_WEKA_PASSWORD_FILE={password_file}"))?
        .trim()
        .to_string();
    let ca_pem = var("ATLAS_WEKA_CA_CERT")
        .map(|p| std::fs::read(&p).with_context(|| format!("ATLAS_WEKA_CA_CERT={p}")))
        .transpose()?;
    let timeout = var("ATLAS_WEKA_TIMEOUT_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let driver = atlas_driver_weka::WekaDriver::real(
        WEKA_BACKEND_ID,
        atlas_driver_weka::WekaConfig {
            endpoint,
            username,
            password,
            org: var("ATLAS_WEKA_ORG"),
            ca_pem,
            timeout: std::time::Duration::from_secs(timeout),
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(Arc::new(driver)))
}

/// Builds the macOS disk driver when `ATLAS_MACOS_ENABLE=1`: `ATLAS_MACOS_DRIVER_MODE` (`fake`
/// default, `real`: runs `diskutil` on this Mac), `ATLAS_MACOS_HOST` (display name, default
/// `localhost`; `mac01.zyvor.lab` in fake mode) and `ATLAS_MACOS_ALLOW_DISK_IMAGES` (also report
/// mounted disk images; off by default).
pub fn macos_driver_from_env() -> Option<Arc<dyn StorageDriver>> {
    let settings = atlas_driver_macos::EnvSettings::from_env();
    settings.enabled.then(|| settings.driver(MACOS_BACKEND_ID))
}

/// One remote Mac per entry of `ATLAS_MACOS_AGENTS` (`name=https://host:5112,name2=...`), each a
/// backend `bkd_macos_<name>`. `ATLAS_MACOS_AGENT_TOKEN` (or `_TOKEN_FILE`) is the bearer token sent to
/// every agent, `ATLAS_MACOS_AGENT_CA_CERT` an extra trusted root. A bad entry is a startup error
/// rather than a silently missing Mac.
pub fn macos_agents_from_env() -> Result<Vec<Arc<dyn StorageDriver>>> {
    Ok(atlas_driver_macos::agents_from_env()
        .map_err(|e| anyhow::anyhow!(e))?
        .into_iter()
        .map(|d| Arc::new(d) as Arc<dyn StorageDriver>)
        .collect())
}

/// Builds the atlas-native driver when `ATLAS_NATIVE_ENABLE=1`:
/// `ATLAS_NATIVE_DRIVER_MODE` (`fake` default, `real`), `ATLAS_NATIVE_ENDPOINTS` (comma-separated
/// node API base URLs), `ATLAS_NATIVE_TOKEN_FILE`, `ATLAS_NATIVE_CA_CERT`,
/// `ATLAS_NATIVE_CLIENT_CERT` + `ATLAS_NATIVE_CLIENT_KEY`, `ATLAS_NATIVE_TIMEOUT_SECS` (10).
pub fn native_driver_from_env() -> Result<Option<Arc<dyn StorageDriver>>> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if !var("ATLAS_NATIVE_ENABLE").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")) {
        return Ok(None);
    }
    let mode = var("ATLAS_NATIVE_DRIVER_MODE").unwrap_or_default();
    if atlas_common::config::DriverMode::from_env_str(&mode)
        == atlas_common::config::DriverMode::Fake
    {
        return Ok(Some(Arc::new(atlas_driver_native::FakeNativeDriver::new(
            NATIVE_BACKEND_ID,
        ))));
    }
    let read = |k: &str| -> Result<Option<Vec<u8>>> {
        var(k)
            .map(|p| std::fs::read(&p).with_context(|| format!("{k}={p}")))
            .transpose()
    };
    let token =
        read("ATLAS_NATIVE_TOKEN_FILE")?.map(|t| String::from_utf8_lossy(&t).trim().to_string());
    let identity = match (
        read("ATLAS_NATIVE_CLIENT_CERT")?,
        read("ATLAS_NATIVE_CLIENT_KEY")?,
    ) {
        (Some(mut cert), Some(key)) => {
            cert.push(b'\n');
            cert.extend_from_slice(&key);
            Some(cert)
        }
        (None, None) => None,
        _ => anyhow::bail!("set both ATLAS_NATIVE_CLIENT_CERT and ATLAS_NATIVE_CLIENT_KEY"),
    };
    let api = atlas_driver_native::HttpApi::new(atlas_driver_native::HttpApiConfig {
        endpoints: var("ATLAS_NATIVE_ENDPOINTS")
            .context("ATLAS_NATIVE_ENDPOINTS is required in real mode")?
            .split(',')
            .map(str::to_string)
            .collect(),
        token,
        ca_pem: read("ATLAS_NATIVE_CA_CERT")?,
        identity_pem: identity,
        timeout: std::time::Duration::from_secs(
            var("ATLAS_NATIVE_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        ),
    })
    .map_err(|e| anyhow::anyhow!("atlas-native driver: {e}"))?;
    Ok(Some(Arc::new(atlas_driver_native::RealNativeDriver::new(
        NATIVE_BACKEND_ID,
        api,
    ))))
}

/// Records the atlas-native backend row and registers its driver.
pub async fn attach_native_driver(
    pool: &sqlx::AnyPool,
    registry: &DriverRegistry,
    driver: Arc<dyn StorageDriver>,
) -> Result<()> {
    atlas_inventory::upsert_backend(
        pool,
        &StorageBackend {
            id: driver.backend_id().into(),
            name: "atlas-native".into(),
            backend_type: BackendType::Native,
            mode: BackendMode::External,
            status: "active".into(),
            capabilities: crate::routes::util::ceph_default_caps(BackendType::Native),
            connection_ref: None,
            cordoned: false,
        },
    )
    .await?;
    registry.register(driver);
    Ok(())
}

/// Records the macOS backend row and registers its driver.
pub async fn attach_macos_driver(
    pool: &sqlx::AnyPool,
    registry: &DriverRegistry,
    driver: Arc<dyn StorageDriver>,
) -> Result<()> {
    atlas_inventory::upsert_backend(
        pool,
        &StorageBackend {
            id: driver.backend_id().into(),
            name: driver.backend_id().trim_start_matches("bkd_").replace('_', "-"),
            backend_type: BackendType::Macos,
            mode: BackendMode::External,
            status: "active".into(),
            capabilities: crate::routes::util::ceph_default_caps(BackendType::Macos),
            connection_ref: None,
            cordoned: false,
        },
    )
    .await?;
    registry.register(driver);
    Ok(())
}

/// Periodic discovery for a driver the Ceph monitor does not cover.
fn spawn_rediscovery(pool: sqlx::AnyPool, driver: Arc<dyn StorageDriver>, interval_secs: u64) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = atlas_discovery::run_discovery(&pool, driver.clone(), None, None).await
            {
                tracing::warn!(backend = driver.backend_id(), "rediscovery failed: {e:#}");
            }
        }
    });
}

pub struct BuildOptions {
    /// Attempt to attach a live Kubernetes driver (disable in unit/integration tests).
    pub enable_k8s: bool,
    /// Run one discovery pass at startup so inventory is populated immediately.
    pub initial_discovery: bool,
    /// Spawn the monitor/alerts worker (disable in tests).
    pub enable_monitor: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            enable_k8s: true,
            initial_discovery: true,
            enable_monitor: true,
        }
    }
}

/// When `ATLAS_SECRETS_BACKEND=vault` (default `env` — a no-op), fetch `jwt-secret`,
/// `admin-password`, and (if OIDC is configured) `oidc-client-secret` from a HashiCorp Vault KV v2
/// path and overwrite the corresponding `Config` fields, so they never need to sit in a plain env
/// var / K8s Secret at all. Reuses the same key names `deploy/k8s/atlas-auth-secret.example.yaml`
/// already uses, so the same Vault secret can back either a plain K8s Secret sync (`deploy/
/// vault-lab/`, via External Secrets Operator) or this direct path — operators choose one, not
/// both. `ATLAS_VAULT_ADDR`/`_TOKEN`/`_SECRET_PATH` are kept out of `Config` (only read here, once,
/// at startup) so they don't touch every test's `Config` literal — same rationale as
/// `ATLAS_STATE_BACKUP_*` in `spawn_state_backup` below. Called *before* `validate_for_start()` in
/// `main.rs` so a Vault-sourced strong secret doesn't get rejected as looking like the dev default.
///
/// Token auth only (`X-Vault-Token`) — the simplest, most universal Vault auth method. AppRole/
/// Kubernetes auth would be more production-grade but are a larger follow-up, not this first slice.
/// See `docs/SECRETS.md`.
pub async fn resolve_vault_secrets(config: &mut Config) -> Result<()> {
    let backend = std::env::var("ATLAS_SECRETS_BACKEND").unwrap_or_default();
    if !backend.trim().eq_ignore_ascii_case("vault") {
        return Ok(());
    }
    let addr = std::env::var("ATLAS_VAULT_ADDR")
        .map_err(|_| anyhow::anyhow!("ATLAS_SECRETS_BACKEND=vault requires ATLAS_VAULT_ADDR"))?;
    let token = std::env::var("ATLAS_VAULT_TOKEN")
        .map_err(|_| anyhow::anyhow!("ATLAS_SECRETS_BACKEND=vault requires ATLAS_VAULT_TOKEN"))?;
    let path = std::env::var("ATLAS_VAULT_SECRET_PATH").map_err(|_| {
        anyhow::anyhow!("ATLAS_SECRETS_BACKEND=vault requires ATLAS_VAULT_SECRET_PATH")
    })?;

    let url = format!(
        "{}/v1/{}",
        addr.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .header("X-Vault-Token", &token)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("vault request to {url} failed"))?;
    if !resp.status().is_success() {
        anyhow::bail!("vault returned HTTP {} for {url}", resp.status());
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("vault response from {url} was not valid JSON"))?;
    // KV v2 wraps the secret's own fields under data.data (the outer "data" is the response
    // envelope, the inner one is the KV v2 secret version's payload).
    let data = body.pointer("/data/data").ok_or_else(|| {
        anyhow::anyhow!("vault response from {url} has no data.data — is this a KV v2 path?")
    })?;

    let mut resolved = Vec::new();
    if let Some(v) = data.get("jwt-secret").and_then(|v| v.as_str()) {
        config.jwt_secret = v.to_string();
        resolved.push("jwt-secret");
    }
    if let Some(v) = data.get("admin-password").and_then(|v| v.as_str()) {
        config.admin_password = v.to_string();
        resolved.push("admin-password");
    }
    if let (Some(oidc), Some(v)) = (
        config.oidc.as_mut(),
        data.get("oidc-client-secret").and_then(|v| v.as_str()),
    ) {
        oidc.client_secret = v.to_string();
        resolved.push("oidc-client-secret");
    }
    tracing::info!(keys = ?resolved, %url, "resolved secrets from Vault");
    Ok(())
}

/// Build fully-wired application state from config.
pub async fn build_state(config: Config, opts: BuildOptions) -> Result<AppState> {
    let pool = atlas_inventory::connect(&config.database_url).await?;
    atlas_inventory::migrate(&pool, &config.database_url).await?;

    // Register the Ceph backend row + driver.
    let (driver, mode): (Arc<dyn StorageDriver>, BackendMode) = match config.ceph_driver_mode {
        CephDriverMode::Real => (
            Arc::new(RealCephDriver::new(CEPH_BACKEND_ID)),
            BackendMode::External,
        ),
        CephDriverMode::Fake => (
            Arc::new(FakeCephDriver::new(CEPH_BACKEND_ID)),
            BackendMode::ManagedRook,
        ),
    };
    let backend = StorageBackend {
        id: CEPH_BACKEND_ID.into(),
        name: "zyvor-ceph-lab".into(),
        backend_type: BackendType::Ceph,
        mode,
        status: "active".into(),
        capabilities: Capabilities {
            block: true,
            file: true,
            object: true,
            snapshots: true,
            clone: true,
            expansion: true,
            replication: true,
        },
        connection_ref: None,
        cordoned: false,
    };
    atlas_inventory::upsert_backend(&pool, &backend).await?;

    let registry = DriverRegistry::new();
    registry.register(driver.clone());

    // Optionally register a second NFS backend — demonstrates that a non-Ceph driver flows through
    // the same discovery → inventory → REST/gRPC surface (PDF §17.2 pluggable drivers).
    let nfs_driver: Option<Arc<dyn StorageDriver>> = if config.nfs_enable {
        let server = config
            .nfs_server
            .clone()
            .unwrap_or_else(|| "nfs01.zyvor.lab".into());
        let exports = if config.nfs_exports.is_empty() {
            vec![
                "/exports/vmstore".to_string(),
                "/exports/backups".to_string(),
            ]
        } else {
            config.nfs_exports.clone()
        };
        let nfs: Arc<dyn StorageDriver> = match config.nfs_driver_mode {
            atlas_common::config::DriverMode::Real => {
                Arc::new(RealNfsDriver::new(NFS_BACKEND_ID, server, exports))
            }
            atlas_common::config::DriverMode::Fake => {
                Arc::new(FakeNfsDriver::new(NFS_BACKEND_ID, server, exports))
            }
        };
        let nfs_backend = StorageBackend {
            id: NFS_BACKEND_ID.into(),
            name: "zyvor-nfs".into(),
            backend_type: BackendType::Nfs,
            mode: BackendMode::External,
            status: "active".into(),
            capabilities: Capabilities {
                block: false,
                file: true,
                object: false,
                snapshots: false,
                clone: false,
                expansion: false,
                replication: false,
            },
            connection_ref: None,
            cordoned: false,
        };
        atlas_inventory::upsert_backend(&pool, &nfs_backend).await?;
        registry.register(nfs.clone());
        tracing::info!("nfs backend registered ({NFS_BACKEND_ID})");
        Some(nfs)
    } else {
        None
    };

    // Optionally register a third ZFS backend — same pluggable-driver contract as Ceph/NFS.
    let zfs_driver: Option<Arc<dyn StorageDriver>> = if config.zfs_enable {
        let host = config
            .zfs_host
            .clone()
            .unwrap_or_else(|| "zfs01.zyvor.lab".into());
        let zpools = if config.zfs_pools.is_empty() {
            vec!["tank".to_string(), "vault".to_string()]
        } else {
            config.zfs_pools.clone()
        };
        let zfs: Arc<dyn StorageDriver> = match config.zfs_driver_mode {
            atlas_common::config::DriverMode::Real => {
                Arc::new(RealZfsDriver::new(ZFS_BACKEND_ID, host, zpools))
            }
            atlas_common::config::DriverMode::Fake => {
                Arc::new(FakeZfsDriver::new(ZFS_BACKEND_ID, host, zpools))
            }
        };
        let zfs_backend = StorageBackend {
            id: ZFS_BACKEND_ID.into(),
            name: "zyvor-zfs".into(),
            backend_type: BackendType::Zfs,
            mode: BackendMode::External,
            status: "active".into(),
            capabilities: Capabilities {
                block: true,
                file: true,
                object: false,
                snapshots: true,
                clone: true,
                expansion: true,
                replication: false,
            },
            connection_ref: None,
            cordoned: false,
        };
        atlas_inventory::upsert_backend(&pool, &zfs_backend).await?;
        registry.register(zfs.clone());
        tracing::info!("zfs backend registered ({ZFS_BACKEND_ID})");
        Some(zfs)
    } else {
        None
    };

    // Attach a live Kubernetes driver if reachable.
    let k8s = if opts.enable_k8s {
        match K8sDriver::try_default().await {
            Ok(d) => {
                tracing::info!("kubernetes driver attached");
                Some(Arc::new(d))
            }
            Err(e) => {
                tracing::warn!("kubernetes driver unavailable: {e}; /storage-classes will 502");
                None
            }
        }
    } else {
        None
    };

    // Use the same in-cluster/kubeconfig identity as the Kubernetes PVC driver. Only register
    // a live Longhorn driver when requested and when Kubernetes is reachable; never fake data.
    let longhorn_driver: Option<Arc<dyn StorageDriver>> = if std::env::var("ATLAS_LONGHORN_ENABLE")
        .ok()
        .is_some_and(|v| v == "1")
    {
        let namespace =
            std::env::var("ATLAS_LONGHORN_NAMESPACE").unwrap_or_else(|_| "longhorn-system".into());
        if !opts.enable_k8s || k8s.is_none() {
            anyhow::bail!("ATLAS_LONGHORN_ENABLE=1 requires a working Kubernetes client");
        }
        let client = kube::Client::try_default()
            .await
            .context("Longhorn Kubernetes client initialization failed")?;
        let longhorn: Arc<dyn StorageDriver> =
            Arc::new(LonghornDriver::new(LONGHORN_BACKEND_ID, namespace, client));
        atlas_inventory::upsert_backend(
            &pool,
            &StorageBackend {
                id: LONGHORN_BACKEND_ID.into(),
                name: "longhorn".into(),
                backend_type: BackendType::Longhorn,
                mode: BackendMode::External,
                status: "active".into(),
                capabilities: Capabilities {
                    block: true,
                    ..Capabilities::default()
                },
                connection_ref: None,
                cordoned: false,
            },
        )
        .await?;
        registry.register(longhorn.clone());
        Some(longhorn)
    } else {
        None
    };

    let native_driver = native_driver_from_env()?;
    if let Some(native) = &native_driver {
        attach_native_driver(&pool, &registry, native.clone()).await?;
    }

    let weka_driver = weka_driver_from_env()?;
    if let Some(weka) = &weka_driver {
        atlas_inventory::upsert_backend(
            &pool,
            &StorageBackend {
                id: WEKA_BACKEND_ID.into(),
                name: "weka".into(),
                backend_type: BackendType::Weka,
                mode: BackendMode::External,
                status: "active".into(),
                capabilities: crate::routes::util::ceph_default_caps(BackendType::Weka),
                connection_ref: None,
                cordoned: false,
            },
        )
        .await?;
        registry.register(weka.clone());
        tracing::info!("weka backend registered ({WEKA_BACKEND_ID})");
    }

    let macos_driver = macos_driver_from_env();
    if let Some(macos) = &macos_driver {
        attach_macos_driver(&pool, &registry, macos.clone()).await?;
        tracing::info!("macos backend registered ({MACOS_BACKEND_ID})");
    }
    let macos_agents = macos_agents_from_env()?;
    for agent in &macos_agents {
        attach_macos_driver(&pool, &registry, agent.clone()).await?;
        tracing::info!("macos agent backend registered ({})", agent.backend_id());
    }

    // Start the async job engine (write path) over the same pool + k8s driver.
    // Durable DB poller (ATLAS_JOB_POLL_SECS) keeps queued work alive across channel loss;
    // tests leave poll_secs=0 so only explicit enqueues/recovery drive the worker.
    let jobs = atlas_jobs::JobEngine::start_with(
        pool.clone(),
        k8s.clone(),
        atlas_jobs::JobEngineOptions {
            poll_secs: config.job_poll_secs,
            stale_secs: config.job_stale_secs,
        },
    );

    // Rate limiter (day-2 governance): per-actor requests/min from ATLAS_RATE_LIMIT_RPM (0 = off).
    let rpm: u32 = std::env::var("ATLAS_RATE_LIMIT_RPM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rate_limit_sync_secs: u64 = std::env::var("ATLAS_RATE_LIMIT_SYNC_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    // OIDC/SSO: discover the issuer now (once) if configured; `None` on any failure just means
    // no "Sign in with SSO" button, not a failed startup.
    let oidc = crate::state::build_oidc_runtime(&config).await;
    let state = AppState {
        pool,
        config: Arc::new(config),
        drivers: Arc::new(registry),
        k8s,
        jobs,
        workers: crate::state::WorkerHealth::default(),
        rate: crate::state::RateLimiter::new(rpm),
        oidc,
    };

    if opts.initial_discovery {
        let rbd_owners = state.rbd_owners().await;
        let rook_pool_kinds = state.rook_pool_kinds().await;
        match atlas_discovery::run_discovery(
            &state.pool,
            driver.clone(),
            rbd_owners.as_ref(),
            rook_pool_kinds.as_ref(),
        )
        .await
        {
            Ok(sum) => tracing::info!(?sum, "initial discovery complete"),
            Err(e) => tracing::warn!("initial discovery failed: {e:#}"),
        }
        if let Some(nfs) = &nfs_driver {
            match atlas_discovery::run_discovery(&state.pool, nfs.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial nfs discovery complete"),
                Err(e) => tracing::warn!("initial nfs discovery failed: {e:#}"),
            }
        }
        if let Some(zfs) = &zfs_driver {
            match atlas_discovery::run_discovery(&state.pool, zfs.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial zfs discovery complete"),
                Err(e) => tracing::warn!("initial zfs discovery failed: {e:#}"),
            }
        }
        if let Some(longhorn) = &longhorn_driver {
            match atlas_discovery::run_discovery(&state.pool, longhorn.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial longhorn discovery complete"),
                Err(e) => tracing::warn!("initial longhorn discovery failed: {e:#}"),
            }
        }
        if let Some(weka) = &weka_driver {
            match atlas_discovery::run_discovery(&state.pool, weka.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial weka discovery complete"),
                Err(e) => tracing::warn!("initial weka discovery failed: {e:#}"),
            }
        }
        if let Some(native) = &native_driver {
            match atlas_discovery::run_discovery(&state.pool, native.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial atlas-native discovery complete"),
                Err(e) => tracing::warn!("initial atlas-native discovery failed: {e:#}"),
            }
        }
        if let Some(macos) = &macos_driver {
            match atlas_discovery::run_discovery(&state.pool, macos.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial macos discovery complete"),
                Err(e) => tracing::warn!("initial macos discovery failed: {e:#}"),
            }
        }
        for agent in &macos_agents {
            match atlas_discovery::run_discovery(&state.pool, agent.clone(), None, None).await {
                Ok(sum) => tracing::info!(?sum, "initial macos agent discovery complete"),
                Err(e) => tracing::warn!("macos agent {} discovery failed: {e:#}", agent.backend_id()),
            }
        }
    }

    // Start the monitor/alerts worker (periodic discovery + alert-rule evaluation).
    if opts.enable_monitor {
        // HA leader election: only the leader replica runs the mutating periodic workers, so a
        // multi-replica deployment doesn't double-fire scheduled jobs. On single-replica SQLite this
        // instance always wins. `is_leader` starts false and flips true on the first lease acquire.
        let is_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lease_ttl = (state.config.monitor_interval_secs as i64 * 3).max(30);
        spawn_leader_election(state.pool.clone(), is_leader.clone(), lease_ttl);

        atlas_monitor::spawn(
            state.pool.clone(),
            driver,
            state.config.monitor_interval_secs,
            state.config.ceph_prometheus_url.clone(),
            state.config.alert_webhook_url.clone(),
            is_leader.clone(),
            state.k8s.clone(),
            state.config.rook_namespace.clone(),
            pagerduty_from_env(),
            opsgenie_from_env(),
            std::env::var("ATLAS_SLACK_WEBHOOK_URL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        );
        if let Some(native) = native_driver.clone() {
            spawn_rediscovery(
                state.pool.clone(),
                native,
                state.config.monitor_interval_secs,
            );
        }
        // External drives come and go, so a Mac is rescanned on the same interval.
        if let Some(macos) = macos_driver.clone() {
            spawn_rediscovery(
                state.pool.clone(),
                macos,
                state.config.monitor_interval_secs,
            );
        }
        for agent in macos_agents.clone() {
            spawn_rediscovery(state.pool.clone(), agent, state.config.monitor_interval_secs);
        }
        // Protection-schedule worker: periodic snapshots + retention (shares the job engine).
        atlas_jobs::spawn_scheduler(
            state.pool.clone(),
            state.jobs.clone(),
            state.config.snapshot_tick_secs,
            is_leader.clone(),
        );
        // Metrics-history sampler: append a capacity/IO/job time-series row each monitor tick,
        // pruning to a 48h window, so the Overview trend charts survive restarts + reloads. It also
        // heartbeats so /readyz can observe the periodic-worker cadence is alive.
        spawn_metrics_sampler(
            state.pool.clone(),
            state.config.monitor_interval_secs,
            state.workers.clone(),
        );
        // DataBridge reconciler: advances migration pipelines (edge CR status, full-load/validation
        // Jobs, CDC lag) that the single-shot job engine can't hold open.
        atlas_databridge::reconcile::spawn_reconciler(
            state.pool.clone(),
            state.k8s.clone(),
            state.config.databridge_reconcile_secs,
            is_leader.clone(),
        );
    }

    // Self-state backup: snapshot the control-plane DB to S3/RGW (independent of the monitor block;
    // disabled unless ATLAS_STATE_BACKUP_SECS > 0).
    spawn_state_backup(
        state.pool.clone(),
        state.config.database_url.clone(),
        state.workers.clone(),
    );

    // Audit retention: prune audit rows older than ATLAS_AUDIT_RETENTION_DAYS (0 = keep forever).
    spawn_audit_retention(state.pool.clone());

    // Cross-replica rate-limit sync (day-2 HA governance): see RateLimiter's own doc comment in
    // state.rs. No-ops when rate limiting itself is off (rpm == 0) or ATLAS_RATE_LIMIT_SYNC_SECS=0.
    let replica_id = format!(
        "{}-{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
        std::process::id()
    );
    crate::state::spawn_rate_limit_sync(
        state.pool.clone(),
        state.rate.clone(),
        replica_id,
        rate_limit_sync_secs,
    );

    Ok(state)
}

/// HA leader election: renew the `workers` lease every `ttl/2`s and flip `is_leader`. Runs on every
/// replica so a non-leader can take over when the current leader's lease expires. The instance id is
/// the pod HOSTNAME (unique per replica in k8s) + pid.
fn spawn_leader_election(
    pool: sqlx::AnyPool,
    is_leader: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ttl_secs: i64,
) {
    use std::sync::atomic::Ordering;
    let instance = format!(
        "{}-{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
        std::process::id()
    );
    let renew = (ttl_secs / 2).max(5) as u64;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(renew));
        loop {
            tick.tick().await; // fires immediately, so leadership is acquired at startup
            match atlas_inventory::leader::try_acquire(&pool, "workers", &instance, ttl_secs).await
            {
                Ok(leader) => {
                    let was = is_leader.swap(leader, Ordering::Relaxed);
                    if leader && !was {
                        tracing::info!("acquired worker leadership ({instance})");
                    } else if !leader && was {
                        tracing::warn!("lost worker leadership ({instance})");
                    }
                }
                Err(e) => tracing::warn!("leader election failed: {e:#}"),
            }
        }
    });
}

/// Native PagerDuty alerting sink (`ATLAS_PAGERDUTY_ROUTING_KEY`) — `None` disables it. Kept out
/// of `Config` so it doesn't touch every test's `Config` literal (same pattern as
/// `ATLAS_STATE_BACKUP_*`/`ATLAS_AUDIT_EXPORT_URL` above).
fn pagerduty_from_env() -> Option<atlas_monitor::PagerDutyConfig> {
    let routing_key = std::env::var("ATLAS_PAGERDUTY_ROUTING_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    Some(atlas_monitor::PagerDutyConfig { routing_key })
}

/// Native Opsgenie alerting sink (`ATLAS_OPSGENIE_API_KEY`, optional `ATLAS_OPSGENIE_REGION` —
/// "us" default or "eu") — `None` disables it. Same out-of-`Config` rationale as above.
fn opsgenie_from_env() -> Option<atlas_monitor::OpsgenieConfig> {
    let api_key = std::env::var("ATLAS_OPSGENIE_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let region = std::env::var("ATLAS_OPSGENIE_REGION").unwrap_or_else(|_| "us".into());
    Some(atlas_monitor::OpsgenieConfig { api_key, region })
}

/// Periodically prune audit rows older than `ATLAS_AUDIT_RETENTION_DAYS` (day-2 governance). Runs
/// every 6h; disabled when the var is unset/0 (keep forever). When `ATLAS_AUDIT_EXPORT_URL` is
/// also set, rows are exported to that sink (SIEM/webhook, batched JSON POST) before deletion —
/// if the export fails, nothing is deleted this tick, so a sink outage can't silently lose audit
/// history the way straight-line pruning would. Both vars are kept out of `Config` so they don't
/// touch every test's `Config` literal (same pattern as `ATLAS_STATE_BACKUP_*` below).
fn spawn_audit_retention(pool: sqlx::AnyPool) {
    let days: i64 = std::env::var("ATLAS_AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if days <= 0 {
        return;
    }
    let export_url = std::env::var("ATLAS_AUDIT_EXPORT_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            let result = match &export_url {
                Some(url) => atlas_monitor::audit_export::export_and_prune(&pool, days, url).await,
                None => atlas_inventory::audit::prune(&pool, days).await,
            };
            match result {
                Ok(n) if n > 0 => {
                    tracing::info!("audit retention: pruned {n} row(s) older than {days}d")
                }
                Ok(_) => {}
                Err(e) if export_url.is_some() => {
                    tracing::warn!("audit export failed, rows kept for retry next tick: {e:#}")
                }
                Err(e) => tracing::warn!("audit retention prune failed: {e:#}"),
            }
        }
    });
}

/// Periodically snapshot the control-plane SQLite DB and upload it to S3/RGW so Atlas can restore its
/// own inventory / jobs / audit / quotas / DataBridge state (it backs up tenant volumes but, until
/// now, not itself). Disabled unless `ATLAS_STATE_BACKUP_SECS > 0` and the S3 endpoint/bucket are set.
/// Reads its own env (kept out of `Config` so it doesn't touch every test's `Config` literal).
fn spawn_state_backup(
    pool: sqlx::AnyPool,
    database_url: String,
    workers: crate::state::WorkerHealth,
) {
    let secs: u64 = std::env::var("ATLAS_STATE_BACKUP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if secs == 0 {
        return;
    }
    let endpoint = std::env::var("ATLAS_STATE_BACKUP_ENDPOINT").unwrap_or_default();
    let bucket = std::env::var("ATLAS_STATE_BACKUP_BUCKET").unwrap_or_default();
    if endpoint.is_empty() || bucket.is_empty() {
        tracing::warn!(
            "ATLAS_STATE_BACKUP_SECS is set but ENDPOINT/BUCKET are not — state backup disabled"
        );
        return;
    }
    let access = std::env::var("ATLAS_STATE_BACKUP_ACCESS_KEY").unwrap_or_default();
    let secret = std::env::var("ATLAS_STATE_BACKUP_SECRET_KEY").unwrap_or_default();
    let region = std::env::var("ATLAS_STATE_BACKUP_REGION").unwrap_or_else(|_| "us-east-1".into());
    let prefix =
        std::env::var("ATLAS_STATE_BACKUP_PREFIX").unwrap_or_else(|_| "atlas-state".into());
    let keep: usize = std::env::var("ATLAS_STATE_BACKUP_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);

    tokio::spawn(async move {
        let s3 =
            match atlas_driver_rgw::S3Target::new(&endpoint, &region, &bucket, &access, &secret) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("state backup disabled: invalid S3 target: {e:#}");
                    return;
                }
            };
        workers.set_interval("state_backup", secs);
        tracing::info!(secs, bucket, prefix, keep, "state backup worker started");
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
        loop {
            tick.tick().await;
            workers.beat("state_backup");
            if let Err(e) = backup_state_once(&pool, &database_url, &s3, &prefix, keep).await {
                tracing::warn!("state backup failed: {e:#}");
            }
        }
    });
}

/// One state-backup pass: snapshot the live control-plane DB, upload it, prune to the newest
/// `keep`. Object keys embed a zero-padded epoch so lexicographic order = chronological order.
///
/// The snapshot mechanism branches on backend, same as the existing pattern of shelling out to
/// the `ceph`/`rbd` CLI rather than reimplementing their protocols in Rust: SQLite uses
/// `VACUUM INTO` (an in-process, transactionally-consistent copy); Postgres shells out to
/// `pg_dump --format=custom`, which takes its own consistent MVCC snapshot server-side. `pg_dump`
/// must be on `PATH` in the gateway image when `database_url` is a `postgres://` URL — mirrors
/// the existing requirement that the real-Ceph image bundles the `ceph`/`rbd` CLIs.
async fn backup_state_once(
    pool: &sqlx::AnyPool,
    database_url: &str,
    s3: &atlas_driver_rgw::S3Target,
    prefix: &str,
    keep: usize,
) -> Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let is_postgres = atlas_inventory::is_postgres_url(database_url);
    let ext = if is_postgres { "dump" } else { "db" };
    let snap = std::env::temp_dir().join(format!("atlas-state-{ts}.{ext}"));
    if is_postgres {
        let status = tokio::process::Command::new("pg_dump")
            .arg("--format=custom")
            .arg("--file")
            .arg(&snap)
            .arg(database_url)
            .status()
            .await
            .context("spawn pg_dump")?;
        if !status.success() {
            anyhow::bail!("pg_dump exited with {status}");
        }
    } else {
        // VACUUM INTO writes a transactionally-consistent copy while the DB stays in use.
        // `snap` is a temp path this function itself built from a numeric timestamp — never user input.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "VACUUM INTO '{}'",
            snap.display()
        )))
        .execute(pool)
        .await
        .context("VACUUM INTO snapshot")?;
    }
    // Ensure the destination bucket exists so the first backup does not fail on a missing name.
    if !s3.bucket_exists().await {
        s3.create_bucket()
            .await
            .context("create state backup bucket")?;
    }
    let bytes = tokio::fs::read(&snap).await.context("read snapshot file")?;
    let _ = tokio::fs::remove_file(&snap).await;
    let size = bytes.len();
    let key = format!("{prefix}/atlas-state-{ts:020}.{ext}");
    s3.put_object(&key, bytes)
        .await
        .context("upload snapshot")?;
    tracing::info!("state backup: uploaded {key} ({size} bytes)");

    let mut objs = s3
        .list_objects(Some(&format!("{prefix}/")))
        .await
        .unwrap_or_default();
    objs.sort_by(|a, b| a.0.cmp(&b.0));
    if objs.len() > keep {
        for (old, _) in &objs[..objs.len() - keep] {
            let _ = s3.delete_object(old).await;
        }
    }
    Ok(())
}

/// Periodically persist one `metrics_history` sample derived from the current summary + counters.
/// `interval_secs == 0` disables it (mirrors the monitor/scheduler workers).
fn spawn_metrics_sampler(
    pool: sqlx::AnyPool,
    interval_secs: u64,
    workers: crate::state::WorkerHealth,
) {
    if interval_secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            workers.beat("metrics_sampler");
            if let Err(e) = sample_metrics_history(&pool).await {
                tracing::debug!("metrics-history sample skipped: {e:#}");
            }
        }
    });
}

async fn sample_metrics_history(pool: &sqlx::AnyPool) -> Result<()> {
    let summary = atlas_inventory::metrics_summary(pool).await?;
    let running: i64 = atlas_inventory::jobs::count_by_state(pool)
        .await?
        .into_iter()
        .filter(|(s, _)| matches!(s.as_str(), "running" | "queued" | "verifying"))
        .map(|(_, n)| n)
        .sum();
    let alerts_open = atlas_inventory::alerts::list(pool, Some("open"))
        .await?
        .len() as i64;
    atlas_inventory::metrics::record_history(pool, &summary, running, alerts_open).await?;
    atlas_inventory::metrics::prune_history(pool, 48).await?;
    Ok(())
}
