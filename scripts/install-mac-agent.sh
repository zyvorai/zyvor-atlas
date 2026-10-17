#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Install atlas-mac-agent on this Mac as a launchd daemon (read-only disk inventory for an Atlas
# gateway). Run as root:  sudo scripts/install-mac-agent.sh <path-to-atlas-mac-agent> <bind> <cert.pem> <key.pem>
#   <bind>   host:port the gateway will reach, e.g. 0.0.0.0:5112 (non-loopback needs the token and TLS)
# Prints the generated token once; put it in the gateway's ATLAS_MACOS_AGENT_TOKEN(_FILE).
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
BIN="${1:?path to the atlas-mac-agent binary}"
BIND="${2:?bind address, e.g. 0.0.0.0:5112}"
CERT="${3:?TLS certificate (PEM)}"
KEY="${4:?TLS key (PEM)}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
LABEL=ai.zyvor.atlas-mac-agent
PLIST=/Library/LaunchDaemons/$LABEL.plist

install -m 0755 "$BIN" /usr/local/bin/atlas-mac-agent
install -d -m 0700 /usr/local/etc/atlas-mac-agent
if [ ! -s /usr/local/etc/atlas-mac-agent/token ]; then
  (umask 077; openssl rand -hex 32 > /usr/local/etc/atlas-mac-agent/token)
  echo "token: $(cat /usr/local/etc/atlas-mac-agent/token)"
fi
sed -e "s|__BIND__|$BIND|" -e "s|__CERT__|$CERT|" -e "s|__KEY__|$KEY|" \
  "$HERE/deploy/macos/$LABEL.plist" > "$PLIST"
chown root:wheel "$PLIST"; chmod 0644 "$PLIST"
launchctl bootout system/$LABEL 2>/dev/null || true
launchctl bootstrap system "$PLIST"
echo "installed $LABEL (log: /var/log/atlas-mac-agent.log)"
