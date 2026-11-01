<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Releasing

A release is a `v*` tag on `main`. Two workflows fire on the tag push:

- **Publish container images** (`docker-publish.yml`): builds, signs (keyless), generates an SBOM and Trivy-scans
  `ghcr.io/zyvorai/atlas` (`Dockerfile`), `atlas-ceph` (`Dockerfile.ceph`), `atlas-io-agent` (`Dockerfile.io`) and `atlas-data` (`Dockerfile.data`).
- **Release** (`release.yml`): GitHub Release with the matching `CHANGELOG.md` section, `atlasctl` and
  `atlas-mac-agent` archives for common platforms (macOS included), the UI bundle, `SHA256SUMS.txt` and its keyless
  cosign signature `SHA256SUMS.txt.sigstore.json`.

## Steps

1. Branch `release/X.Y.Z` from `main`. Move `## [Unreleased]` entries under `## [X.Y.Z] — date` in `CHANGELOG.md`.
2. Bump the version in: `Cargo.toml` (workspace) and the workspace members in `Cargo.lock`, `README.md` badge,
   `docs/DEPLOYMENT.md`, `docs/STATUS.md`, both Helm `Chart.yaml` (`version`/`appVersion`),
   `crates/atlas-gateway/ui/package*.json`, `website/package*.json`. Check with `cargo metadata --locked`.
3. Open the PR, merge it.
4. Tag the merge commit (`git tag -a vX.Y.Z -m vX.Y.Z`) and push the tag.
5. Wait for both workflows. Then bump the `atlas-io-agent` image in `deploy/k8s/atlas-io-agent*.yaml` in a follow-up PR; never deploy the floating `:latest`.

## Verifying a release

Download the archives you need together with `SHA256SUMS.txt` and `SHA256SUMS.txt.sigstore.json` from the release
page, then check the signature and the hashes:

```bash
cosign verify-blob SHA256SUMS.txt \
  --bundle SHA256SUMS.txt.sigstore.json \
  --certificate-identity-regexp 'https://github.com/zyvorai/zyvor-atlas/\.github/workflows/release\.yml@refs/tags/v.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum --ignore-missing -c SHA256SUMS.txt
```

Container images are verified with `cosign verify` against `docker-publish.yml`, and their CycloneDX SBOMs are
attached to the release. The checksum signature starts with the first tag cut after this was added; earlier
releases carry checksums only.

## ghcr visibility

Visibility is per package, not per tag. `atlas`, `atlas-ceph` and `atlas-io-agent` are public, so new tags are
public as soon as they are pushed and nodes pull anonymously. A package that has never been published starts
private: set it to public once under the organisation's Packages settings.

## Notes

- Runners can queue for a long time; the tag is not "released" until both workflows are green.
- Pre-pull a new image on a node before rolling a DaemonSet to it (`k3s ctr -n k8s.io images pull`), because a
  failed pull kills the old pod first.
