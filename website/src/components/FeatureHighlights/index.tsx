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
    eyebrow: 'Data protection',
    title: 'Locked means locked.',
    description:
      'Retention-locked snapshots that refuse delete and shorter locks, S3 compliance retention, and an explainable ransomware assessment that can take a locked snapshot for you. Lab-verified 27 of 27 on a synthetic burst; a real encrypting workload is still to come.',
    to: '/docs/atlas-native/worm',
    cta: 'Retention and ransomware',
  },
  {
    eyebrow: 'Efficiency',
    title: 'Store less, know exactly how much.',
    description:
      'Opt-in LZ4 compression and extent dedup on Atlas Native, with a per-volume savings report. Reads stay byte-exact; the report separates compression from sharing so snapshots and clones are not passed off as dedup.',
    to: '/docs/atlas-native/data-reduction',
    cta: 'Data reduction',
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
      'An MCP server with 27 read-only tools: inventory, the Ops Advisor, eBPF I/O risk, Ceph, DR and jobs, with the same roles and tenant limits as the API. Two opt-in writes, snapshots and alert acknowledgement, each behind a single-use confirmation token and an audit record. Failover and delete are never exposed.',
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
