<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Cross-cluster DR (RBD mirroring)

Atlas exposes a control-plane catalog and failover API for Ceph RBD mirroring. The real
`rbd mirror` CLI paths run as jobs. One-way mirroring was verified live between two Rook Ceph
clusters on 2026-10-04, and two-way mirroring with clean failback on 2026-10-06 (see below). Fake mode (`ATLAS_CEPH_DRIVER_MODE=fake` / `make run`) exercises the full
API and catalog without calling `rbd`.

For atlas-native filesystems, cross-cluster DR is asynchronous replication between two native
clusters instead: [`NATIVE_REPLICATION.md`](NATIVE_REPLICATION.md).

## Status

| Layer | State |
|---|---|
| Peers / mirrors catalog | Done |
| Enable / disable / promote / demote API + jobs | Done |
| Role transition guards + force promote | Done |
| Preflight + one-click failover runbook | Done (`control_plane_ready`; `dataplane_verified` now a config flag — see below) |
| Fake-mode job success (no second cluster) | Done (`cargo test -p atlas-gateway --test dr`) |
| Live two-site `rbd mirror` peer bootstrap | **Verified** (2026-10-04, Rook `peers.secretNames`) |
| Live enable / replicate / demote / failover through Atlas's API | **Verified** (2026-10-04) |
| Secondary-site registration (`role=secondary`) + `resync` | **Verified** (2026-10-04) |
| Two-way (`rx-tx`) replication, clean failback | **Verified** (2026-10-06) |
| Clean-failback guard on non-forced promote (`GET /dr/mirrors/{id}/status`) | **Verified** (2026-10-06) |
| Journal-mode mirroring (`mode=journal`) | **Verified** (2026-10-06): enable, replay, failover and clean failback with the `peer_replayed` guard |
| Pool-mode mirroring (`PUT /dr/pools/{pool}/mirroring`) | **Verified** (2026-10-06) on a Rook pool-mode pool |
| Multi-image consistency groups (`/volume-groups`) | **Verified locally** (2026-10-06); not replicated cross-site, see below |

