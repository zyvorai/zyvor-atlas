// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// Deterministic, idempotent sync of curated engineering docs (../docs/*.md)
// into website/docs/<category>/. Generated files carry a marker comment and
// are safe to regenerate; run with `npm run sync-docs` (also runs on prebuild).
import {readFileSync, writeFileSync, mkdirSync, existsSync, statSync, readdirSync, copyFileSync} from 'node:fs';
import {dirname, join, relative, resolve, posix} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const siteDir = resolve(here, '..');
const repoDir = resolve(siteDir, '..');
const srcDir = join(repoDir, 'docs');
const outDir = join(siteDir, 'docs');
const REPO = 'https://github.com/zyvorai/zyvor-atlas';

// [source file, category dir, slug (a leading slash makes it docs-root relative), position, sidebar label]
const DOCS = [
  ['GETTING_STARTED.md', 'getting-started', 'build-and-run', 2, 'Build and run locally'],
  ['DEPLOYMENT.md', 'getting-started', 'deployment', 3, 'Deployment'],
  ['ENVOY.md', 'getting-started', 'envoy', 4, 'Envoy front door'],

  ['ARCHITECTURE.md', 'core-concepts', 'architecture', 1, 'Architecture'],
  ['PRODUCTS.md', 'core-concepts', 'products', 2, 'Product integration'],
  ['API.md', 'api-reference', 'index', 1, 'API Reference'],

  ['DAY2.md', 'control-plane', 'day-2-operations', 1, 'Day-2 operations'],
  ['ALERTING.md', 'control-plane', 'alerting', 2, 'Alerting'],
  ['AI_ADVISOR.md', 'control-plane', 'ai-advisor', 3, 'AI Advisor'],
  ['DATABRIDGE.md', 'control-plane', 'databridge', 4, 'DataBridge'],
  ['DISKS.md', 'control-plane', 'disks', 5, 'Disk provisioning'],
  ['HA.md', 'control-plane', 'ha', 6, 'High availability'],
  ['SECRETS.md', 'control-plane', 'secrets', 7, 'Secrets'],
  ['TRACING.md', 'control-plane', 'tracing', 8, 'Tracing'],
  ['LONGHORN.md', 'control-plane', 'longhorn', 9, 'Longhorn driver'],
  ['RUSTFS.md', 'control-plane', 'rustfs', 10, 'RustFS (history)'],
  ['IO_EBPF.md', 'control-plane', 'io-ebpf', 11, 'eBPF I/O sensor'],
  ['IO_RISK.md', 'control-plane', 'io-risk', 12, 'I/O risk scoring'],
  ['MCP.md', 'control-plane', 'mcp', 13, 'MCP and agents'],
  ['HERMES_AGENT.md', 'control-plane', 'hermes-agent', 14, 'Hermes Agent'],
  ['MACOS.md', 'control-plane', 'macos', 15, 'macOS disks'],
  ['MACOS_CLIENT.md', 'control-plane', 'macos-client', 16, 'Mac as a client'],
  ['RELEASING.md', 'control-plane', 'releasing', 17, 'Releasing'],
  ['AI_DATA_SERVICES.md', 'control-plane', 'ai-data-services', 18, 'AI data services (experimental)'],
  ['IMAGE_IMPORT.md', 'control-plane', 'image-import', 19, 'Image import'],

  ['NATIVE_STORAGE.md', 'atlas-native', 'overview', 1, 'Storage overview'],
  ['NATIVE_NODE.md', 'atlas-native', 'node', 2, 'Node'],
  ['NATIVE_FS.md', 'atlas-native', 'filesystem', 3, 'Filesystem'],
  ['NATIVE_METADATA.md', 'atlas-native', 'metadata', 4, 'Metadata'],
  ['NATIVE_NFS.md', 'atlas-native', 'nfs', 5, 'NFS gateway'],
  ['NATIVE_SMB.md', 'atlas-native', 'smb', 6, 'SMB gateway'],
  ['NATIVE_S3.md', 'atlas-native', 's3', 7, 'S3 gateway'],
  ['NATIVE_CSI.md', 'atlas-native', 'csi', 8, 'CSI driver'],
  ['NATIVE_EDGE.md', 'atlas-native', 'edge', 9, 'Edge'],
  ['NATIVE_REPLICATION.md', 'atlas-native', 'replication', 10, 'Replication'],
  ['WORM.md', 'atlas-native', 'worm', 11, 'Retention locks and ransomware'],
  ['NATIVE_DATA_REDUCTION.md', 'atlas-native', 'data-reduction', 12, 'Compression and dedup'],
  ['IO500.md', 'atlas-native', 'io500', 13, 'IO500 benchmark'],

  ['DR.md', 'disaster-recovery', 'dr', 1, 'Disaster recovery (Ceph)'],
  ['NATIVE_REPLICATION.md', 'disaster-recovery', 'native-replication', 2, 'Native replication'],
  ['APPLICATION_RECOVERY.md', 'disaster-recovery', 'application-recovery', 3, 'Application recovery drills'],

  ['COMPARISON.md', 'evaluate', 'comparison', 1, 'Comparison'],
  ['STATUS.md', 'evaluate', 'status', 2, 'Maturity status'],
  ['LICENSING.md', 'evaluate', '/licensing', 3, 'Licensing'],
  ['SUBSCRIPTION-MODEL.md', 'evaluate', 'subscription-model', 4, 'Subscription model'],
  ['ROADMAP.md', 'evaluate', 'roadmap', 5, 'Roadmap'],
];

