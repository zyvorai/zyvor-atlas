// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
/** In-console API surface map — curated from docs/API.md (not a full OpenAPI host). */
import { Link } from "react-router-dom";
import { navCrumbs } from "../nav/routes";
import { TerminalPane, colorizeJson } from "../ui/kit";
import { DocsPage, type DocsRow, type DocsSection } from "../ui/templates/DocsPage";

const SECTIONS: { title: string; items: DocsRow[] }[] = [
  {
    title: "Service",
    items: [
      { method: "GET", path: "/health", note: "Basic health check" },
      { method: "GET", path: "/readyz", note: "Deep readiness: database and driver" },
      { method: "GET", path: "/version", note: "Gateway build identity" },
      { method: "GET", path: "/metrics", note: "Prometheus text, same auth as the API" },
    ],
  },
  {
    title: "Auth and governance",
    items: [
      { method: "POST", path: "/api/atlas/v1/auth/login", note: "Console username/password → JWT" },
      { method: "POST", path: "/api/atlas/v1/auth/tokens", note: "Mint scoped service-account JWT (admin)" },
      { method: "GET", path: "/api/atlas/v1/auth/users", note: "List console users (admin)" },
      { method: "GET", path: "/api/atlas/v1/tenants", note: "Tenants, quotas, policies" },
    ],
  },
  {
    title: "Inventory",
    items: [
      { method: "GET", path: "/api/atlas/v1/clusters", note: "Storage clusters" },
      { method: "GET", path: "/api/atlas/v1/pools", note: "Pools (?kind=)" },
      { method: "GET", path: "/api/atlas/v1/volumes", note: "Volumes (?state=&kind=)" },
      { method: "GET", path: "/api/atlas/v1/snapshots", note: "Snapshots" },
      { method: "GET", path: "/api/atlas/v1/buckets", note: "Object buckets (Ceph RGW default)" },
      { method: "GET", path: "/api/atlas/v1/osds", note: "OSD inventory" },
    ],
  },
  {
    title: "Write path (async jobs)",
    items: [
      { method: "POST", path: "/api/atlas/v1/volumes", note: "Create volume → job" },
      { method: "DELETE", path: "/api/atlas/v1/volumes/{id}", note: "Delete volume → job" },
      { method: "POST", path: "/api/atlas/v1/volumes/{id}/expand", note: "Expand → job" },
      { method: "POST", path: "/api/atlas/v1/volumes/{id}/snapshots", note: "Create snapshot → job" },
      { method: "GET", path: "/api/atlas/v1/jobs/{id}", note: "Job status" },
    ],
  },
  {
    title: "Object storage (Ceph RGW)",
    items: [
      { method: "POST", path: "/api/atlas/v1/buckets", note: "Create bucket (backend_id; Ceph RGW default) → job" },
      { method: "DELETE", path: "/api/atlas/v1/buckets/{id}", note: "Delete bucket (must be empty) → job" },
      { method: "POST", path: "/api/atlas/v1/buckets/{id}/objects/upload-url", note: "Presigned PUT (browser → RGW)" },
      { method: "POST", path: "/api/atlas/v1/databridge/object", note: "Bucket-to-bucket migration (explicit source/dest endpoint + secret_ref)" },
    ],
  },
  {
    title: "Disks (raw device provisioning)",
    items: [
      { method: "GET", path: "/api/atlas/v1/zfs/devices", note: "Whole disks on this host + status" },
      { method: "POST", path: "/api/atlas/v1/zfs/pools/from-device", note: "Wipe + zpool create (confirm, wipe_existing) → job" },
      { method: "POST", path: "/api/atlas/v1/zfs/pools/{name}/destroy", note: "zpool destroy (confirm_pool_name) → job" },
      { method: "POST", path: "/api/atlas/v1/ceph/devices", note: "Claim a disk as a Ceph OSD via Rook → job" },
    ],
  },
  {
    title: "Observability",
    items: [
      { method: "GET", path: "/api/atlas/v1/metrics/summary", note: "Capacity + I/O + recovery rollup" },
      { method: "GET", path: "/api/atlas/v1/metrics/history", note: "Time series (?minutes=)" },
      { method: "GET", path: "/api/atlas/v1/events", note: "Unified activity feed" },
      { method: "GET", path: "/api/atlas/v1/alerts", note: "Alerts (?state=)" },
      { method: "GET", path: "/api/atlas/v1/ceph/status", note: "Native Ceph status JSON" },
    ],
  },
  {
    title: "DR (RBD mirroring)",
    items: [
      { method: "GET", path: "/api/atlas/v1/dr/status", note: "Peers / mirrors / RPO posture" },
      { method: "POST", path: "/api/atlas/v1/dr/peers", note: "Register peer cluster" },
      { method: "POST", path: "/api/atlas/v1/volumes/{id}/mirror", note: "Enable mirror (?mode=&peer=)" },
      { method: "POST", path: "/api/atlas/v1/dr/mirrors/{id}/promote", note: "Promote / failover" },
      { method: "POST", path: "/api/atlas/v1/dr/failover", note: "Confirm-gated failover runbook" },
    ],
  },
  {
    title: "DataBridge",
    items: [
      { method: "GET", path: "/api/atlas/v1/databridge/sources", note: "Cloud DB sources" },
      { method: "POST", path: "/api/atlas/v1/databridge/sources/{id}/discover", note: "Discover → job" },
      { method: "GET", path: "/api/atlas/v1/databridge/plans", note: "Migration plans" },
      { method: "POST", path: "/api/atlas/v1/databridge/plans/{id}/cdc/start", note: "Start Debezium CDC" },
      { method: "POST", path: "/api/atlas/v1/databridge/plans/{id}/cutover", note: "Cutover → job" },
    ],
  },
];

