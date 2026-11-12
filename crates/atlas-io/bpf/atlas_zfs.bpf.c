// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// OpenZFS probes (tracepoint category `zfs`, names `zfs_read` / `zfs_write`). Many kernels
// ship without those tracepoints; attach then fails open and atlas_zfs stays in
// programs_missing. Observe-only counters, same shape as the NFS probes.
//
// Included by atlas_bio.bpf.c.

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, __u64);
} atlas_zfs_counts SEC(".maps");

SEC("tracepoint/zfs/zfs_read")
int atlas_zfs_read(void *ctx)
{
    __u32 slot = 0;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_zfs_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}

SEC("tracepoint/zfs/zfs_write")
int atlas_zfs_write(void *ctx)
{
    __u32 slot = 1;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_zfs_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}
