---
title: Atlas documentation
sidebar_label: Overview
sidebar_position: 1
slug: /
---

<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Atlas documentation

**One API for the storage you have. A filesystem for the storage you need.**

Atlas is a storage control plane. It gives you one API and one console over the backends you already run (Ceph, Longhorn, ZFS, NFS and more), and it ships Atlas Native, a filesystem with NFS, SMB, S3 and CSI front ends for the storage you still need to build.

## Choose your path

| I want to... | Start here |
| --- | --- |
| Evaluate Atlas | [Comparison](./evaluate/comparison.md), [Maturity status](./evaluate/status.md), [Subscription model](./evaluate/subscription-model.md) |
| Run the control plane | [Quickstart](./getting-started/quickstart.md), [Build and run locally](./getting-started/build-and-run.md), [Deployment](./getting-started/deployment.md), [Architecture](./core-concepts/architecture.md) |
| Run Atlas Native | [Storage overview](./atlas-native/overview.md), [Node](./atlas-native/node.md), [Filesystem](./atlas-native/filesystem.md), [CSI driver](./atlas-native/csi.md) |
| Protect data from ransomware and deletion | [Retention locks and ransomware](./atlas-native/worm.md), [Compression and dedup](./atlas-native/data-reduction.md) |
| Build a product on Atlas | [Product integration](./core-concepts/products.md) (includes the Rust client), [API Reference](./api-reference/index.md), [AI data services](./control-plane/ai-data-services.md) |
| Set up disaster recovery | [Disaster recovery (Ceph)](./disaster-recovery/dr.md), [Native replication](./disaster-recovery/native-replication.md) |
| Operate day to day | [Day-2 operations](./control-plane/day-2-operations.md), [Alerting](./control-plane/alerting.md), [High availability](./control-plane/ha.md), [API Reference](./api-reference/index.md) |

## A note on maturity

Lab verification is not production support. The [maturity status](./evaluate/status.md) page is the source of truth for what has been verified on real infrastructure and what is lab-only. When the roadmap and the status page disagree, the status page wins. Production use is covered by an enterprise [subscription](./evaluate/subscription-model.md).
