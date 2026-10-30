<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# MLPerf Storage on atlas-native

[MLPerf Storage](https://github.com/mlcommons/storage) (MLCommons) measures how well a storage
system feeds AI training (data loading for `unet3d` and `retinanet`), absorbs LLM checkpoints
(`llama3-8b` to `llama3-1t`), serves vector-database search and holds inference KV-cache data. Atlas
ships a **harness** that runs the unmodified `mlpstorage` tool against an atlas-native filesystem
([`NATIVE_FS.md`](NATIVE_FS.md)) through the FUSE client, the same way [`IO500.md`](IO500.md) does
for IO500.

**Status: the harness is written but has not been built or run, and there is no MLPerf Storage
result and no submission.** The image build, the `mlpstorage` flags used below (taken from the
upstream README) and the mount step are untested here. Nothing on this page is a performance
claim; a representative run needs NVMe and network hardware the shared lab does not have, and
MLPerf's closed division defines accelerator counts and dataset sizes that must be chosen with
`mlpstorage ... datasize`.

## What is in the repo

| Piece | Role |
|---|---|
| `Dockerfile.mlperf` | Image `atlas-mlperf`: Ubuntu 24.04 (upstream's tested platform) with Open MPI, `uv`, a checkout of `mlcommons/storage` at `MLPERF_REF` (default `main`; pin a release tag for a reproducible image), and `atlas-native-mount` |
| `deploy/mlperf/mlperf-run.sh` | Entry point: mounts `MLPERF_FS`, runs `mlpstorage $MLPERF_ARGS`, unmounts |

The benchmark is a separate program; nothing in Atlas links it.

## Running it

The entry point reads its settings from the environment (see the header of
`deploy/mlperf/mlperf-run.sh`). It needs `/dev/fuse` and the capability to mount, as the IO500 image
does. `MLPERF_ARGS` is everything after `mlpstorage`; the script appends `--data-dir` (a directory on
the mounted filesystem), `--results-dir` and, when running as root, `--allow-run-as-root`.

```sh
podman build -f Dockerfile.mlperf -t atlas-mlperf .
# size first: how much data would this workload need? (no data or results directories)
podman run --rm --device /dev/fuse --cap-add SYS_ADMIN \
  -e ATLAS_NATIVE_ENDPOINTS=http://atlas-native:8080 -e MLPERF_FS=<fs-id> \
  -e MLPERF_APPEND_DIRS=0 \
  -e MLPERF_ARGS="closed training unet3d datasize file --num-accelerators 2 --accelerator-type h100 --client-host-memory-in-gb 64" \
  atlas-mlperf
# generate the dataset, then run
podman run ... -e MLPERF_ARGS="closed training unet3d datagen file --num-processes 4" atlas-mlperf
podman run ... -e MLPERF_ARGS="closed training unet3d run file --num-accelerators 2 --accelerator-type h100 --client-host-memory-in-gb 64" atlas-mlperf
```

Multi-host runs need passwordless SSH from the launching host to every client host and are not
covered by this single-container harness. Results land in `MLPERF_RESULTS` (default `/results`);
copy them out with `MLPERF_HOLD_SECS`.

## What would make this a real result

- A run on dedicated hardware with a dataset sized by `datasize` for the chosen accelerator count.
- The `closed` division's rules followed exactly; `whatif` runs are not submittable.
- Checkpointing and kv-cache families run separately: the atlas-native checkpoint fast path
  ([`NATIVE_FS.md`](NATIVE_FS.md), "Checkpoint fast path") is the part this would exercise.
