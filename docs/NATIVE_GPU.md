<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# GPU / GPUDirect Storage capability probe (atlas-native)

**Status: UNVERIFIED on real GPU hardware.** The probe (`crates/atlas-native/src/gpu.rs`) was
written and tested against fixture directory trees only. The development host has no GPU, no CUDA
and no `nvidia_fs`. **No GPU I/O path exists in Atlas**, and none was tested: no cuFile or GDS read
or write is performed anywhere. The probe only reads files and says so in every report
(`"probe only, no I/O was performed; unverified on real GPU hardware"`).

## Where it shows up

`GET /v1/status` on `atlas-native-node` (authenticated like the rest of `/v1/*`) carries a `gpu_io`
object, probed once at node startup from `/`:

```json
{ "status": "gds|compat|none", "reasons": ["..."], "probe_only": true,
  "verified_on_hardware": false, "note": "probe only, no I/O was performed; ...",
  "evidence": { "nvidia_module": false, "nvidia_fs_module": false, "gpu_devices": [],
                "kernel_release": "...", "kernel_ok": true, "open_rm_ok": null,
                "allow_compat_mode": null, "force_compat_mode": null,
                "backends": ["file"], "direct_io_backend": false } }
```

It is not surfaced through the gateway, `atlasctl` or `atlas-io`: the gateway has no route to the
node's `/v1/status` today, `atlas-io` has no node-capability report to extend, and nothing that
needs privileges was added to the gateway.

## What it checks (all relative to a root; tests use a fixture sysroot)

| File | Used for |
| --- | --- |
| `proc/modules` | `nvidia`, `nvidia_fs`, `nvidia_peermem` loaded |
| `proc/driver/nvidia-fs/` | corroborates that `nvidia_fs` is live |
| `dev/nvidia<N>` | a GPU node exists (`nvidiactl`, `nvidia-uvm` alone do not count) |
| `proc/sys/kernel/osrelease` | Linux 6.2 or later (CUDA 12.8+ local-NVMe GDS through P2PDMA, per NVIDIA's documentation, not re-verified here) |
| `proc/driver/nvidia/version` | the open kernel module ("OpenRM") at major 570 or later (same source and caveat) |
| `etc/cufile.json` | `properties.allow_compat_mode`, `properties.force_compat_mode` (`//` and `/* */` comments are tolerated, as in NVIDIA's shipped file) |
| data node config | device backends: only `io_uring` opens with `O_DIRECT`; `file` and `aligned` do not |

## Status rules

- `none`: no `dev/nvidia<N>` node, or the `nvidia` module is not loaded.
- `gds`: only when **all** of these are corroborated: GPU present, `nvidia_fs` in `/proc/modules`
  **and** `/proc/driver/nvidia-fs` present, kernel >= 6.2, open driver >= 570, an `io_uring`
  backend, and `force_compat_mode` not true. This means "the machine looks capable", not "GDS
  works".
- `compat`: a GPU is present but a condition above is unmet, so cuFile would use its POSIX
  bounce-buffer path (or refuse, if `allow_compat_mode` is false; the report says so).

A file that is missing, unreadable (permission denied, wrong type) or malformed is "not
corroborated", never a pass, and the reason is listed. A malformed `cufile.json` leaves both flags
unknown.

## Limits

- The backing filesystem and block device are **not** checked (the probe only reads files); the
  report says so. GDS needs a supported filesystem and device; the NVIDIA support matrix was not
  verified here.
- The kernel and driver thresholds are taken from NVIDIA's documentation as understood at the time
  of writing and were not checked against real hardware. The `/proc/driver/nvidia/version` format
  parsed here is an assumption from memory of its usual shape.
- The probe is taken once at startup; restart the node after loading modules.
- Nothing about performance is claimed.

## The `cufile` cargo feature

`cufile` is declared in `crates/atlas-native/Cargo.toml`, default-off, as a placeholder for a future
lazily loaded (`dlopen`) cuFile binding. It adds **no dependency and no code** today; the probe
module is always compiled. Because it adds no compile surface, it is deliberately not in
`make native-features`; add it there when the binding exists.
