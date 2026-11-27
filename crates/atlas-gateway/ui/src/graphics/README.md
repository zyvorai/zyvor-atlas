<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->

# Console graphics

Inline SVG and CSS graphics for the Atlas console. No raster assets and no dependencies beyond
React. Import from `./graphics`; each component pulls in `graphics.css` itself.

Rules every component follows:

- Data arrives through typed props. Components never fetch.
- `null` or `undefined` means "not reported". It renders as words, never as a number.
- Colour comes only from `--a-surface`, `--a-surface-2`, `--a-bg`, `--a-ink`, `--a-ink-2`,
  `--a-line`, `--a-blue`, `--a-ok`, `--a-warn`, `--a-fail` (plus `--a-radius`, `--a-font`,
  `--a-mono`). The fallbacks are light-theme values; define the tokens under `data-theme="dark"`
  and `"light"`.
- State is a shape or line style plus words, not colour alone.
- Motion runs only under `prefers-reduced-motion: no-preference`.

## Components

### `ClusterHero`

One chassis per host, one bay per disk, and a replication bus.

| Prop | Type | Notes |
| --- | --- | --- |
| `nodes` | `HeroNode[] \| null` | `{ host, disks: HeroDisk[] }`; empty state when missing |
| `clusterName` | `string` | |
| `health` | `HealthValue \| null` | omit to hide the indicator; `null` shows "Not reported" |
| `replicaSizes` | `number[] \| null` | the bus appears only with a size above 1 and two or more hosts |
| `rebuilding` | `boolean` | pass true only for real recovery or backfill; animates the bus |
| `loading` | `boolean` | |
| `maxNodes`, `maxDisksPerNode` | `number` | default 12; failed disks are drawn first when truncating |

The bus shows that pools keep copies. It does not show which host holds which copy.

### `CapacityRing`

`usedBytes`, `totalBytes` (`number | null`), `label` (default "Capacity"), `size` (max px, 220),
`warnAt` (70), `failAt` (85), `loading`.

### `PoolUsageBars`

`pools: PoolUsage[] | null`, `label`, `limit`, `onSelect(pool)`, `warnAt`, `failAt`, `loading`.
`PoolUsage` is `{ id, name, usedBytes?, capacityBytes?, availableBytes?, percentUsed?, health? }`.
The fill is `percentUsed`, else `usedBytes / capacityBytes`, else "Fill unknown".

### `TrendChart`

| Prop | Type | Notes |
| --- | --- | --- |
| `title` | `string` | required |
| `series` | `TrendSeries[] \| null` | at most two, same unit; `points: { t: epochMs, v: number \| null }` |
| `formatValue` | `(v) => string` | `formatBytesPerSecond`, `formatMicros`, or your own |
| `variant` | `"area" \| "sparkline"` | |
| `height` | `number` | 220, or 44 for a sparkline |
| `maxGapMs` | `number` | longest silence a line bridges; default 3x the median spacing |
| `loading` | `boolean` | |

The area variant has a hover and arrow-key readout and a data table. Needs two measurements.

### `HealthIndicator`

`health` (`Health | ClusterHealthState | null`), `label`, `detail`, `size` (`"sm" | "lg"`),
`iconOnly`. `StatusGlyph` (`tone`, `size`) and `healthMeta(value)` are exported too.

### `SpotIllustration` and `EmptyState`

`SpotIllustration`: `kind` (`cluster`, `volume`, `pool`, `snapshot`, `backup`, `bucket`,
`chart`, `alert`), `size`, `title` (omit for decorative art).

`EmptyState`: `art` (a kind), `title`, `description`, `compact`, and actions as children.

All components also accept `className`.

## Usage

```tsx
import {
  CapacityRing, ClusterHero, PoolUsageBars, TrendChart, capacityFromSummary,
  formatBytesPerSecond, poolUsageFromCephDf, replicaSizesFromPools, throughputSeries,
  topologyFromOsds,
} from "../graphics";

const { data: summary } = useSummary();
const { data: osds, isLoading } = useOsds();
const { data: pools } = usePools();
const { data: df } = useCephDf();
const { data: history } = useHistory();
const rc = summary?.recovery;

<ClusterHero
  nodes={topologyFromOsds(osds)}
  loading={isLoading}
  replicaSizes={replicaSizesFromPools(pools)}
  rebuilding={!!rc && rc.pg_recovering + rc.pg_backfilling > 0}
/>
<CapacityRing {...capacityFromSummary(summary)} />
<PoolUsageBars pools={poolUsageFromCephDf(df)} limit={6} />
<TrendChart title="Client throughput" series={throughputSeries(history)}
  formatValue={formatBytesPerSecond} />
```

## Adapters

`topologyFromOsds`, `replicaSizesFromPools`, `capacityFromSummary`, `throughputSeries`,
`iopsSeries` (counter resets become gaps), `latencySeries` (from `/io/heatmap` series),
`poolUsageFromCephDf` and `poolUsageFromPools`.

`StoragePool.max_bytes` means different things per driver, so `poolUsageFromPools` requires
`"total"` or `"available"`. The real Ceph driver reports `max_avail` there, and its `used_bytes`
includes replicas, so `"available"` shows both figures without a fill level. For a Ceph fill level
use `poolUsageFromCephDf`.

The real Ceph driver does not report `replica_size`, so the hero says "Replication not reported"
and draws no bus there.
