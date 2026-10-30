// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import type {ReactNode} from 'react';
import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useBaseUrl from '@docusaurus/useBaseUrl';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import FeatureHighlights from '@site/src/components/FeatureHighlights';
import ScreenshotStrip from '@site/src/components/ScreenshotStrip';
import Reveal from '@site/src/components/Reveal';
import {AnimatedFeatures, Compare, PromoteDemo, StatsBand, TypedTerminal} from '@site/src/components/Landing';

import styles from './index.module.css';

function HomepageHeader() {
  const heroShot = useBaseUrl('/00-overview.png');
  return (
    <header className={clsx('hero hero--primary', styles.heroBanner)}>
      <div className="container">
        <p className={styles.brandMark}>Atlas</p>
        <Heading as="h1" className="hero__title">
          One API for the storage you have. A filesystem for the storage you
          need.
        </Heading>
        <p className="hero__subtitle">
          A control plane that maps intent to Ceph, NFS, ZFS and Longhorn, plus
          Atlas Native: erasure-coded, Raft-backed, NFS, SMB and S3, with
          tamper-proof snapshots, replication and clean DR failback. Apache-2.0.
        </p>
        <div className={styles.buttons}>
          <Link
            className={clsx('button button--secondary button--lg', styles.pill)}
            to="/docs/getting-started/quickstart">
            Get Started
          </Link>
          <Link
            className={clsx(
              'button button--outline button--lg button--secondary',
              styles.pill,
            )}
            to="https://github.com/zyvorai/zyvor-atlas">
            View on GitHub
          </Link>
        </div>
        <TypedTerminal />
        <div className={styles.heroMedia}>
          <img
            src={heroShot}
            alt="Atlas Storage Center — Overview"
            onError={(e) => {
              (e.currentTarget as HTMLImageElement).style.display = 'none';
            }}
          />
        </div>
      </div>
    </header>
  );
}

function ProblemStatement() {
  return (
    <section className={styles.problem}>
      <div className="container">
        <Reveal className="text--center">
          <Heading as="h2" className={styles.sectionHeading}>
            Intent in. Backend details out.
          </Heading>
          <p className={styles.lede}>
            Products request production storage, not pool names or CSI quirks.
            Atlas owns inventory, ownership, audit and the async job engine, and
            when you need a filesystem of your own, Atlas Native runs it on your
            nodes.
          </p>
        </Reveal>
      </div>
    </section>
  );
}

function TrustBand() {
  return (
    <section className={styles.trust}>
      <div className="container">
        <Reveal className={styles.trustGrid}>
          <div>
            <Heading as="h3" className={styles.sectionHeading}>
              Open source. Apache License 2.0.
            </Heading>
            <p className={styles.lede}>
              Atlas is Apache-2.0: run it, modify it, and ship it — in
              production, in managed services, and in your own products.
            </p>
            <Link to="/docs/licensing">Read the licensing guide →</Link>
          </div>
          <div className={styles.trustBadges}>
            <img
              src="https://github.com/zyvorai/zyvor-atlas/actions/workflows/ci.yml/badge.svg"
              alt="CI status"
            />
            <img
              src="https://img.shields.io/badge/License-Apache%202.0-blue.svg"
              alt="Apache License 2.0"
            />
          </div>
        </Reveal>
      </div>
    </section>
  );
}

function EnterpriseCTA() {
  return (
    <section className={styles.enterprise}>
      <div className="container text--center">
        <Reveal>
          <Heading as="h2" className={styles.sectionHeading}>
            Running Atlas in production?
          </Heading>
          <p className={styles.enterpriseCopy}>
            The software is free under Apache-2.0. Talk to Zyvor about
            deployment help, support, and integrations.
          </p>
          <Link
            className={clsx('button button--primary button--lg', styles.pill)}
            to="https://zyvor.dev/schedule?utm_source=github&utm_medium=atlas&utm_campaign=site_footer">
            Book a demo
          </Link>
          <Link
            className={clsx('button button--outline button--lg button--primary', styles.pill)}
            to="https://zyvor.dev/pricing?utm_source=github&utm_medium=atlas&utm_campaign=site_footer">
            Pricing
          </Link>
        </Reveal>
      </div>
    </section>
  );
}

export default function Home(): ReactNode {
  return (
    <Layout
      title="Atlas — one API for your storage, and a filesystem of its own"
      description="Apache-2.0 storage platform: an intent API over Ceph, NFS, ZFS and Longhorn, plus Atlas Native, a distributed filesystem with erasure coding, NFS, SMB, S3, retention-locked snapshots, ransomware-aware auto-protect, replication and clean DR failback, and an MCP server so AI agents can triage storage read-only.">
      <HomepageHeader />
      <main>
        <StatsBand />
        <ProblemStatement />
        <AnimatedFeatures />
        <PromoteDemo />
        <Compare />
        <Reveal>
          <FeatureHighlights />
        </Reveal>
        <Reveal>
          <ScreenshotStrip />
        </Reveal>
        <TrustBand />
        <EnterpriseCTA />
      </main>
    </Layout>
  );
}
