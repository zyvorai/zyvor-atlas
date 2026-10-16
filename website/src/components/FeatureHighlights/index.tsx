// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Heading from '@theme/Heading';
import styles from './styles.module.css';

type FeatureItem = {
  eyebrow: string;
  title: string;
  description: ReactNode;
  to: string;
  cta: string;
};

const FeatureList: FeatureItem[] = [
  {
    eyebrow: 'Control plane',
    title: 'Intent → any backend',
    description:
      'Volumes, snapshots, clones, CephFS RWX and buckets over REST and gRPC, mapped to Ceph, NFS, ZFS and Longhorn by one StorageDriver trait. A fake driver runs the whole console with no cluster.',
    to: '/docs/core-concepts/architecture',
    cta: 'Architecture',
  },
  {
    eyebrow: 'Atlas Native',
    title: 'A filesystem of its own.',
    description:
      'Striped, checksummed extents with sharded Raft metadata, erasure coding and rebuild, S3 tiering, and NFS, SMB, S3 and CSI over the same files. Lab-verified; NVMe and RDMA benchmarks pending.',
    to: 'https://github.com/zyvorai/zyvor-atlas/blob/main/docs/NATIVE_STORAGE.md',
    cta: 'Native roadmap status',
  },
  {
    eyebrow: 'DataBridge',
    title: 'Cloud to edge, on Ceph.',
    description:
      'Six source engines, CDC, cutover, and Ceph-backed edge targets — migration as a control-plane product.',
    to: '/docs/getting-started/quickstart',
    cta: 'Quickstart',
  },
  {
    eyebrow: 'Agent-ready',
    title: 'Let an agent look. Not leap.',
    description:
      'An MCP server with 20 read-only tools: inventory, the Ops Advisor, eBPF I/O risk, DR and jobs, with the same roles and tenant limits as the API. One opt-in write, snapshots, behind a single-use confirmation token and an audit record. Failover and delete are never exposed.',
    to: '/docs/control-plane/mcp',
    cta: 'MCP and agents',
  },
  {
    eyebrow: 'Day-2 and DR',
    title: 'Fail over. Fail back cleanly.',
    description:
      'Alerts, maintenance, quotas, upgrade preflight, two-way RBD mirroring (snapshot, journal and pool modes) with a guarded failback, RBD consistency groups, and native replication to a second site.',
    to: '/gallery',
    cta: 'See the console',
  },
];

function Feature({eyebrow, title, description, to, cta}: FeatureItem) {
  return (
    <article className={styles.band}>
      <div className={styles.inner}>
        <p className={styles.eyebrow}>{eyebrow}</p>
        <Heading as="h2" className={styles.title}>
          {title}
        </Heading>
        <p className={styles.copy}>{description}</p>
        <Link className={styles.link} to={to}>
          {cta} →
        </Link>
      </div>
    </article>
  );
}

export default function FeatureHighlights(): ReactNode {
  return (
    <section className={styles.features} aria-label="Capabilities">
      {FeatureList.map((props) => (
        <Feature key={props.title} {...props} />
      ))}
    </section>
  );
}
