// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// io_uring submit and completion probes. Observe-only counters; they do not read the
// tracepoint record. Attach is best-effort. Kernels before the io_uring tracepoints
// leave atlas_uring in programs_missing.
//
// Included by atlas_bio.bpf.c.

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, __u64);
} atlas_uring_counts SEC(".maps");

SEC("tracepoint/io_uring/io_uring_submit_req")
int atlas_uring_submit(void *ctx)
{
    __u32 slot = 0;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_uring_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}

SEC("tracepoint/io_uring/io_uring_complete")
int atlas_uring_complete(void *ctx)
{
    __u32 slot = 1;
    __u64 *n;
    (void)ctx;
    n = bpf_map_lookup_elem(&atlas_uring_counts, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
    return 0;
}
