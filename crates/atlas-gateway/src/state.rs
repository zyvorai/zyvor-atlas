// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Shared application state injected into every axum handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use atlas_common::Config;
use atlas_driver_core::DriverRegistry;
use atlas_driver_k8s::K8sDriver;
use openidconnect::{EndpointMaybeSet, EndpointNotSet, EndpointSet};
use sqlx::AnyPool;

/// `CoreClient::from_provider_metadata(..).set_redirect_uri(..)`'s exact concrete type — the
/// crate's endpoint-presence type-state machinery means this can't be named as the bare
/// `openidconnect::core::CoreClient` alias (that alias defaults every endpoint to
/// `EndpointNotSet`, but discovery always sets the auth/token endpoints and optionally the
/// userinfo endpoint). Determined empirically via a deliberate type-mismatch compile probe
/// against openidconnect 4.0.1 / oauth2 5.0.0 — if either crate is upgraded and this stops
/// compiling, re-derive it the same way rather than guessing.
type DiscoveredOidcClient = openidconnect::core::CoreClient<
    EndpointSet,      // HasAuthUrl — discovery always provides authorization_endpoint
    EndpointNotSet,   // HasDeviceAuthUrl — not part of OIDC discovery
    EndpointNotSet,   // HasIntrospectionUrl — not part of OIDC discovery
    EndpointNotSet,   // HasRevocationUrl — not set unless discovered separately (we don't)
    EndpointMaybeSet, // HasTokenUrl — discovery *should* provide token_endpoint but it's optional
    EndpointMaybeSet, // HasUserInfoUrl — userinfo_endpoint is optional in the discovery doc
>;

/// OIDC login state held between the `/auth/oidc/login` redirect and the `/auth/oidc/callback`
/// return trip, keyed by the CSRF `state` parameter. In-memory only (single-gateway-instance
/// deployment, per the rest of this codebase's HA posture) — a login abandoned mid-flow just
/// leaks one small entry until `sweep_expired` clears it.
pub(crate) struct PendingOidcLogin {
    pub nonce: String,
    pub pkce_verifier: String,
    pub created_at: Instant,
}

/// Runtime OIDC client, built once at startup by discovering the configured issuer. Wraps the
/// discovered `CoreClient` plus the in-flight-login map described above. `AppState.oidc` is
/// `None` whenever `Config.oidc` is `None` (feature not configured) or discovery failed at boot
/// (logged as a warning — the gateway still starts, just without SSO).
pub struct OidcRuntime {
    pub(crate) client: DiscoveredOidcClient,
    pub(crate) http: openidconnect::reqwest::Client,
    pending: Mutex<HashMap<String, PendingOidcLogin>>,
}

const PENDING_LOGIN_TTL_SECS: u64 = 600;

impl OidcRuntime {
    pub(crate) fn new(client: DiscoveredOidcClient, http: openidconnect::reqwest::Client) -> Self {
        Self {
            client,
            http,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn stash(&self, csrf_state: String, nonce: String, pkce_verifier: String) {
        let mut g = match self.pending.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.retain(|_, v| v.created_at.elapsed().as_secs() < PENDING_LOGIN_TTL_SECS);
        g.insert(
            csrf_state,
            PendingOidcLogin {
                nonce,
                pkce_verifier,
                created_at: Instant::now(),
            },
        );
    }

    /// Consume (remove) the pending login for `csrf_state`, if present and not expired.
    pub(crate) fn take(&self, csrf_state: &str) -> Option<PendingOidcLogin> {
        let mut g = self.pending.lock().ok()?;
        let entry = g.remove(csrf_state)?;
        if entry.created_at.elapsed().as_secs() >= PENDING_LOGIN_TTL_SECS {
            return None;
        }
        Some(entry)
    }
}

/// Discover the configured OIDC issuer and build a runtime client. Returns `None` (logged as a
/// warning, never fatal) on any failure — a lab Dex instance being briefly unreachable at boot
/// shouldn't take down the rest of the gateway; the "Sign in with SSO" button just won't appear.
pub async fn build_oidc_runtime(cfg: &Config) -> Option<Arc<OidcRuntime>> {
    let oidc_cfg = cfg.oidc.as_ref()?;
    use openidconnect::core::{CoreClient, CoreProviderMetadata};
    use openidconnect::{ClientId, ClientSecret, IssuerUrl, RedirectUrl};

    let issuer_url = match IssuerUrl::new(oidc_cfg.issuer_url.clone()) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("OIDC issuer URL invalid, SSO disabled: {e}");
            return None;
        }
    };
    let redirect_url = match RedirectUrl::new(oidc_cfg.redirect_url.clone()) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("OIDC redirect URL invalid, SSO disabled: {e}");
            return None;
        }
    };
    // A throwaway/lab IdP is commonly plain HTTP with no reverse proxy in front of it — this
    // client is used server-to-server only (discovery + token exchange), never to fetch
    // anything an end user supplies, so following redirects isn't the SSRF risk it would be for
    // a general-purpose HTTP client. `reqwest::Client` (async) implements openidconnect's
    // `AsyncHttpClient` directly, so no adapter type is needed.
    let http = openidconnect::reqwest::Client::builder()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()
        .ok()?;

    let metadata = match CoreProviderMetadata::discover_async(issuer_url, &http).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "OIDC discovery against {} failed, SSO disabled: {e}",
                oidc_cfg.issuer_url
            );
            return None;
        }
    };

    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(oidc_cfg.client_id.clone()),
        Some(ClientSecret::new(oidc_cfg.client_secret.clone())),
    )
    .set_redirect_uri(redirect_url);

    tracing::info!(issuer = %oidc_cfg.issuer_url, "OIDC/SSO login enabled");
    Some(Arc::new(OidcRuntime::new(client, http)))
}

