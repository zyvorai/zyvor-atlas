# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# atlas-io-agent: the observe-first storage I/O sensor (crates/atlas-io, docs/IO_EBPF.md). No UI,
# no optional DataBridge connectors, no Ceph/ZFS CLIs — this binary only ever talks HTTP on
# :5111 and, in `live` mode, attaches its block-layer BPF programs, so the image stays minimal on
# purpose. Built with the `bpf` feature: clang + libbpf headers compile the CO-RE object, which is
# embedded in the binary (the runtime image needs no BPF tooling).
# ---- builder ----
FROM docker.io/library/rust:1.99-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS builder
RUN apt-get update && apt-get install -y --no-install-recommends clang libbpf-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release -p atlas-io --features bpf,mcp --bin atlas-io-agent

# ---- runtime ----
FROM docker.io/library/debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/atlas-io atlas-io \
    && mkdir -p /var/lib/atlas-io && chown atlas-io:atlas-io /var/lib/atlas-io
COPY --from=builder /build/target/release/atlas-io-agent /usr/local/bin/atlas-io-agent
USER atlas-io
WORKDIR /var/lib/atlas-io
ENV ATLAS_IO_BIND=0.0.0.0:5111 \
    ATLAS_IO_MODE=fake
EXPOSE 5111
ENTRYPOINT ["/usr/local/bin/atlas-io-agent"]
