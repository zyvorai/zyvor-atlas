<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Deploy the Atlas data companion

Use Python 3.11+ and the repository's `tools` directory; core execution has no
third-party dependencies. Install the optional Iceberg requirements for tables.

The service is a separate authenticated endpoint on 5120; it is not mounted under
the Rust gateway and does not accept Atlas JWTs. Credentials are explicitly scoped
in its private config. Do not forward arbitrary actor/tenant headers.

Example systemd unit for an installation at `/opt/atlas`:

```ini
[Unit]
Description=Atlas AI data services
After=network-online.target

[Service]
User=atlas-data
Group=atlas-data
WorkingDirectory=/opt/atlas
Environment=PYTHONPATH=/opt/atlas/tools
ExecStart=/opt/atlas/venv/bin/python -m atlas_data serve --root /var/lib/atlas-data --config /etc/atlas-data/config.json
Restart=on-failure
UMask=0077
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/atlas-data
MemoryMax=2G
TasksMax=128
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

Create the service account and private directories before starting. Mount the data
root on persistent Atlas-backed storage, with an actual filesystem quota. The CAS
capacity argument does not cap the Iceberg warehouse or restored drill workspace.
For large recovery drills, put temporary storage on a suitably sized isolated mount.

Expose remotely only with TLS, using the flags in [the service guide](../../docs/AI_DATA_SERVICES.md).
Keep one instance per data root. A shared volume does not make SQLite a distributed
control plane. Give node workers separate trusted credentials, private per-tenant
cache directories, and restrict credential/config filesystem access.

When containerizing, run as a non-root UID, use a read-only image, mount config and
TLS certificates read-only, and mount only the required data/temp volumes writable.
The image is `Dockerfile.data`; build and smoke-test it on the lab with `scripts/atlas-data-image-remote.sh <host> <user>`. It is not published to a registry yet. Manifests: `deploy/k8s/atlas-data.yaml`, `deploy/k8s/atlas-data-gate-example.yaml`.
