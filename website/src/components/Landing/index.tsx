// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import {useEffect, useState} from 'react';
import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import Heading from '@theme/Heading';
import Reveal from '@site/src/components/Reveal';
import {reducedMotion, useCountUp, useInView} from './hooks';
import styles from './styles.module.css';

const LINES = [
  '$ atlasctl volume create db-data --size 200Gi --class fast',
  '  ceph-rbd  ->  pool atlas-fast  ·  owner zorvia  ·  audit entry written',
  '$ atlasctl dr status mirror-prod',
  '  site A primary  ·  site B replaying  ·  promote would be CLEAN',
  '$ atlasctl dr promote mirror-prod --site B',
  '  refused until B has replayed A\'s demotion (use --force for a real disaster)',
];

export function TypedTerminal(): ReactNode {
  const total = LINES.join('\n').length;
  const [n, setN] = useState(total);
  useEffect(() => {
    if (reducedMotion()) return;
    setN(0);
    const id = setInterval(() => setN((v) => (v >= total + 40 ? 0 : v + 1)), 35);
    return () => clearInterval(id);
  }, [total]);
  return (
    <div className={styles.term} role="img" aria-label="Terminal showing atlasctl creating a volume and a guarded DR promote">
      <div className={styles.termBar}><i /><i /><i /></div>
      <pre>{LINES.join('\n').slice(0, n)}<span className={styles.caret} /></pre>
    </div>
  );
}

function Stat({n, suffix = '', label, run}: {n: number; suffix?: string; label: string; run: boolean}) {
  const v = useCountUp(n, run);
  return (
    <div className={styles.stat}>
      <b>{v}{suffix}</b>
      <span>{label}</span>
    </div>
  );
}

export function StatsBand(): ReactNode {
  const [ref, seen] = useInView<HTMLDivElement>();
  return (
    <div className={clsx('container', styles.stats)} ref={ref}>
      <Stat n={4} suffix=" + Native" label="storage backends" run={seen} />
      <Stat n={150} suffix="+" label="REST endpoints" run={seen} />
      <Stat n={6} label="DataBridge DB engines" run={seen} />
      <Stat n={2} suffix="-way" label="DR with clean failback" run={seen} />
    </div>
  );
}

const BLOCKS = [
  {
    tag: 'Control plane',
    title: 'One API, every backend.',
    body: 'Products state intent: volumes, snapshots, clones, CephFS RWX, S3 buckets. The StorageDriver trait maps it to Ceph, NFS, ZFS, Longhorn, macOS disks or Atlas Native. Real drivers never fabricate data, and every driver has a fake mode.',
    items: ['REST, gRPC and SSE', 'Quotas, audit export, OIDC/SSO', 'DataBridge database migration'],
    img: '/anim/control-plane.svg',
    alt: 'Animated control plane routing intent to storage drivers',
    to: '/docs/core-concepts/architecture',
  },
  {
    tag: 'Atlas Native',
    title: 'Run the filesystem yourself.',
    body: 'Striped, checksummed extents, sharded Raft metadata, 3 replicas or Reed-Solomon k+m, cold extents tiered to S3, and NFS, SMB, S3 and CSI over the same files. Retention-locked snapshots, opt-in LZ4 compression and dedup, and a ransomware assessment (unit-tested; only snapshot locks are lab-verified). One small pod for edge sites.',
    items: ['Erasure coding with a rebuild controller', 'POSIX ACLs and quotas', 'Single-node edge profile'],
    img: '/anim/native-io.svg',
    alt: 'Animated Atlas Native data path',
    to: '/docs/',
  },
  {
    tag: 'Agent-ready operations',
    title: 'Let an agent look. Not leap.',
    body: 'Atlas speaks the Model Context Protocol, so an AI agent can triage storage the way an operator does: the Ops Advisor, anomalies, eBPF I/O risk, DR status and jobs. It is read-only by default; the two opt-in writes (a snapshot, an alert acknowledgement) each need a single-use confirmation token, and MCP can never call out to an external LLM.',
    items: ['27 read-only gateway tools, 6 on the node agent', 'Same roles, tenant scoping and audit as the REST API', 'Opt-in guarded snapshot and alert ack; failover and delete never exposed'],
    img: '/anim/agent-ops.svg',
    alt: 'An AI agent calling Atlas over MCP: read-only tools and guarded writes',
    to: '/docs/control-plane/mcp',
  },
  {
    tag: 'Disaster recovery',
    title: 'Fail over, fail back cleanly.',
    body: 'Two-way RBD mirroring and native replication. A non-forced promote is refused until this site has replayed the peer\'s demotion, so a failback cannot silently lose writes. Force stays available for real disasters.',
    items: ['Live mirror status per site', 'Promote guard', 'Native replication with read-only replicas', 'Automated application recovery drills'],
    img: '/anim/dr-failover.svg',
    alt: 'Animated cross-site failover and failback',
    to: '/docs/',
  },
];

