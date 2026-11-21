<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# GPU telemetry and storage-starvation correlation

Status: **unit and HTTP tested over the fake source on a Linux dev host with no GPU. Real mode
(`nvidia-smi`) has not been run against a real GPU**, so the CSV column order and the `[N/A]` handling
are written from NVIDIA's documented `--query-gpu` fields and are unverified assumptions.

## What it is

An optional part of the `atlas-io` node agent (`crates/atlas-io/src/gpu.rs`), read-only, with the same
fake-first pattern as the I/O sources. It stays in the agent so the gateway never needs the NVIDIA tools.

| Setting | Meaning |
|---|---|
| `ATLAS_GPU_MODE=fake` | scripted GPUs that match the fake I/O demo pids (default with `ATLAS_IO_MODE=fake`) |
| `ATLAS_GPU_MODE=real` | runs `nvidia-smi` (arg arrays, 2 s snapshot cache); a missing binary or driver is an `error`, with empty lists |
| `ATLAS_GPU_MODE=off` | no GPU endpoints data (default with `ATLAS_IO_MODE=live`) |

## Agent endpoints

- `GET /io/gpu` per GPU utilisation, memory, temperature, power, uncorrected ECC and the compute processes.
- `GET /io/gpu-correlation` each GPU next to the storage requests and mean latency of the same pids.
- `GET /io/gds` read-only GPUDirect Storage readiness (the `nvidia_fs` module; run `gdscheck -p` for the rest).
- `GET /io/gpu-health` threshold findings (uncorrected ECC is `critical`; temperature at or above 85 °C and memory at or above 95% are `warning`).
- `GET /io/gpu-whatif?gpus=&per_gpu_mbps=&storage_mbps=` whether the stated storage bandwidth can feed that many GPUs (`ok` at 120% of need or more, `tight`, `starves`). Arithmetic on rates you supply: it does not measure bandwidth and does not model checkpoint writes or caching.
- `/io/gpu` lists each GPU process with its cgroup path (`/proc/<pid>/cgroup`), `null` when unreadable, so a container or job can be named.
- `/metrics` gains `atlas_gpu_*` gauges and `atlas_gpu_storage_starved{gpu}`.
- MCP tools `gpu_status`, `gpu_correlation` and `gpu_health` (the agent's `mcp` feature).

## Gateway and CLI

With `ATLAS_IO_AGENT_URL` set, the gateway serves `GET /api/atlas/v1/io/gpu` (operator role) and adds an `io_gpu`
block to `POST /ai/advisor` plus a warning when a GPU looks starved. The MCP tool `io_gpu` mirrors it.
`atlasctl io gpu`, `atlasctl io gpu-correlation` and `atlasctl io-gpu` are the CLI forms. Like the other
`io_*` views, this never changes `risk_score`, fails open as `unavailable`, whitelists verdicts and does not
forward the agent's evidence text.

## The starvation verdict, and its limits

`storage_starved_suspected` means: the GPU has processes, at least 20 storage requests are accounted to those
pids, GPU utilisation is at or below 40%, and the mean request latency is at least 2 ms. Other verdicts are
`idle`, `insufficient_data`, `underused_storage_ok` and `compute_bound_or_ok`.

It is a heuristic over one sample. It cannot see the data loader, network reads, CPU-bound preprocessing or
batch size, and per-pid accounting only covers I/O on devices the agent sees. The thresholds are constants in
`gpu.rs`, not tuned on real training jobs. Every row lists the numbers behind it, and nothing acts on it.

## Not done

Real-GPU verification, DCGM, MIG partitions, Xid log parsing, and folding GPU health into the advisor's
`risk_score`.
