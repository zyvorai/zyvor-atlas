<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# DataBridge edge operators

Installs the operators the **real** (non-fake) DataBridge pipeline needs on an edge cluster. The
control plane (`atlas-gateway`) and its **fake pipeline run with none of these** — use this only to
migrate against real cloud databases.

| Operator | Purpose | Namespace |
|---|---|---|
| CloudNativePG | Postgres edge targets (data + WAL on Ceph RBD) | `cnpg-system` |
| Percona XtraDB Cluster | MySQL edge targets (data on Ceph RBD) | `zyvor-databridge` |
| Strimzi (Kafka) | runs Debezium (KafkaConnect) for CDC | `zyvor-databridge` |

## Prerequisites
- `kubectl` pointing at the edge cluster.
- A Ceph RBD StorageClass named **`zyvor-rbd-prod`** (from [`deploy/rook-ceph-lab`](../rook-ceph-lab));
  edge DB data + WAL land there.

## Install
```bash
./up.sh                 # CloudNativePG + Percona + Strimzi + lab Kafka (zyvor-kafka)
./up.sh --pg-only       # just CloudNativePG (Postgres only)
./up.sh --no-streaming  # skip Kafka (full-load only, no CDC yet)
SKIP_KAFKA_CR=1 ./up.sh # operators only (reuse an existing Kafka)
CNPG_VERSION=1.24.1 PXC_VERSION=1.14.0 ./up.sh
```

`10-kafka.yaml` installs a single-broker KRaft Kafka named **`zyvor-kafka`** (bootstrap
`zyvor-kafka-kafka-bootstrap:9092`) — what `start_cdc` hard-codes. Lab RF=1 only.

## Connect image (required for real CDC)
```bash
podman build -t databridge-connect:dev -f connect/Dockerfile connect
# import into the cluster, then set ATLAS_DATABRIDGE_CONNECT_IMAGE on the gateway
```
The image bundles Debezium (Postgres / MySQL / MariaDB / MongoDB / Oracle / SQL Server) + JDBC sink
drivers + Mongo Kafka sink.

## Verify
```bash
kubectl -n cnpg-system get deploy cnpg-controller-manager
kubectl -n zyvor-databridge get deploy | grep -E 'percona|strimzi'
kubectl -n zyvor-databridge get kafka zyvor-kafka
kubectl get storageclass | grep zyvor-rbd-prod
```

## Then
Register a **real** source (`driver_mode: real`, credentials in a k8s Secret) from the **Cloud
Databases** page (or `POST /api/atlas/v1/databridge/sources`), create a plan, and run the pipeline:
discover → assess → provision → full-load → CDC → validate → cutover. The edge CloudNativePG /
Percona `Cluster` binds its PVCs on `zyvor-rbd-prod`; Atlas's reconciler advances the cluster to
`ready` and (later) tracks CDC lag.

> The fake pipeline (`make run-databridge`) exercises the same control-plane flow end-to-end with no
> operators installed — start there.

## Engine verification (live)
| Engine | Discover | Full-load | Validate | CDC | Cutover |
|---|---|---|---|---|---|
| Postgres | live | live | live | live | live |
| MySQL | live | live | live | live | live |
| MariaDB | live | live | live | live | live |
| MongoDB | live | live | live | live | live |
| Oracle / SQL Server | live | via Debezium `initial` | advisory | live | live |

## Lab CDC stack (`deploy/databridge/up.sh`)

On the **edge** cluster (where CNPG/Percona already run):

```bash
cd deploy/databridge
./up.sh                         # CNPG + PXC + PSMDB + Strimzi + zyvor-kafka
podman build -t databridge-connect:dev -f connect/Dockerfile connect
# import image into k3s/containerd, then on atlas-gateway-ceph:
kubectl -n rook-ceph set env deploy/atlas-gateway-ceph \
  ATLAS_DATABRIDGE_CONNECT_IMAGE=localhost/databridge-connect:dev
```

**Lab `212.8.248.187` (2026-09-01):** Strimzi + CNPG + PXC + PSMDB + `zyvor-kafka` Ready; Connect
image includes MariaDB + Mongo plugins; gateway Ceph image built with `mongodb`/`kafka-lag` features.
**MariaDB and MongoDB** verified live through CDC + cutover (alongside Postgres); **MySQL** followed
on 2026-10-04 (see `docs/DATABRIDGE.md`). Atlas **refuses** real `cdc/start` without
`ATLAS_DATABRIDGE_CONNECT_IMAGE`. Fake mode still runs discover→cutover for all six engines in CI.
