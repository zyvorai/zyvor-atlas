# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
.PHONY: dev build release test lint fmt fmt-check run run-databridge run-io cli clean ui ui-dev \
	features docker-smoke ci audit headers help status deploy-remote deploy-ceph

dev: lint test ## Clippy, then tests

build: ## Debug build of the workspace
	cargo build --workspace

release: ## Release build of the workspace
	cargo build --workspace --release

test: ## Workspace tests
	cargo test --workspace

lint: ## Clippy, warnings denied
	cargo clippy --workspace --all-targets -- -D warnings

# SPDX + copyright header gate (see scripts/check-license-headers.sh, CLA.md, DCO.md, NOTICE).
headers: ## SPDX and copyright header check
	./scripts/check-license-headers.sh

# Supply-chain gate: known-vulnerable/yanked advisories, disallowed licenses, unknown sources
# (deny.toml). Installs cargo-deny on first run if missing.
audit: ## cargo-deny: advisories, bans, licenses, sources
	@command -v cargo-deny >/dev/null || cargo install cargo-deny --locked
	cargo deny check advisories bans licenses sources

fmt: ## Format Rust sources
	cargo fmt --all

fmt-check: ## Fail if rustfmt would change sources (informational in CI)
	cargo fmt --all --check

# Compile-check optional DataBridge connectors (mirrors the CI feature gate).
features: ## Compile-check optional DataBridge connectors
	cargo check -p atlas-databridge --features sqlserver
	cargo check -p atlas-databridge --features mongodb
	cargo check -p atlas-databridge --features azure-blob
	cargo check -p atlas-databridge --features oracle
	@command -v cmake >/dev/null && cargo check -p atlas-databridge --features kafka-lag \
		|| echo "skip kafka-lag (cmake not installed)"

# Fast Docker smoke: UI stages of both images (needs podman or docker).
docker-smoke: ## Build the UI stage of both gateway images
	@RT=$$(command -v podman >/dev/null && echo podman || echo docker); \
	  $$RT build --target ui -t atlas-gateway:ui -f Dockerfile .; \
	  $$RT build --target ui -t atlas-gateway-ceph:ui -f Dockerfile.ceph .

# Local equivalent of the CI static gate (no Docker, no containers).
ci: headers lint test audit features ui ## Local static gate (fmt is informational in CI)

run: ui ## Gateway with the fake Ceph driver
	ATLAS_CEPH_DRIVER_MODE=fake cargo run -p atlas-gateway

# Run the gateway for a DataBridge demo: fake Ceph + the reconciler ticking every 5s so fake CDC
# lag drains and the migration pipeline runs end-to-end with no cloud/k8s. See docs/DATABRIDGE.md.
run-databridge: ui ## DataBridge demo: fake Ceph, reconciler every 5s
	ATLAS_CEPH_DRIVER_MODE=fake ATLAS_DATABRIDGE_RECONCILE_SECS=5 cargo run -p atlas-gateway

run-io: ## Observe-first I/O agent (fake bio source, :5111)
	ATLAS_IO_MODE=fake cargo run -p atlas-io --bin atlas-io-agent

# Build the React Storage Center UI into crates/atlas-gateway/ui/dist (embedded by the gateway).
# Matches h2kvm- `web/Makefile` frontend target: npm ci + vite build.
ui: ## Production build of the Storage Center UI
	cd crates/atlas-gateway/ui && npm ci --silent && npm run build

# Vite dev server (hot reload) proxying /api to a running gateway (make run in another shell).
ui-dev: ## Vite dev server, proxying /api to a running gateway
	cd crates/atlas-gateway/ui && npm install && npm run dev

cli: ## Run atlasctl (pass args with ARGS='health')
	cargo run -p atlasctl -- $(ARGS)

clean: ## Remove build artifacts and the local SQLite files
	cargo clean
	rm -f atlas.db atlas.db-wal atlas.db-shm

status: ## atlasctl health (ATLAS_BASE_URL, default http://127.0.0.1:5110)
	cargo run -q -p atlasctl -- health

deploy-remote: ## Deploy the gateway: make deploy-remote H=<host> [U=sus] [ARGS=--with-ceph]
	@test -n "$(H)" || (echo "Usage: make deploy-remote H=<host> [U=user] [ARGS=--with-ceph]"; exit 1)
	./scripts/deploy-remote.sh $(H) $(or $(U),sus) $(ARGS)

deploy-ceph: ## Deploy the real-Ceph gateway: make deploy-ceph H=<host> [U=sus]
	@test -n "$(H)" || (echo "Usage: make deploy-ceph H=<host> [U=user]"; exit 1)
	./scripts/deploy-ceph-gateway-remote.sh $(H) $(or $(U),sus) $(ARGS)

help: ## Show targets
	@grep -E '^[a-zA-Z0-9_-]+:.*## ' $(MAKEFILE_LIST) | sort | awk -F':.*## ' '{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

.PHONY: data-test data-demo
data-test: ## Test the experimental AI data companion (optional Iceberg test skips without dependency)
	PYTHONPATH=tools python3 -m unittest discover -s tools/atlas_data/tests -v

data-demo: ## Run all nine AI data workflows against a real local HTTP server
	PYTHONPATH=tools python3 examples/data/end_to_end.py