const GRPC: DocsRow[] = [
  { method: "RPC", path: "Health", note: "Liveness for the gRPC service" },
  { method: "RPC", path: "ListClusters", note: "Storage clusters" },
  { method: "RPC", path: "ListPools", note: "Pools" },
  { method: "RPC", path: "ListVolumes", note: "Volumes" },
  { method: "RPC", path: "GetVolume", note: "Volume detail" },
  { method: "RPC", path: "CreateVolume", note: "Create → job; Owner sets product_bindings" },
  { method: "RPC", path: "DeleteVolume", note: "Delete → job" },
  { method: "RPC", path: "ExpandVolume", note: "Expand → job" },
  { method: "RPC", path: "CreateSnapshot", note: "Create snapshot → job" },
  { method: "RPC", path: "ListSnapshots", note: "Snapshots" },
  { method: "RPC", path: "ListVolumesByOwner", note: "Volumes scoped to an owner" },
  { method: "RPC", path: "GetJob", note: "Job status" },
  { method: "RPC", path: "WatchJob", note: "Job status (stream)" },
  { method: "RPC", path: "ListAlerts", note: "Alerts" },
  { method: "RPC", path: "GetMetricsSummary", note: "Capacity + I/O + recovery rollup" },
  { method: "RPC", path: "ListBuckets", note: "RGW / object buckets" },
];

export default function ApiDocs() {
  const sections: DocsSection[] = [
    {
      title: "Conventions",
      children: (
        <>
          <p>
            Base path <code className="at-terminal-inline">/api/atlas/v1</code>. Auth:{" "}
            <code className="at-terminal-inline">Authorization: Bearer &lt;JWT&gt;</code> when{" "}
            <code className="at-terminal-inline">ATLAS_AUTH_REQUIRED=1</code>.
          </p>
          <TerminalPane title="error.json" chrome>
            {colorizeJson(`{\n  "error": {\n    "code": "not_found",\n    "message": "volume missing"\n  }\n}`)}
          </TerminalPane>
          <p className="mt-3.5">
            gRPC on <code className="at-terminal-inline">ATLAS_GRPC_ADDR</code> (default{" "}
            <code className="at-terminal-inline">:5111</code>, lab NodePort <strong>30512</strong>), service{" "}
            <code className="at-terminal-inline">atlas.v1.AtlasStorage</code>. For product ownership, see{" "}
            <Link to="/access" className="a-link">Access</Link> tokens and{" "}
            <code className="at-terminal-inline">docs/PRODUCTS.md</code>.
          </p>
        </>
      ),
    },
    { title: "gRPC service AtlasStorage", rows: GRPC },
    ...SECTIONS.map((sec) => ({ title: sec.title, rows: sec.items })),
  ];

  return (
    <DocsPage
      crumbs={navCrumbs("api-docs")}
      eyebrow="Reference"
      title="API docs"
      state="The main REST and gRPC endpoints. Full examples are in docs/API.md."
      sections={sections}
    />
  );
}
