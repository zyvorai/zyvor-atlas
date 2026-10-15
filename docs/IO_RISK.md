<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas I/O risk scoring

`atlas_io::risk` turns RCA results (`atlas_io::rca`) plus an optional capacity signal into one
`RiskLevel`. It is deterministic, read-only and makes no external calls. It is a **library
helper only**: it is not exposed through an HTTP route, `atlasctl`, or the Ops Advisor yet.

## Levels

The result is the worst level over all RCA entries and the capacity signal.

| Level | Triggers |
|---|---|
| Healthy | no elevated latency, queue pressure or capacity pressure |
| Elevated | verdict `elevated_latency`; capacity ≥ 75 % used or < 7 days to full |
| Critical | verdict `critical_latency`; RCA note `cause=scheduler_queue_pressure` or `cause=mixed_queue_and_service`; capacity ≥ 90 % used |

## Evidence rules

- Queue pressure is read from the RCA's `cause=` notes, so the RCA's own guards apply (missing
  queue events, inconsistent counters and small samples never produce a queue cause).
- A verdict of `unknown` (no completions) is not evidence and scores Healthy.
- A capacity `used_percent` that is NaN or outside 0–100, or a negative / NaN `days_to_full`,
  is ignored rather than guessed at.

```rust
use atlas_io::{score, CapacitySignal, RiskLevel};
let level = score(&rca_results, Some(CapacitySignal { used_percent: 82.0, days_to_full: Some(4.0) }));
assert!(level >= RiskLevel::Elevated);
```
