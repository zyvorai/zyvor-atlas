#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
#
# Host-networked Ceph listens on the public interface; accept its ports only from the mirror peer
# and from this host (including its pods), drop everyone else. Idempotent.
set -euo pipefail
PEER="${PEER:?PEER=<peer cluster public IP>}"
LOCAL_NETS="${LOCAL_NETS:-127.0.0.0/8 10.0.0.0/8}"
SELF="$(ip -4 route get 1.1.1.1 | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}')"

iptables -N ATLAS-DR 2>/dev/null || iptables -F ATLAS-DR
for src in "$PEER" "$SELF" $LOCAL_NETS; do
  iptables -A ATLAS-DR -s "$src" -j ACCEPT
done
iptables -A ATLAS-DR -j DROP
for ports in 3300 6800:7300; do
  iptables -C INPUT -p tcp --dport "$ports" -j ATLAS-DR 2>/dev/null ||
    iptables -I INPUT 1 -p tcp --dport "$ports" -j ATLAS-DR
done
