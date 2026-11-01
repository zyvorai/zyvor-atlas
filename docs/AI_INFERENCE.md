<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Inference KV-cache storage on Atlas

Status: **a compatibility recipe and a smoke test, not a connector.** Atlas has not been run against LMCache,
NVIDIA Dynamo, vLLM or any other inference engine, and no speed-up or throughput is claimed anywhere on this page.

## What inference engines ask of storage

Inference engines can spill KV-cache blocks from GPU and CPU memory to a lower tier. From the vendors' own
documentation (read as search results, not tested by us), the lower tier is reached through one of:

| Path | Where it appears |
|---|---|
| Files on a POSIX or NFS mount | Dynamo KV Block Manager (local and remote file targets), LMCache local-disk and NIXL POSIX backends |
| S3-compatible object store | Dynamo KV Block Manager, NIXL object plugin |
| NIXL storage plugins (POSIX, object, GPUDirect Storage, HF3FS) | Dynamo and the LMCache NIXL L2 adapter; GDS plugins need CUDA and cuFile |
| A connector class inside the engine | vLLM's KV connector interface, which the engines above implement |

Sources: Dynamo KVBM architecture (docs.nvidia.com/dynamo/v-0-8-0/components/kvbm/architecture), LMCache NIXL L2
storage (docs.lmcache.ai/mp/l2_storage/nixl.html). NVIDIA's own pages on GPUDirect Storage filesystem
requirements and on NFS or S3 over RDMA were not checked, so they are not relied on here.

## What Atlas offers on those paths

The atlas-native cluster already serves the first two paths:

| Interface | Component | Notes |
|---|---|---|
| POSIX mount | `atlas-native-mount` (FUSE), the CSI driver | `O_DIRECT` is supported through FUSE with no alignment requirement ([NATIVE_FS.md](NATIVE_FS.md#direct-io)) |
| NFS | `atlas-native-nfs` ([NATIVE_NFS.md](NATIVE_NFS.md)) | NFSv4.1 and v4.2 |
| S3 | `atlas-native-s3` ([NATIVE_S3.md](NATIVE_S3.md)) | multipart and ranged `GetObject`; the bucket is a filesystem |

The engine owns tensor serialisation, block sizing and eviction. Atlas stores and returns bytes.

The engine-neutral `KVAdapter` in `tools/atlas_data/client.py` ([AI_DATA_SERVICES.md](AI_DATA_SERVICES.md#prefix-blocks))
is a separate, content-addressed store with tenant-scoped keys. It is not a vLLM or Dynamo connector.

## Pointing an engine at Atlas

These are the steps an operator takes. The engine-side settings are the engine's, so follow its documentation;
none of this has been exercised against a real engine.

1. Create a filesystem volume (`atlasctl create-volume kv-cache --kind filesystem --backend bkd_native`).
2. Put it where the engine runs: mount it on the GPU node with `atlas-native-mount`, a CSI `PersistentVolumeClaim`,
   or an NFS export. For an S3-based tier, add the filesystem as a bucket in the S3 gateway.
3. Give the engine that directory (file backend) or bucket (S3 backend) as its cache location.
4. Run the smoke test below against the same path before trusting it.

## Smoke test

`deploy/native/kvcache-smoke.sh <dir>` exercises the I/O pattern of a KV-cache tier against any directory, and
optionally the S3 gateway:

- direct-I/O write of many multi-MiB block files, published by rename;
- byte-exact read-back, both direct and buffered;
- concurrent readers over random blocks;
- a block replaced by rename is never seen half-written;
- delete removes a block;
- with `S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY` and `S3_SECRET_KEY`: a put and get, and a ranged get, byte-exact.

It prints one `PASS`, `FAIL` or `SKIP` line per check and measures no throughput. On the lab,
`scripts/lab-kvcache-remote.sh <host> <user>` starts a throwaway three-node cluster, mounts it over FUSE, serves it
as an S3 bucket and runs the smoke test over both.

## Lab result (2026-11-01)

`scripts/lab-kvcache-remote.sh` on the shared lab host: three metadata nodes and three data nodes (debug build, one
container, replicas 3, cluster `extent_bytes` 4 MiB), an `atlas-native-mount` FUSE mount and the S3 gateway on the same
filesystem. 64 blocks of 2048 KiB. Raw output:

```
PASS   direct-I/O write + rename of 64 blocks of 2048 KiB
PASS   no temp files left after rename
PASS   direct read-back is byte-exact
PASS   buffered read-back is byte-exact
PASS   8 concurrent readers read every block byte-exactly
PASS   rename-over replacement never exposes a partial block
PASS   unlink removes a block
PASS  S3 put + get is byte-exact
PASS  S3 ranged get returns the requested bytes
ALL CHECKS PASSED
```

This shows the interfaces behave correctly for a KV-cache I/O pattern on one host. It is not a performance result, not a
multi-host result, and no inference engine was involved.

Earlier runs of the same test failed, and the cause was the test setup, not the data path:

- With the lab's usual 4 KiB cluster extent size, every 2 MiB block became 512 replicated extents. After about 115 MiB
  the metadata group stopped holding a leader (term 324, no leader) and writes failed with `EIO` (S3: HTTP 500). Use the
  default extent size for large blocks; do not copy the 4 KiB lab setting. That the metadata group loses its leader
  under this much metadata churn on a loaded debug build is a robustness limit we saw but did not investigate.
- Clients started with only the first leader's URL get `421 not_leader` after leadership moves. Give the mount and the
  S3 gateway every metadata node: `--endpoint http://m1:7400,http://m2:7400,http://m3:7400`.

## Single-copy profile for cache data

`deploy/helm/atlas-native/values-cache.yaml` keeps three pods and three metadata voters but stores one copy of
each extent (`node.replicationFactor: 1`), for data that can be recomputed. Losing a pod or its disk loses the
blocks stored on it, so reads of those fail and the engine has to treat them as a miss. Do not put anything here
that cannot be rebuilt. Scrub finds corruption but has nothing to repair from, so the profile runs it daily.

Lab result (2026-11-01): the same smoke test with `RF=1 scripts/lab-kvcache-remote.sh <host> <user>` (three
metadata nodes and three data nodes, one copy per extent) passed all 9 checks. That shows the single-copy cluster
is correct while healthy. It does not test what happens when a node is lost, it does not measure the space or
speed gained, and whether a cache tier should skip replication is the engine vendors' argument, not ours.

## Not done, and why

- **No engine has been run against Atlas.** The smoke test checks the storage behaviour an engine needs, not the engine.
- **No performance numbers.** The lab is a shared CPU host with no GPU or RDMA NIC. GPUDirect Storage has no code in Atlas
  ([STATUS.md](STATUS.md)).
- **Single-copy profile is opt-in and only smoke tested.** KV-cache blocks can be recomputed, so some vendors argue they do not need
  replication; that is their argument, not something Atlas has measured. Today a KV-cache filesystem is replicated
  like any other unless you opt into the single-copy profile above.
- **No vLLM connector.** Writing one over `KVAdapter` makes sense only if the file or S3 path proves insufficient.
