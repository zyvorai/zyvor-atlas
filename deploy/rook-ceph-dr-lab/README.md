<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Two-site RBD mirroring lab

What the two-way drill in [`docs/DR.md`](../../docs/DR.md) (2026-10-06) ran on. Each of two hosts
runs a host-networked Rook cluster `rook-ceph-dr` beside whatever Rook cluster the host already
runs, plus an `rbd-mirror` daemon and an Atlas real-Ceph gateway.

Each site's `rbd-mirror` dials the other site's mons and OSDs. Pod IPs aren't routable between
hosts, so both clusters use `network.provider: host`. Their Ceph ports (3300 and 6800–7300) are
firewalled to the peer's IP.

## Each site

```bash
sudo truncate -s 40G /var/lib/ceph-dr-osd.img
sudo install -m0644 ceph-dr-osd-loop.service ceph-dr-firewall.service /etc/systemd/system/
sudo install -m0755 ceph-dr-firewall.sh /usr/local/sbin/
# Edit PEER= in ceph-dr-firewall.service to the other site's public IP first.
sudo systemctl daemon-reload && sudo systemctl enable --now ceph-dr-osd-loop ceph-dr-firewall
kubectl -n rook-ceph patch cm rook-ceph-operator-config --type merge \
  -p '{"data":{"ROOK_CEPH_ALLOW_LOOP_DEVICES":"true"}}'
kubectl create -f 00-rbac.yaml
kubectl apply -f 01-cephcluster.yaml      # edit the node name first
kubectl apply -f 03-rbd-mirror.yaml
NAMESPACE=rook-ceph-dr bash ../../scripts/ensure-atlas-auth-secret.sh
kubectl apply -f 02-atlas-gateway-dr.yaml # NodePort 30521
```

Allowing loop devices is operator-wide. That's safe only if every other CephCluster on the
operator names its devices (`useAllDevices: false`). Otherwise those clusters would claim
`/dev/loop30` too. The mons run msgr2 only (`requireMsgr2`) because one lab host already had
something on 6789.

## Peering (once, on one site)

Rook writes each site's bootstrap token to Secret `pool-peer-token-atlas-dr-mirror-test`. Copy site
A's token (keys `token` and `pool`) to site B as Secret `atlas-dr-site-peer`, then:

```bash
kubectl -n rook-ceph-dr patch cephblockpool atlas-dr-mirror-test --type merge \
  -p '{"spec":{"mirroring":{"peers":{"secretNames":["atlas-dr-site-peer"]}}}}'
```

Rook imports the token with its own admin credentials. That avoids the `(13) Permission denied`
that a manual `rbd mirror pool peer bootstrap import` hits under a scoped identity. The import
registers an `rx-tx` peer on **both** sites, so don't import site B's token on site A as well. Check
with `rbd mirror pool info atlas-dr-mirror-test` on each site. Because both clusters are
host-networked, each token's `mon_host` is already the routable `v2:<host-ip>:3300`.

## Teardown (each site)

```bash
kubectl -n rook-ceph-dr delete -f 02-atlas-gateway-dr.yaml -f 03-rbd-mirror.yaml
kubectl -n rook-ceph-dr patch cephcluster rook-ceph-dr --type merge \
  -p '{"spec":{"cleanupPolicy":{"confirmation":"yes-really-destroy-data"}}}'
kubectl delete -f 01-cephcluster.yaml && kubectl delete -f 00-rbac.yaml
sudo systemctl disable --now ceph-dr-firewall ceph-dr-osd-loop
sudo iptables -D INPUT -p tcp --dport 3300 -j ATLAS-DR; sudo iptables -D INPUT -p tcp --dport 6800:7300 -j ATLAS-DR
sudo iptables -F ATLAS-DR; sudo iptables -X ATLAS-DR
sudo rm -rf /var/lib/rook-dr /var/lib/ceph-dr-osd.img
```