/// In-process worker heartbeats: each periodic worker stamps its name every tick so `/readyz` can
/// surface a wedged worker. Empty when workers are disabled (e.g. tests), which readyz treats as
/// "nothing to check" rather than a failure.
#[derive(Clone, Default)]
pub struct WorkerHealth {
    beats: Arc<Mutex<HashMap<&'static str, Instant>>>,
    /// Workers whose tick interval is not the monitor interval (e.g. the hourly state backup), so
    /// `/readyz` doesn't flag them stale between two healthy ticks.
    intervals: Arc<Mutex<HashMap<&'static str, u64>>>,
}

impl WorkerHealth {
    /// Record a heartbeat for `worker` (called at the top of each worker loop iteration).
    pub fn beat(&self, worker: &'static str) {
        if let Ok(mut g) = self.beats.lock() {
            g.insert(worker, Instant::now());
        }
    }

    /// Declare `worker`'s own tick interval in seconds.
    pub fn set_interval(&self, worker: &'static str, secs: u64) {
        if let Ok(mut g) = self.intervals.lock() {
            g.insert(worker, secs);
        }
    }

    /// `(worker, seconds_since_last_beat, declared_interval_secs)` for every worker that has beat
    /// at least once.
    pub fn ages(&self) -> Vec<(&'static str, u64, Option<u64>)> {
        let intervals = self
            .intervals
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.beats
            .lock()
            .map(|g| {
                g.iter()
                    .map(|(k, v)| (*k, v.elapsed().as_secs(), intervals.get(k).copied()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Per-actor fixed-window rate limiter (day-2 governance). `rpm == 0` disables it. Keyed by actor id;
/// each 60s window resets the count.
///
/// `allow()` itself stays fully in-process, synchronous, and DB-free — it's called from both the
/// REST `axum` middleware (`auth.rs`) and the gRPC path's `tonic::Interceptor` (`grpc.rs`), and
/// that second call site's trait signature is synchronous with no `.await`; an async DB query
/// there would risk blocking the Tokio runtime it's invoked from. Cross-replica awareness instead
/// comes from `spawn_rate_limit_sync` (`startup.rs`), a periodic *background* task — the only thing
/// that ever touches `rate_limit_counters` — which writes this replica's own current-window counts
/// to the database and reads back each actor's cluster-wide total, flagging any actor already over
/// budget cluster-wide in `globally_limited` so `allow()` can deny them immediately even if this
/// replica's own local count hasn't hit `rpm` yet. Eventually consistent within one sync interval
/// (a few seconds) — a real tradeoff for a governance/abuse-prevention control, not a hard security
/// boundary, and the same one distributed rate limiters generally accept. Single-replica SQLite
/// behaves exactly as before: sync runs, finds only this replica's own counts, and never flags
/// anything the local check wouldn't already have caught.
#[derive(Clone)]
pub struct RateLimiter {
    rpm: u32,
    windows: Arc<Mutex<HashMap<String, (u64, u32)>>>,
    /// actor_id -> the window_minute it was last confirmed over the *cluster-wide* budget for.
    globally_limited: Arc<Mutex<HashMap<String, u64>>>,
}

impl RateLimiter {
    pub fn new(rpm: u32) -> Self {
        Self {
            rpm,
            windows: Arc::new(Mutex::new(HashMap::new())),
            globally_limited: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn current_minute() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0)
    }

    /// Whether the request is allowed. Increments the actor's count for the current minute and
    /// returns false once it exceeds `rpm`, *or* once the last sync confirmed the actor is already
    /// over budget cluster-wide. Always true when disabled (`rpm == 0`).
    pub fn allow(&self, actor: &str) -> bool {
        if self.rpm == 0 {
            return true;
        }
        let minute = Self::current_minute();
        if let Ok(g) = self.globally_limited.lock() {
            if g.get(actor) == Some(&minute) {
                return false;
            }
        }
        let mut g = match self.windows.lock() {
            Ok(g) => g,
            Err(_) => return true, // never lock out on a poisoned mutex
        };
        let entry = g.entry(actor.to_string()).or_insert((minute, 0));
        if entry.0 != minute {
            *entry = (minute, 0);
        }
        entry.1 += 1;
        entry.1 <= self.rpm
    }

    /// This replica's own per-actor counts for the current window — read by `spawn_rate_limit_sync`
    /// to report into the database. Never called from the hot request path.
    fn snapshot_current_window(&self) -> (u64, Vec<(String, u32)>) {
        let minute = Self::current_minute();
        let counts = match self.windows.lock() {
            Ok(g) => g
                .iter()
                .filter(|(_, (w, _))| *w == minute)
                .map(|(actor, (_, count))| (actor.clone(), *count))
                .collect(),
            Err(_) => Vec::new(),
        };
        (minute, counts)
    }

    /// Replace the set of actors known to be over budget cluster-wide for `minute` — called by
    /// `spawn_rate_limit_sync` after reading back the cluster-wide totals. Never called from the
    /// hot request path.
    fn set_globally_limited(&self, minute: u64, actors: impl IntoIterator<Item = String>) {
        if let Ok(mut g) = self.globally_limited.lock() {
            g.retain(|_, w| *w == minute); // drop flags from windows that have since rolled over
            for actor in actors {
                g.insert(actor, minute);
            }
        }
    }
}

/// Periodic background sync between this replica's in-process `RateLimiter` and
/// `rate_limit_counters` — see `RateLimiter`'s doc comment for the full design. `interval_secs ==
/// 0` disables it (mirrors the other periodic workers in `startup.rs`); harmless to run even on a
/// single SQLite replica (it just finds one replica's own counts and confirms what the local check
/// already knew).
pub fn spawn_rate_limit_sync(
    pool: AnyPool,
    limiter: RateLimiter,
    replica_id: String,
    interval_secs: u64,
) {
    if interval_secs == 0 || limiter.rpm == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            sync_rate_limits_once(&pool, &limiter, &replica_id).await;
        }
    });
}

/// One sync tick: report this replica's counts for the current window, read back the cluster-wide
/// totals and flag every actor already over budget. Best effort: a database error is logged and the
/// next tick tries again.
pub async fn sync_rate_limits_once(pool: &AnyPool, limiter: &RateLimiter, replica_id: &str) {
    let (minute, counts) = limiter.snapshot_current_window();
    for (actor, count) in counts {
        if let Err(e) = atlas_inventory::rate_limit::upsert_replica_count(
            pool,
            &actor,
            minute as i64,
            replica_id,
            count as i64,
        )
        .await
        {
            tracing::warn!("rate limit sync: upsert failed for {actor}: {e:#}");
        }
    }
    match atlas_inventory::rate_limit::cluster_totals(pool, minute as i64).await {
        Ok(totals) => {
            let over_budget = totals
                .into_iter()
                .filter(|(_, total)| *total as u32 > limiter.rpm)
                .map(|(actor, _)| actor);
            limiter.set_globally_limited(minute, over_budget);
        }
        Err(e) => tracing::warn!("rate limit sync: cluster_totals failed: {e:#}"),
    }
    // Keep a few minutes of history (covers a sync hiccup) and no more.
    if let Err(e) = atlas_inventory::rate_limit::prune_before(pool, minute as i64 - 5).await {
        tracing::debug!("rate limit sync: prune failed: {e:#}");
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: AnyPool,
    pub config: Arc<Config>,
    /// Backend storage drivers keyed by backend id (Ceph real/fake in the MVP).
    pub drivers: Arc<DriverRegistry>,
    /// Live Kubernetes driver, if a cluster was reachable at startup.
    pub k8s: Option<Arc<K8sDriver>>,
    /// Async job engine (write path).
    pub jobs: atlas_jobs::JobEngine,
    /// Periodic-worker heartbeats for `/readyz`.
    pub workers: WorkerHealth,
    /// Per-actor request rate limiter (day-2 governance).
    pub rate: RateLimiter,
    /// OIDC/SSO login, when configured and discovery succeeded at startup.
    pub oidc: Option<Arc<OidcRuntime>>,
}

impl AppState {
    /// Resolve the driver for a backend id, or the only one when there's a single backend.
    pub fn driver_for(
        &self,
        backend_id: &str,
    ) -> Option<Arc<dyn atlas_driver_core::StorageDriver>> {
        self.drivers.get(backend_id).or_else(|| self.drivers.any())
    }

    /// Build the RBD-image → owning-PVC map from cluster PersistentVolumes, so discovery can
    /// attribute raw `rbd ls` images to their Kubernetes VM disk. `None` when no cluster is
    /// attached or the PV list fails (discovery then just leaves those volumes unattributed).
    pub async fn rbd_owners(&self) -> Option<atlas_discovery::RbdOwners> {
        let k8s = self.k8s.as_ref()?;
        match k8s.rbd_image_owners().await {
            Ok(m) => Some(
                m.into_iter()
                    .map(|(img, o)| (img, (o.namespace, o.pvc_name, o.storage_class)))
                    .collect(),
            ),
            Err(e) => {
                tracing::warn!("rbd_image_owners failed: {e}");
                None
            }
        }
    }

    /// Build the pool-name → precise-kind map from live Rook CRs (`config.rook_namespace`), so
    /// discovery can classify pools exactly instead of guessing from the name. `None` when no
    /// cluster is attached or the CR list fails (discovery then just keeps the name heuristic).
    pub async fn rook_pool_kinds(&self) -> Option<atlas_discovery::RookPoolKinds> {
        let k8s = self.k8s.as_ref()?;
        match k8s.known_rook_pool_kinds(&self.config.rook_namespace).await {
            Ok(m) => Some(
                m.into_iter()
                    .map(|(pool, kind)| (pool, kind.as_str().to_string()))
                    .collect(),
            ),
            Err(e) => {
                tracing::warn!("known_rook_pool_kinds failed: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod rate_limiter_tests {
    use super::RateLimiter;

    /// The exact mechanism `spawn_rate_limit_sync` relies on: a sync tick learning the
    /// cluster-wide total exceeds `rpm` must deny the actor's next request even though this
    /// replica's *own* local count alone would still allow it — proven directly against
    /// `RateLimiter`'s real fields/methods, not a slow/flaky multi-process HTTP simulation (the
    /// DB round trip itself — `cluster_totals` correctly summing per-replica rows — is covered
    /// separately by `atlas_inventory::rate_limit`'s own tests).
    #[tokio::test]
    async fn cluster_wide_flag_denies_even_when_local_count_is_still_under_budget() {
        let limiter = RateLimiter::new(5);
        // This replica's own usage: 4 requests, all still within the per-replica budget alone.
        for _ in 0..4 {
            assert!(limiter.allow("alice"));
        }

        // Simulate what a sync tick learns: added to usage on other replicas, the cluster-wide
        // total for this window is over budget.
        let minute = RateLimiter::current_minute();
        limiter.set_globally_limited(minute, [String::from("alice")]);

        // The local count (now 4, would become 5 — still <= rpm) would allow this request on its
        // own; the cluster-wide flag must deny it anyway.
        assert!(!limiter.allow("alice"));
        // An actor nobody flagged is unaffected.
        assert!(limiter.allow("bob"));
    }

    /// A window rollover must drop a stale flag — `set_globally_limited` filters by the window it
    /// was called with, so a flag from a past minute never lingers into the current one.
    #[tokio::test]
    async fn stale_window_flag_does_not_carry_over() {
        let limiter = RateLimiter::new(5);
        let past_minute = RateLimiter::current_minute().saturating_sub(1);
        limiter.set_globally_limited(past_minute, [String::from("alice")]);
        // A fresh sync for the *current* window with no over-budget actors clears the stale flag.
        limiter.set_globally_limited(RateLimiter::current_minute(), std::iter::empty::<String>());
        assert!(limiter.allow("alice"));
    }

    /// Two replicas (two limiters, one database) each stay under `rpm` alone but are over it
    /// together: after both have synced twice (report, then read back the total) each denies.
    /// SQLite in-process; the SQL is the shared dialect-neutral code the Postgres tests also run.
    #[tokio::test]
    async fn two_replicas_share_a_budget_through_the_database() {
        let db = format!("{}/atlas-budget-sync-{}.db", std::env::temp_dir().display(), std::process::id());
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = atlas_inventory::connect(&url).await.unwrap();
        atlas_inventory::migrate(&pool, &url).await.unwrap();

        let (a, b) = (RateLimiter::new(5), RateLimiter::new(5));
        for _ in 0..3 {
            assert!(a.allow("mcp:call:tok"));
            assert!(b.allow("mcp:call:tok"));
        }
        // Without a sync, neither replica knows about the other: both would still allow.
        for r in [&a, &b] {
            super::sync_rate_limits_once(&pool, r, if std::ptr::eq(r, &a) { "a" } else { "b" }).await;
        }
        for r in [&a, &b] {
            super::sync_rate_limits_once(&pool, r, if std::ptr::eq(r, &a) { "a" } else { "b" }).await;
        }
        assert!(!a.allow("mcp:call:tok"), "6 calls cluster-wide against a budget of 5");
        assert!(!b.allow("mcp:call:tok"));
        assert!(a.allow("mcp:call:other"), "another key is unaffected");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn disabled_limiter_always_allows() {
        let limiter = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(limiter.allow("anyone"));
        }
    }
}