// Source file -> primary output (first listing wins for link targets).
const target = new Map();
for (const [src, cat, slug] of DOCS) {
  if (!target.has(src)) target.set(src, `${cat}/${slug.replace(/^\//, '')}.md`);
}

const warnings = [];

function rewriteLinks(text, srcFile, outRel) {
  const outFileDir = posix.dirname(outRel);
  return text.replace(/(!?\[[^\]]*\]\()([^)\s]+)((?:\s+"[^"]*")?\))/g, (m, pre, url, post) => {
    if (/^([a-z][a-z0-9+.-]*:|#|\/\/)/i.test(url)) return m;
    const [pathPart, ...anchorParts] = url.split('#');
    const anchor = anchorParts.length ? '#' + anchorParts.join('#') : '';
    if (!pathPart) return m;
    const abs = resolve(srcDir, pathPart);
    const fromRepo = relative(repoDir, abs).split('\\').join('/');
    if (abs.startsWith(srcDir + '/') && !pathPart.includes('/')) {
      const t = target.get(pathPart);
      if (t) {
        let rel = posix.relative(outFileDir, t);
        if (!rel.startsWith('.')) rel = './' + rel;
        return `${pre}${rel}${anchor}${post}`;
      }
    }
    // Sibling repos outside this repository: keep the text, drop the dead link.
    if (fromRepo.startsWith('..')) return pre.startsWith('!') ? m : pre.slice(1, pre.indexOf(']'));
    if (!existsSync(abs)) warnings.push(`${srcFile}: link target missing in repo: ${url}`);
    const kind = existsSync(abs) && statSync(abs).isDirectory() ? 'tree' : 'blob';
    return `${pre}${REPO}/${kind}/main/${fromRepo.replace(/\/$/, '')}${anchor}${post}`;
  });
}

function transform(raw, srcFile, outRel, slug, pos, label) {
  let body = raw.replace(/\r\n/g, '\n');
  // Strip leading license HTML comments.
  body = body.replace(/^(\s*<!--[\s\S]*?-->\s*\n)+/, '');
  const h1 = body.match(/^# +(.+?)\s*$/m);
  const title = h1 ? h1[1].replace(/[`*_]/g, '') : label;
  // Rewrite links outside fenced code blocks.
  const parts = body.split(/(^```[\s\S]*?^```[^\n]*$|^~~~[\s\S]*?^~~~[^\n]*$)/m);
  body = parts.map((p, i) => (i % 2 ? p : rewriteLinks(p, srcFile, outRel))).join('');
  const fm = [
    '---',
    `title: ${JSON.stringify(title)}`,
    `sidebar_label: ${JSON.stringify(label)}`,
    `sidebar_position: ${pos}`,
    slug === 'index' ? null : `slug: ${JSON.stringify(slug)}`,
    `custom_edit_url: ${JSON.stringify(`${REPO}/edit/main/docs/${srcFile}`)}`,
    '---',
    '',
    '<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->',
    '<!-- SPDX-License-Identifier: Apache-2.0 -->',
    `<!-- Generated by website/scripts/sync-docs.mjs from docs/${srcFile}. Do not edit here. -->`,
    '',
  ].filter((l) => l !== null);
  return fm.join('\n') + body.replace(/^\n+/, '');
}

let changed = 0;
for (const [src, cat, slug, pos, label] of DOCS) {
  const outRel = `${cat}/${slug.replace(/^\//, '')}.md`;
  const out = join(outDir, outRel);
  const text = transform(readFileSync(join(srcDir, src), 'utf8'), src, outRel, slug, pos, label);
  mkdirSync(dirname(out), {recursive: true});
  if (!existsSync(out) || readFileSync(out, 'utf8') !== text) {
    writeFileSync(out, text);
    changed++;
  }
}
// Homepage artwork lives with the docs (docs/ux/anim) and is copied to static/anim, served at /anim/*.
const animSrc = join(srcDir, 'ux', 'anim');
const animOut = join(siteDir, 'static', 'anim');
if (existsSync(animSrc)) {
  mkdirSync(animOut, {recursive: true});
  for (const f of readdirSync(animSrc)) copyFileSync(join(animSrc, f), join(animOut, f));
}
for (const w of warnings) console.warn('sync-docs warning:', w);
console.log(`sync-docs: ${DOCS.length} docs processed, ${changed} written`);