export function AnimatedFeatures(): ReactNode {
  return (
    <section className={styles.features}>
      <div className="container">
        {BLOCKS.map((b, i) => (
          <Reveal key={b.tag} className={clsx(styles.block, i % 2 === 1 && styles.flip)}>
            <div className={styles.copy}>
              <p className={styles.eyebrow}>{b.tag}</p>
              <Heading as="h2" className={styles.h2}>{b.title}</Heading>
              <p>{b.body}</p>
              <ul>{b.items.map((t) => <li key={t}>{t}</li>)}</ul>
              <Link to={b.to}>Read the docs →</Link>
            </div>
            <img className={styles.art} src={useBaseUrl(b.img)} alt={b.alt} loading="lazy" />
          </Reveal>
        ))}
      </div>
    </section>
  );
}

type Mode = 'journal' | 'snapshot';

export function PromoteDemo(): ReactNode {
  const [replayed, setReplayed] = useState(false);
  const [force, setForce] = useState(false);
  const [mode, setMode] = useState<Mode>('journal');
  const allowed = replayed || force;
  return (
    <section className={styles.demo}>
      <div className="container">
        <Reveal>
          <p className={styles.eyebrow}>Try it</p>
          <Heading as="h2" className={styles.h2}>Would this promote be safe?</Heading>
          <p className={styles.lede}>A model of the DR guard, not a live system. Toggle the state and try to promote site B.</p>
          <div className={styles.demoGrid}>
            <div className={styles.card}>
              <div className={styles.seg}>
                {(['journal', 'snapshot'] as Mode[]).map((m) => (
                  <button key={m} className={mode === m ? styles.on : ''} onClick={() => setMode(m)}>{m} mode</button>
                ))}
              </div>
              <label className={styles.check}>
                <input type="checkbox" checked={replayed} onChange={(e) => setReplayed(e.target.checked)} />
                Site B has replayed A's demotion
              </label>
              <label className={styles.check}>
                <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
                Use ?force=1 (real disaster)
              </label>
            </div>
            <div className={styles.card}>
              <p className={styles.mono}>POST /dr/mirrors/prod/promote{force ? '?force=1' : ''}</p>
              <p className={clsx(styles.verdict, allowed ? styles.ok : styles.refused)}>
                {allowed
                  ? force && !replayed
                    ? 'Promoted by force. Writes since the last replay may be lost.'
                    : 'Clean promote. No writes lost.'
                  : 'Refused: the local rbd-mirror has not replayed the peer\'s demotion yet.'}
              </p>
              <p className={styles.note}>Mode: {mode}. Mirroring status is read live from rbd mirror image status.</p>
            </div>
          </div>
        </Reveal>
      </div>
    </section>
  );
}

const ROWS = [
  ['What it is', 'Control plane with drivers per backend, plus its own filesystem', 'Kubernetes operator that runs Ceph'],
  ['Interface', 'Intent-based REST, gRPC and SSE', 'Custom resources and CSI PVCs'],
  ['Backends', 'Ceph, NFS, ZFS, Longhorn, macOS disks, Atlas Native, S3', 'Ceph'],
  ['Cross-site DR', 'Two-way mirroring with a guarded failback; native replication', 'RBD mirroring CRDs'],
  ['Database migration', 'DataBridge: discovery, full load, CDC, cutover', 'Not in scope'],
  ['AI and agents', 'MCP server (27 read-only tools, opt-in guarded snapshot and alert ack), Ops Advisor, eBPF I/O risk', 'Not in scope'],
];

export function Compare(): ReactNode {
  return (
    <section className={styles.compare}>
      <div className="container">
        <Reveal>
          <p className={styles.eyebrow}>Atlas vs Rook alone</p>
          <Heading as="h2" className={styles.h2}>Keep Ceph. Add one API over all your storage.</Heading>
          <div className={styles.tableWrap}>
            <table>
              <thead><tr><th /><th>Atlas</th><th>Rook alone</th></tr></thead>
              <tbody>{ROWS.map((r) => <tr key={r[0]}><th>{r[0]}</th><td>{r[1]}</td><td>{r[2]}</td></tr>)}</tbody>
            </table>
          </div>
          <p className={styles.note}>Choose Rook alone when you only run Ceph inside Kubernetes and its own dashboard and CRDs are enough. Versus WEKA, including where WEKA is better today: <Link to="/docs/evaluate/comparison">the comparison</Link>.</p>
        </Reveal>
      </div>
    </section>
  );
}
