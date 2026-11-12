<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# GPU / GPUDirect Storage capability probe (atlas-native)

**Status: UNVERIFIED on real GPU hardware.** The files probe (`crates/atlas-native/src/gpu.rs`) was
written and tested against fixture directory trees only. The development host has no GPU, no CUDA
and no `nvidia_fs`. That probe only reads files and says so in every report
(`"probe only, no I/O was performed; unverified on real GPU hardware"`). No cuFile read or write
of a GPU buffer has been run. With `--features cufile`, thin wrappers can call `cuFileRead` and
`cuFileWrite` when `libcufile` is present; see below. `io_performed` is not that transfer.

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
  works": that would need a cuFile read or write of a real buffer. The `cufile` feature's
  driver-open test call is not that run.
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

`cufile` is declared in `crates/atlas-native/Cargo.toml`, default-off. It compiles
`crates/atlas-native/src/cufile.rs`. Nothing is linked at build time. The module `dlopen`s
`libcufile.so.0` (then `libcufile.so`) and looks up `cuFileDriverOpen`, `cuFileDriverClose`,
`cuFileRead` and `cuFileWrite`.

- **Library or symbol missing.** [`read`](../crates/atlas-native/src/cufile.rs), `write`,
  `driver_open` and `driver_close` return `CuFileError` (`LibraryAbsent` or `SymbolMissing`).
  They do not panic and they do not call into cuFile. The unit test
  `absent_library_performs_no_io_and_the_wrappers_error` covers a host without the library
  (this development host). On that host `io_performed` is false.
- **Symbols present.** `read` and `write` call `cuFileRead` / `cuFileWrite` with the caller's
  `CUfileHandle_t` and buffer. They do not allocate a GPU buffer, register memory, or open a
  file. A negative return is `CuFileError::Io`. A zero-length buffer is refused
  (`CuFileError::Empty`) so it cannot be reported as a transfer. The caller must
  `driver_open` first; the wrapper does not hide that.
- **Test call.** Once per process, `probe` calls `cuFileDriverOpen` when that symbol exists.
  `io_performed` is true only when the returned `CUfileOpError` is success (`0`). The probe then
  calls `cuFileDriverClose` if that symbol exists. This is not a read or write of a device
  buffer. A failed open leaves `io_performed` false and puts the op/cu codes in `io_error`.
  The mapping is kept for the process (not `dlclose`d) so a later call is not a use-after-unload.
  `dlopen` may run the library's constructors.

When the feature is on, `gpu_io` in `/v1/status` gains a `cufile` object:

```json
{ "library_present": false, "library": null, "driver_open_symbol": false,
  "read_symbol": false, "write_symbol": false, "io_performed": false,
  "io_error": "libcufile is not installed",
  "note": "libcufile was not found; no cuFile call was made; ..." }
```

The files-only `status` (`gds` / `compat` / `none`) does not change, and that report's
`probe_only` stays true: the file probe did not transfer GPU bytes. `cufile.io_performed` is the
driver-open result, not a GDS read or write.

The feature stays out of `make native-features`. CI hosts have no `libcufile`, and the files-only
probe does not need the feature. A transfer of a real GPU buffer has not been run.
