// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import type {ReactNode} from 'react';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';
import useBaseUrl from '@docusaurus/useBaseUrl';
import styles from './gallery.module.css';

type Shot = {
  src: string;
  caption: string;
};

const TOUR: Shot[] = [
  {src: '/00-overview.png', caption: 'Overview'},
  {src: '/01-volumes.png', caption: 'Volumes'},
  {src: '/02-observatory.png', caption: 'Observatory'},
  {src: '/03-ceph.png', caption: 'Ceph'},
  {src: '/04-databridge.png', caption: 'DataBridge'},
  {src: '/05-login.png', caption: 'Sign in'},
];

function ShotCard({shot}: {shot: Shot}) {
  const src = useBaseUrl(shot.src);
  return (
    <figure className={styles.shot}>
      <img src={src} alt={shot.caption} loading="lazy" />
      <figcaption>{shot.caption}</figcaption>
    </figure>
  );
}

export default function Gallery(): ReactNode {
  return (
    <Layout
      title="Gallery"
      description="A walkthrough of Atlas Storage Center, captured against a live lab deployment.">
      <header className={styles.header}>
        <div className="container">
          <p className={styles.eyebrow}>Product tour</p>
          <Heading as="h1">Atlas Storage Center</Heading>
          <p className={styles.lede}>
            Every frame is captured against a real lab deployment — not a
            mockup.
          </p>
        </div>
      </header>
      <main className={styles.main}>
        {TOUR.map((shot) => (
          <ShotCard key={shot.src} shot={shot} />
        ))}
      </main>
    </Layout>
  );
}
