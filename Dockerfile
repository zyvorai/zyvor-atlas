# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# ---- UI builder (React Storage Center → dist) ----
FROM docker.io/library/node:26-alpine@sha256:ef24c5053d50fdc3e4e56eb4e7ddb7861874ab0fdc797046ba897581deb8e868 AS ui
WORKDIR /ui
# node:22-alpine ships npm 10.9.8, which mis-resolves this lockfile's optional/platform deps
# (npm ci "succeeds" but leaves node_modules incomplete — vite/rollup then fail to resolve
# packages like recharts that are present on disk with a valid package.json). npm >=11 installs
# cleanly against the same lockfile; pin newer npm before `ci` rather than downgrading the base image.
RUN npm install -g npm@12.0.2
COPY crates/atlas-gateway/ui/package.json crates/atlas-gateway/ui/package-lock.json ./
RUN npm ci
COPY crates/atlas-gateway/ui/ ./
RUN npm run build

# ---- builder ----
FROM docker.io/library/rust:1.99-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS builder
# protoc for the gRPC crates; cmake for the vendored librdkafka (kafka-lag feature).
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
COPY --from=ui /ui/dist crates/atlas-gateway/ui/dist
# Build the real MongoDB (pure Rust) + SQL Server (tiberius) + Oracle connectors and precise CDC lag in.
# The `oracle` crate vendors ODPI-C (compiles with the toolchain here) and dlopens the Oracle Instant
# Client at *runtime* — so no OCI libs are needed at build time, only in the runtime stage below.
RUN cargo build --release -p atlas-gateway -p atlasctl \
    --features atlas-gateway/mcp,atlas-databridge/mongodb,atlas-databridge/sqlserver,atlas-databridge/oracle,atlas-databridge/kafka-lag

# ---- runtime ----
FROM docker.io/library/debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime
# ceph/rbd CLIs are only needed when ATLAS_CEPH_DRIVER_MODE=real against a real cluster.
# zfsutils-linux (zpool/zfs) + util-linux (lsblk/wipefs/findmnt) are only exercised when
# ATLAS_ZFS_DRIVER_MODE=real against a raw host disk (see docs/DISKS.md) — the container still
# needs the host's /dev and its `zfs` kernel module (privileged + hostPath /dev in the k8s
# manifest); these packages only provide the userspace CLI the driver shells out to.
# zfsutils-linux ships in Debian's `contrib` component (CDDL, not in `main`) — enable it before
# installing; nothing else in this image needs it.
# File capabilities (setcap), not `privileged`/`runAsUser: 0`: the gateway runs as a non-root uid
# (10001, see below) and — confirmed live against the deploy target — Kubernetes does NOT populate
# a non-root container's effective/ambient capability set from `privileged: true` or from
# `securityContext.capabilities.add` (only the *bounding* set changes; CapEff/CapPrm/CapAmb stayed
# all-zero either way, so `wipefs`/`zpool` still got `Permission denied` opening a root:disk 0660
# device). File capabilities on the binaries themselves ARE honored regardless of the calling
# process's uid, independent of that ambient-capability gap — the standard mechanism for "grant
# capabilities to one binary, not root". Requires `allowPrivilegeEscalation: true` in the manifest
# (the default no-new-privs behavior otherwise suppresses file capabilities on exec) and the target
# capabilities still present in the container's bounding set (`capabilities.add` in the manifest).
# Oracle Instant Client (Basic Lite) + libaio provide libclntsh.so, which ODPI-C dlopens at runtime
# for the DataBridge `oracle` connector; freely redistributable. The URL is a build ARG so air-gapped
# builds can point at an internal mirror, e.g. --build-arg ORACLE_IC_URL=https://mirror.corp/…zip
ARG ORACLE_IC_URL=https://download.oracle.com/otn_software/linux/instantclient/2113000/instantclient-basiclite-linux.x64-21.13.0.0.0dbru.zip
RUN echo "deb http://deb.debian.org/debian bookworm contrib" >> /etc/apt/sources.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip libaio1 zfsutils-linux util-linux xfsprogs libcap2-bin \
    && for bin in /usr/sbin/wipefs /usr/sbin/zpool /usr/sbin/zfs /usr/bin/lsblk /usr/bin/findmnt /usr/sbin/blockdev; do \
         setcap cap_dac_override,cap_sys_admin=ep "$bin"; \
       done \
    && apt-get purge -y --auto-remove libcap2-bin \
    && curl -fsSL -o /tmp/ic.zip "$ORACLE_IC_URL" \
    && mkdir -p /opt/oracle && unzip -q /tmp/ic.zip -d /opt/oracle && rm /tmp/ic.zip \
    && apt-get purge -y --auto-remove curl unzip \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/atlas atlas \
    && mkdir -p /var/lib/atlas && chown atlas:atlas /var/lib/atlas
ENV LD_LIBRARY_PATH=/opt/oracle/instantclient_21_13
COPY --from=builder /build/target/release/atlas-gateway /usr/local/bin/atlas-gateway
COPY --from=builder /build/target/release/atlasctl /usr/local/bin/atlasctl
COPY --from=builder /build/migrations /usr/local/share/atlas/migrations
USER atlas
WORKDIR /var/lib/atlas
ENV ATLAS_BIND_ADDR=0.0.0.0:5110 \
    ATLAS_DATABASE_URL=sqlite:///var/lib/atlas/atlas.db?mode=rwc
EXPOSE 5110
ENTRYPOINT ["/usr/local/bin/atlas-gateway"]
