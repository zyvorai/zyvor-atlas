// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'Atlas',
  tagline: 'One API for the storage you have. A filesystem for the storage you need.',
  favicon: 'img/favicon.svg',

  future: {
    v4: true,
  },

  url: 'https://zyvorai.github.io',
  baseUrl: '/zyvor-atlas/',

  organizationName: 'zyvorai',
  projectName: 'zyvor-atlas',

  onBrokenLinks: 'throw',

  markdown: {
    // Synced engineering docs are plain CommonMark; render .md as such, .mdx as MDX.
    format: 'detect',
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  staticDirectories: ['static', '../docs/ux', '../docs/social'],

  presets: [
    [
      'classic',
      {
        docs: {
          sidebarPath: './sidebars.ts',
          editUrl: 'https://github.com/zyvorai/zyvor-atlas/tree/main/website/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    image: 'img/atlas-social.jpg',
    colorMode: {
      defaultMode: 'dark',
      respectPrefersColorScheme: false,
    },
    navbar: {
      title: 'Atlas',
      logo: {
        alt: 'Atlas',
        src: 'img/favicon.svg',
      },
      items: [
        {
          type: 'docSidebar',
          sidebarId: 'docsSidebar',
          position: 'left',
          label: 'Docs',
        },
        {
          to: '/gallery',
          label: 'Gallery',
          position: 'left',
        },
        {to: '/support', label: 'Support', position: 'right'},
        {
          href: 'https://github.com/zyvorai/zyvor-atlas',
          label: 'GitHub',
          position: 'right',
        },
        {
          href: 'https://zyvor.dev/pricing?utm_source=github&utm_medium=atlas&utm_campaign=docs_site',
          label: 'Pricing',
          position: 'right',
        },
        {
          href: 'https://zyvor.dev',
          label: 'Enterprise',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Docs',
          items: [
            {label: 'Overview', to: '/docs/'},
            {label: 'Quickstart', to: '/docs/getting-started/quickstart'},
            {label: 'Architecture', to: '/docs/core-concepts/architecture'},
            {label: 'API Reference', to: '/docs/api-reference'},
            {label: 'MCP and agents', to: '/docs/control-plane/mcp'},
            {label: 'Licensing', to: '/docs/licensing'},
          ],
        },
        {
          title: 'Project',
          items: [
            {label: 'Support', to: '/support'},
            {label: 'GitHub', href: 'https://github.com/zyvorai/zyvor-atlas'},
            {
              label: 'Changelog',
              href: 'https://github.com/zyvorai/zyvor-atlas/blob/main/CHANGELOG.md',
            },
            {
              label: 'License',
              href: 'https://github.com/zyvorai/zyvor-atlas/blob/main/LICENSE',
            },
          ],
        },
        {
          title: 'Zyvor Enterprise',
          items: [
            {label: 'zyvor.dev', href: 'https://zyvor.dev'},
          ],
        },
      ],
      copyright: `Copyright © ${new Date().getFullYear()} ZyvorAI Labs. Apache License 2.0.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