`GET /dr/status` and `GET /dr/preflight` both expose `control_plane_ready` (catalog coherent) and
`dataplane_verified`. The latter used to be a hard-coded `false`; it's now `Config::
dr_dataplane_verified` (env `ATLAS_DR_DATAPLANE_VERIFIED`, default `false`) — a per-deployment
toggle an operator sets only after *personally* completing the checklist below against their own
real hardware, never a blanket product claim. Preflight `ready` means you can enqueue a failover
**job**; it is not a claim that Ceph mirroring is live.

### Clean-failback guard

Ceph accepts a non-forced `rbd mirror image promote` whenever the image's newest mirror snapshot is
a demotion. That includes a site that never replayed the peer's later writes, which is how the
2026-10-04 one-way failback silently lost data (below). So in real mode Atlas reads
`rbd mirror image status` before a non-forced promote and refuses it with 409 unless this site's
`rbd-mirror` reports `up+…` and "remote image demoted". That status means the local daemon replayed
the peer's demotion snapshot, so this copy holds everything the peer wrote.

`GET /dr/mirrors/{id}/status` returns the live status plus `promote_ready` and `promote_blocker`.
Poll it after demoting the peer. `?force=1` skips the check, for disasters where the peer is gone;
resync the peer afterwards if it comes back.

### 2026-10-06 two-way drill: verified

This drill used two host-networked Rook clusters, `rook-ceph-dr` on each lab host (Squid 19.2.3,
fsids `e03b3aca…` and `5fbcfa9a…`). Each cluster runs a `CephRBDMirror`, and each host's
firewall opens the Ceph ports to the other host only. One Rook token import
(`peers.secretNames` on the second site) registered an `rx-tx` peer on both sites. A
real-Ceph Atlas gateway ran on each site. Everything below went through the `/dr/*` API, with
`rbd bench` for writes and `rbd export | sha256sum` for checks.

1. **Site A primary.** Peer, `POST /rbd-images`, and snapshot mirroring enabled on 1 GiB `tw-1`. Then
   32 MiB of random 4K writes. Site B went `up+replaying`, and after the next 1-minute snapshot
   both checksums matched. B was registered with `role=secondary`.
2. **Guard, peer still primary.** A non-forced promote on B returned 409 (`up+replaying`, peer not
   demoted).
3. **Planned failover.** After `demote` on A, B reported `promote_ready` within 6 s
   (`up+unknown`, "remote image demoted"), and a non-forced promote succeeded.
4. **Writes on B replicate to A.** This is the step that one-way topology couldn't do. After 16 MiB
   written on B, A replayed it and both checksums matched. A non-forced promote on A while B was
   still primary returned 409.
5. **Clean failback.** After `demote` on B, A was `promote_ready` in 7 s, and a non-forced promote
   succeeded. A held all of B's writes, with no split-brain on either side. Then 8 MiB written on
   A replicated back to B with matching checksums.
6. **The 2026-10-04 failure, reproduced and blocked.** With A's `rbd-mirror` deleted, A was demoted,
   B promoted, 4 MiB written on B, and B demoted. A's status was the stale `down+stopped` "local
   image is primary", and Atlas refused the non-forced promote. Ceph itself would have accepted it
   and lost the 4 MiB.

Finding: a site whose daemon was down for the peer's whole primary period does **not** catch up
after the peer demotes. Both images are then non-primary, both report `up+unknown` "remote image
is not primary", and the guard blocks a non-forced promote on both sites. Atlas can't tell from
status alone which copy is newer. The recovery that worked:

- Force-promote the copy that took the last writes (`?force=1` on B).
- A's daemon replayed B's writes without a resync, and the checksums matched.
- Demote B again, then do a normal clean promote on A (ready in 18 s).

The drill ended with A primary, B `up+replaying`, and identical checksums.

Not covered: RPO under sustained load. Both OSDs are 40 GB loop files on shared HDDs. Journal and
pool modes and consistency groups were run the same day; see the next section.

### Journal mode, pool mode and consistency groups (2026-10-06)

Same two clusters and gateways as the two-way drill.

**Journal mode.** `POST /volumes/{id}/mirror?mode=journal` runs `rbd mirror image enable … journal`,
which also turns on the image's `journaling` feature. Site B went `up+replaying` and showed
`entries_behind_primary` while it caught up. Replay on this lab is slow (0.2–7 entries/s, depending on
host load), so 4K random writes take minutes to drain, while 4 MiB sequential writes drain in about
two minutes per 16 MiB. B's `rbd-mirror` pod was restarted by Rook mid-replay, about 268 entries
behind, and it resumed and caught up. After A demoted and B drained, both checksums matched.

Finding: in journal mode the caught-up copy reports `up+unknown`, "remote image is not primary".
Snapshot mode reports "remote image demoted" at that point. The 2026-10-06 drill above saw the same
"remote image is not primary" in the stale case too, where a copy did *not* replay the peer's
writes. So in journal mode this site's status can't prove it is current. The proof is on the
peer, the site that wrote the journal. `rbd journal status` there lists each peer `rbd-mirror`
client's commit position, and once the peer has replayed everything it equals the local client's
position, including the demotion tag. Atlas exposes that as `live.journal_peers_replayed` in
`GET /dr/mirrors/{id}/status`. A non-forced journal-mode promote needs the local daemon `up`, the
peer non-primary, **and** `peer_replayed=1` (query flag on promote, field on `/dr/failover`). That
flag is the operator confirming they saw `journal_peers_replayed: true` on the peer.

The journal failover and failback drill, through the gateways on both sites:

1. A (demoted) reported `journal_peers_replayed: true`. On B, a promote without the flag returned
   409 with the journal-mode blocker. With `peer_replayed`, the non-forced promote succeeded
   (`force_promoted: false`), and 8 MiB was written on B.
2. B was demoted straight away, but A's `rbd-mirror` was down. Rook was rescheduling it after the
   host's evicted pods filled the pod limit. B's `journal_peers_replayed` stayed `false` for
   10 minutes, because A's client had committed entry 209 of 515. A promote on A with
   `peer_replayed` was still refused (`down+stopped`). This is the stale case that status text
   alone couldn't show.
3. Once A's daemon was back, it replayed the rest, and B flipped to `true` 4 minutes later. A
   non-forced promote on A with `peer_replayed` succeeded. A's checksum equalled B's
   after-write checksum, so none of B's writes were lost.
4. 8 MiB written on A replayed to B (`entries_behind_primary: 0`), and the checksums matched.

**Pool mode.** `PUT /dr/pools/{pool}/mirroring {"mode":"pool"}` runs `rbd mirror pool enable`.
`GET` reads the live mode and peers from `rbd mirror pool info`. It returns site names and
directions only, never the peer's key or `mon_host`. The drill used a separate CephBlockPool
`atlas-dr-pool-mode` with `mirroring.mode: pool` on both sites, peered through one Rook
`peers.secretNames` import (`rx-tx`). In a pool-mode pool, Ceph mirrors every image that has
journaling, so Atlas:

- refuses `mode=snapshot` with 400,
- maps `mode=journal` to "join the pool" (`rbd feature enable … journaling`),
- maps disable to "leave the pool" (`feature disable journaling`), because `rbd mirror image
  disable` is rejected in pool mode.

A 256 MiB image `pm-1` was created through `POST /rbd-images`. A snapshot-mode enable returned 400,
and a journal-mode enable made it `mirroring mode: journal, primary: true`. 16 MiB written on A
replicated to B (`up+replaying`, `entries_behind_primary: 0`), and the SHA-256 checksums matched.

**Consistency groups.** `POST /volume-groups` creates an `rbd group` and adds the member images. All
members must be in one pool, and a volume can be in only one group. `POST
/volume-groups/{id}/snapshots` takes a crash-consistent `rbd group snap create` across every member.
The drill used two 64 MiB images `ga` and `gb`:

1. Wrote 8 MiB to each and took group snapshot `cp1`.
2. Overwrote both with 4 MiB of random 4K writes; both checksums changed.
3. `POST …/snapshots/cp1/rollback` without `confirm: true` returned 400.
4. With `confirm: true`, the rollback job succeeded, and both images were back to their `cp1`
   checksums.

**Groups are local to one cluster.** Cross-site group mirroring (`rbd mirror group enable`) is not
in any released Ceph. Squid 19.2.3 and Tentacle 20.2.4 (`quay.io/ceph/ceph:v20`) both answer
"unknown option 'mirror group enable'". Snapshot-mode `rbd-mirror` mirrors the member images one by
one but does not carry group snapshots: after a group snap on A, B's members had only
`.mirror.non_primary` snapshots and no `cp1`. Mirroring each member still works, but the members'
mirror snapshots are taken independently, so the peer's copies are not mutually consistent at one
point in time. Atlas doesn't claim cross-site group consistency until Ceph ships group mirroring.

### 2026-10-04 live two-site drill: verified

The setup is in [`deploy/rook-ceph-dr-lab/`](../deploy/rook-ceph-dr-lab/README.md):

- **Primary:** a new host-networked Rook cluster `rook-ceph-dr` (Squid 19.2.3, fsid `e03b3aca…`)
  on the second lab host. Its Ceph ports are firewalled to the peer's IP.
- **Secondary:** the first lab host's existing pod-networked cluster (fsid `7785878b…`), running a
  `CephRBDMirror`.
- **Atlas:** a real-Ceph gateway on each site.

Both August blockers are avoided:

- Host networking makes the bootstrap token's `mon_host` the routable `v2:<host-ip>:3300`.
- Rook imports the token itself (`CephBlockPool.spec.mirroring.peers.secretNames`) using admin
  credentials, so there's no `site_name_set` permission error.

What ran, all through the `/dr/*` API unless noted:

1. On the primary, `POST /dr/peers`, `POST /rbd-images` (pool `atlas-dr-mirror-test`), then
   `POST /volumes/{id}/mirror?mode=snapshot`. The job ran a real `rbd mirror image enable`.
2. Wrote 32 MiB of random 4K writes (`rbd bench`). The secondary went `up+replaying`, and once it
   was idle the image's SHA-256 matched the primary's. A second 16 MiB round matched again about
   140 s after the writes stopped (1-minute snapshot schedule plus sync on an HDD-backed OSD),
   recorded with `POST /dr/mirrors/{id}/rpo`.
3. On the secondary, the gateway's discovery had already catalogued the replicated image.
   `POST /volumes/{id}/mirror?role=secondary` registered it. Mirror ids hash `pool/image`, so
   both sites agree on the id.
4. **Planned failover:** `demote` on the primary. The secondary showed "remote image demoted"
   within about 10 s, its preflight went `ready`, and `POST /dr/failover` promoted it without
   force. Both images were identical, and the new primary accepted writes.
5. **Failback in a one-way topology is split-brain.** After `demote` on the new primary, a
   *non-forced* `promote` on the original primary succeeded. The original primary has no
   `rbd-mirror`, so it never received the other side's writes, and its newest snapshot is its own
   "demoted" one, which Ceph accepts. Result: the original primary silently lacked the 4 MiB the
   other site wrote, and the other site reported `up+error split-brain`. Neither Ceph nor Atlas
   can see this from the promoting side. In a one-way setup, treat failback as unclean.
6. **Recovery:** `POST /dr/mirrors/{id}/resync` on the split-brained secondary
   (`rbd mirror image resync`). It went `up+replaying`, then idle, and its checksum matched the
   primary again.

Bugs found and fixed on the way:

- Enabling mirroring as primary on the peer's replicated copy was a successful no-op in Ceph, so
  Atlas catalogued a read-only image as an enabled primary. A real-mode enable now checks
  `rbd info` and returns 409, pointing at `role=secondary`.
- There was no way to record the secondary side, and no `resync`. Both have been added.

Not covered then: two-way replication (it needs both clusters host-networked or otherwise mutually
routable; done on 2026-10-06, above), journal-mode mirroring, and pool-mode mirroring. `rbd mirror image snapshot` (manual)
segfaulted once in the gateway image's Squid client. Atlas doesn't call it; the pool's snapshot
schedule takes the snapshots.

### 2026-08-25 real two-cluster attempt — what was reached, what blocked it

Using the two real Rook Ceph clusters already in this lab (`<ephemeral-ip>`, fsid
`18675a0d-6bd6-455a-aca6-3a045d79a46f`, and `<ephemeral-ip>`, fsid
`e51cf24f-f89f-4061-8635-6e07caa8a3f9` — confirmed distinct), not a synthetic second cluster:

- Created a dedicated `atlas-dr-mirror-test` CephBlockPool (image mirroring mode) on both clusters
  — deliberately isolated from any pool carrying real tenant data.
- Deployed a `CephRBDMirror` daemon (`atlas-dr-mirror`) on both clusters via Rook; both came up
  `Running` (slowly — see below).
- Generated a real `rbd mirror pool peer bootstrap create` token on the primary. Discovered the
  token embeds the mon's address as a Kubernetes **ClusterIP** (`10.43.12.84`), which is not
  routable from the peer cluster — a real architectural gap for any two genuinely separate
  clusters, not specific to this lab. Worked around it by NodePort-exposing `rook-ceph-mon-a`
  (alongside its existing ClusterIP, non-destructively) and rewriting the token's embedded
  `mon_host` to the externally-reachable `host:nodePort` pair.
- Copied the corrected token to the secondary cluster and got as far as `rbd mirror pool peer
  bootstrap import` actually **reaching** the local mon (connection succeeded) before failing:
  `(13) Permission denied` on `site_name_set`. The `rbd-mirror` daemon's own cephx identity
  (`client.rbd-mirror.a`) is deliberately scoped and lacks the mon-config capability needed to set
  a cluster's mirror site name — that operation needs `client.admin`-equivalent privilege, which
  this session stopped short of extracting from the live cluster's Secret rather than pull a
  full cluster-admin credential just to finish a lab drill.
- **Rolled back the NodePort exposure** on the primary's mon (back to ClusterIP-only) since the
  peer relationship was never completed and there was no reason to leave it reachable. Left the
  test pool and mirror daemon deployed on both clusters (harmless, clearly named, isolated from
  real data) as an honest record of how far this got and a head start for whoever finishes it.

**To actually finish this**: run the same `bootstrap import` step with `client.admin` (or a
purpose-built cephx identity granted `mon 'allow *'` scoped just for this), then `rbd mirror pool
enable atlas-dr-mirror-test image` on both sides, create a real image, `rbd mirror image enable
<pool>/<image> snapshot`, and drive the promote/demote cycle through Atlas's `/dr/*` API as
originally planned below. Only then set `ATLAS_DR_DATAPLANE_VERIFIED=true` on the deployment that
actually completed it.

## API

| Method | Path | Notes |
|---|---|---|
| `POST` | `/dr/peers` | Register peer (`secret_ref` = k8s Secret name, never the token) |
| `GET` | `/dr/peers` | List peers |
| `DELETE` | `/dr/peers/{id}` | Remove peer (+ dependent mirrors) |
| `POST` | `/volumes/{id}/mirror?mode=snapshot\|journal&peer=` | Enable (requires a registered peer; 409 on the peer's non-primary copy or a disabled pool; only `journal` in a pool-mode pool) |
| `POST` | `/volumes/{id}/mirror?role=secondary&peer=` | Register this site's non-primary copy (checked with `rbd info`; no CLI write) |
| `DELETE` | `/volumes/{id}/mirror` | Disable |
| `GET` | `/dr/mirrors` · `/dr/status` | Catalog + posture (`verified: false` until live) |
| `GET` | `/dr/preflight` | Checklist before failover |
| `POST` | `/dr/mirrors/{id}/demote` | Primary → secondary |
| `GET` | `/dr/mirrors/{id}/status` | Live `rbd mirror image status` + `promote_ready` / `promote_blocker`; journal mode adds `journal_peers_replayed` |
| `POST` | `/dr/mirrors/{id}/promote?force=0\|1&peer_replayed=0\|1` | Secondary → primary; non-forced needs `promote_ready` in real mode, plus `peer_replayed=1` in journal mode (`force` = split-brain) |
| `POST` | `/dr/mirrors/{id}/resync` | Discard this secondary copy and re-pull from the peer's primary |
| `POST` | `/dr/failover` | `{ mirror_id, confirm: true, force?, peer_replayed? }` runbook |
| `POST` | `/dr/mirrors/{id}/rpo` | `{ rpo_seconds }` observed RPO |
| `GET` · `PUT` | `/dr/pools/{pool}/mirroring` | Pool mirroring mode `image` / `pool` / `disabled` (live `rbd mirror pool info` in real mode) |
| `POST` · `GET` | `/volume-groups` | `{ name, volume_ids }` consistency group (`rbd group`, one pool) |
| `GET` · `DELETE` | `/volume-groups/{id}` | Group detail / remove the group (member images are kept) |
| `POST` · `GET` | `/volume-groups/{id}/snapshots` | `{ name }` crash-consistent group snapshot |
| `DELETE` | `/volume-groups/{id}/snapshots/{snap}` | Remove a group snapshot |
| `POST` | `/volume-groups/{id}/snapshots/{snap}/rollback` | `{ confirm: true }` job; rolls every member back (admin) |

Guards: promote of an already-primary mirror is **409** unless `?force=1`; demote of an already-secondary
is **409**; resync of a primary is **409**; disabled mirrors cannot be promoted/demoted/resynced; in
real mode a non-forced promote is **409** until this site has replayed the peer's demotion (see
"Clean-failback guard").

## Application recovery drills

Mirroring protects volumes; proving an *application* actually recovers is a separate step.
[APPLICATION_RECOVERY.md](APPLICATION_RECOVERY.md) covers the operator-run tool that restores
snapshots into clones in dependency order, verifies the data and reports RPO/RTO.

## Failover drill (fake)

```bash
make run
B=http://127.0.0.1:5110/api/atlas/v1

# Peer + direct-RBD volume (seed via API or SQL in tests)
curl -sS -X POST $B/dr/peers -H 'Content-Type: application/json' \
  -d '{"name":"dc2","cluster_fsid":"fsid-2","secret_ref":"dc2-bootstrap"}'

curl -sS $B/dr/preflight | jq
curl -sS -X POST $B/dr/failover -H 'Content-Type: application/json' \
  -d '{"mirror_id":"<id>","confirm":true}'
```

## Live two-site checklist (when a second cluster exists)

1. Bootstrap RBD mirroring between sites (`rbd mirror pool peer bootstrap` / Rook CephRBDMirror).
   The setup that worked for two-way mirroring (2026-10-06) is two host-networked clusters, an
   `rbd-mirror` on each, and one Rook `peers.secretNames` import; see `deploy/rook-ceph-dr-lab/`.
   **Gotcha (confirmed 2026-08-25):** the bootstrap token embeds the mon's address as whatever
   `mon_host` the local cluster resolves to — inside Kubernetes that's a ClusterIP, not routable
   from a genuinely separate cluster. Either NodePort/LoadBalancer-expose the mon (Rook won't do
   this for you) and rewrite the token's `mon_host` to the external address before importing it
   on the peer, or run the bootstrap from outside Kubernetes against a routable mon endpoint.
   **Gotcha:** `rbd mirror pool peer bootstrap import` needs `client.admin`-equivalent mon
   capability (it sets the cluster's mirror site name) — the `rbd-mirror` daemon's own scoped
   cephx identity (`client.rbd-mirror.<id>`) does not have this and will fail with `(13)
   Permission denied` on `site_name_set`. Use `client.admin` (from the `rook-ceph-mon` Secret) or
   a purpose-built identity with `mon 'allow *'` for this one step only.
2. Store the peer bootstrap token in a k8s Secret; register the peer with `secret_ref`.
3. Enable mirroring on critical volumes (`mode=snapshot` or `journal`).
4. Confirm `GET /dr/preflight` is ready; run a scheduled demote/promote drill.
5. Measure RPO and `POST /dr/mirrors/{id}/rpo`.
6. Document site roles and force-promote policy for split-brain.

Steps 1–6 were run on the 2026-10-04 lab (above). That verifies the code paths, not your
deployment: set `ATLAS_DR_DATAPLANE_VERIFIED` only on a deployment whose own operator has drilled
it (see `Config::dr_dataplane_verified` above).
