// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// NFS client probes. Observe-only counters: they do not read the tracepoint record, so a
// kernel whose nfs tracepoint layout differs still passes the verifier. Attach is best-effort
// (src/bpf.rs); a host with no NFS client leaves atlas_nfs in programs_missing.
//
// Included by atlas_bio.bpf.c.

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, __u64);
} atlas_nfs_counts SEC(".maps");

SEC("tracepoint/nfs/nfs_initiate_read")
int atlas_nfs_read(void *ctx)
{
    __u32 slot = 0;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_nfs_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}

SEC("tracepoint/nfs/nfs_initiate_write")
int atlas_nfs_write(void *ctx)
{
    __u32 slot = 1;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_nfs_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}
