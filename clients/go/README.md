<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Atlas Go client

`github.com/zyvorai/atlas/clients/go` is a Go client for the Atlas REST API
(`/api/atlas/v1`, see [docs/API.md](../../docs/API.md)). It depends only on the
Go standard library, so products with strict dependency rules (Kairon's
controller and node) can use it.

```bash
go get github.com/zyvorai/atlas/clients/go@latest
```

```go
c, err := atlas.New("http://atlas-gateway:5110", atlas.WithToken(os.Getenv("ATLAS_TOKEN")))

// PVC-backed volume on any backend (Ceph RBD, CephFS, NFS, ZFS), owned by a product resource.
a, err := c.CreateVolume(ctx, atlas.CreateVolumeRequest{
    TenantID: "acme", Name: "web-root", SizeBytes: 20 << 30, Policy: "database",
    Owner:      &atlas.Owner{Product: "kairon", ResourceType: "machine", ResourceID: uid, Role: "root_disk"},
    Kubernetes: &atlas.KubernetesOpts{Namespace: "default", CreatePVC: true},
})
if _, err := c.Wait(ctx, a, time.Second); err != nil { ... } // a 202 only means "queued"
vol, err := c.GetVolume(ctx, a.Resource.VolumeID)

// Raw RBD image for consumers that open RBD directly (QEMU), no PVC.
r, err := c.CreateRBDImage(ctx, atlas.CreateRBDImageRequest{Name: "web-root", SizeBytes: 20 << 30})
pool, image, ok := atlas.ParseRBD("rbd:" + r.Resource.RBD)
```

Coverage: health/version, volumes (list/get/create/delete/expand), direct RBD
images, snapshots (create/list/delete/clone/restore), backup and restore jobs,
and jobs (`GetJob`, `Wait`, `WaitJob`). Writes return `Accepted`; most are
async (202 + job) and some complete synchronously (200), which `Wait` handles.
Errors are `*APIError` with the upstream HTTP status (`IsNotFound`,
`IsConflict`, `IsUnavailable`); a failed job is `*JobFailedError`
(`IsJobFailed`).

## Tests

```bash
go test -race ./...                                     # unit tests (fake gateway)
cargo run -p atlas-gateway &                             # real gateway, fake drivers
ATLAS_CONTRACT_URL=http://127.0.0.1:5110 go test -run Contract ./...
```

CI (`.github/workflows/go-client.yml`) runs both, so the Go types can't drift
from the Rust routes without failing a build.

Versions are tagged `clients/go/vX.Y.Z`.
