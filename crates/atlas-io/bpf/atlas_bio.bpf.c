// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// Block I/O latency, size and attribution. Observe-only: never changes or drops I/O.
//
//   tracepoint/block/block_rq_insert    remember (dev, sector) -> scheduler insert time
//   tracepoint/block/block_rq_issue     remember (dev, sector) -> issue time, size, op, task
//   tracepoint/block/block_rq_complete  join, emit one atlas_bio_event on the ring buffer and,
//                                       for Atlas Native processes, aggregate it
//                                       (atlas_native_io.bpf.c)
//
// Requests dispatched straight to the driver (no I/O scheduler, or a bypass) are never
// inserted, so their queue time stays 0.
//
// Built with clang -target bpf (crates/atlas-io/build.rs, `bpf` feature) and loaded by
// src/bpf.rs. The tracepoint records are declared with preserve_access_index, so field
// offsets are relocated against the running kernel's BTF (CO-RE); no vmlinux.h is needed.

#include <linux/types.h>
#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>

#include "atlas_native_io.bpf.c"

// Only the fields read here; names must match the kernel's trace_event_raw_* types.
struct trace_event_raw_block_rq {
    __u32 dev;
    __u64 sector;
    __u32 bytes;
    char rwbs[10];
} __attribute__((preserve_access_index));

struct trace_event_raw_block_rq_completion {
    __u32 dev;
    __u64 sector;
    int error;
} __attribute__((preserve_access_index));

// Must match RawEvent in src/bpf.rs.
struct atlas_bio_event {
    __u64 issued_ns;
    __u64 completed_ns;
    __u64 queued_ns;
    __u32 bytes;
    __u32 op;
    __u32 major;
    __u32 minor;
    __u32 pid;
    __u32 pad;
    __u64 cgroup_id;
    char comm[16];
};

struct atlas_bio_key {
    __u32 dev;
    __u32 pad;
    __u64 sector;
};

struct atlas_bio_inflight {
    __u64 issued_ns;
    __u64 inserted_ns;
    __u64 cgroup_id;
    __u32 pid;
    __u32 bytes;
    __u32 op;
    __u32 pad;
    char comm[16];
};

#define ATLAS_BIO_INFLIGHT_MAX 8192

// Linux REQ_OP_* values, as parsed by source::parse_op.
#define ATLAS_OP_READ 0
#define ATLAS_OP_WRITE 1
#define ATLAS_OP_FLUSH 2
#define ATLAS_OP_DISCARD 3
#define ATLAS_OP_OTHER 255

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, ATLAS_BIO_INFLIGHT_MAX);
    __type(key, struct atlas_bio_key);
    __type(value, struct atlas_bio_inflight);
} atlas_inflight SEC(".maps");

// LRU: an inserted request that is merged away or never issued must not pin an entry.
struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, ATLAS_BIO_INFLIGHT_MAX);
    __type(key, struct atlas_bio_key);
    __type(value, __u64);
} atlas_queued SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1 << 22);
} atlas_events SEC(".maps");

// [0] issues not tracked (inflight full), [1] completions not delivered (ring buffer full),
// [2] native completions not aggregated (stats map full), [3] native slow events not delivered.
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 4);
    __type(key, __u32);
    __type(value, __u64);
} atlas_dropped SEC(".maps");

// tgid -> 1 for the Atlas Native processes; written by userspace.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, ATLAS_NATIVE_PIDS_MAX);
    __type(key, __u32);
    __type(value, __u8);
} atlas_native_pids SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, ATLAS_NATIVE_IO_STATS_MAX);
    __type(key, struct atlas_native_io_key);
    __type(value, struct atlas_native_io_value);
} atlas_native_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1 << 18);
} atlas_native_slow SEC(".maps");

static __always_inline void count_drop(__u32 slot)
{
    __u64 *n = bpf_map_lookup_elem(&atlas_dropped, &slot);
    if (n)
        __sync_fetch_and_add(n, 1);
}

// rwbs is blktrace's op string: an optional 'F' (preflush), then the op letter — 'R', 'W', 'D'
// (discard), 'F' (flush) or 'N' — then flag letters. So the op letter is rwbs[0] or rwbs[1].
static __always_inline __u32 op_letter(char c)
{
    switch (c) {
    case 'R':
        return ATLAS_OP_READ;
    case 'W':
        return ATLAS_OP_WRITE;
    case 'D':
        return ATLAS_OP_DISCARD;
    case 'F':
        return ATLAS_OP_FLUSH;
    default:
        return ATLAS_OP_OTHER;
    }
}

SEC("tracepoint/block/block_rq_insert")
int atlas_rq_insert(struct trace_event_raw_block_rq *ctx)
{
    struct atlas_bio_key key = {};
    __u64 now = bpf_ktime_get_ns();

    key.dev = ctx->dev;
    key.sector = ctx->sector;
    bpf_map_update_elem(&atlas_queued, &key, &now, BPF_ANY);
    return 0;
}

SEC("tracepoint/block/block_rq_issue")
int atlas_rq_issue(struct trace_event_raw_block_rq *ctx)
{
    struct atlas_bio_key key = {};
    struct atlas_bio_inflight v = {};
    __u64 *inserted;
    // Read byte by byte: the verifier refuses loads through a ctx pointer moved by a memcpy.
    char c0 = ctx->rwbs[0];
    char c1 = ctx->rwbs[1];

    key.dev = ctx->dev;
    key.sector = ctx->sector;

    v.issued_ns = bpf_ktime_get_ns();
    inserted = bpf_map_lookup_elem(&atlas_queued, &key);
    if (inserted) {
        if (*inserted <= v.issued_ns)
            v.inserted_ns = *inserted;
        bpf_map_delete_elem(&atlas_queued, &key);
    }
    v.cgroup_id = bpf_get_current_cgroup_id();
    v.pid = bpf_get_current_pid_tgid() >> 32;
    v.bytes = ctx->bytes;
    v.op = op_letter(c0 == 'F' && c1 != 0 ? c1 : c0);
    bpf_get_current_comm(v.comm, sizeof(v.comm));

    if (bpf_map_update_elem(&atlas_inflight, &key, &v, BPF_ANY))
        count_drop(0);
    return 0;
}

static __always_inline void native_account(const struct atlas_bio_key *key,
                                           const struct atlas_bio_inflight *v, __u64 now,
                                           int error)
{
    struct atlas_native_io_key k = {};
    struct atlas_native_io_value zero = {};
    struct atlas_native_io_value *s;
    struct atlas_native_extent_event *e;
    __u32 pid = v->pid;
    __u64 lat = now - v->issued_ns;

    if (!bpf_map_lookup_elem(&atlas_native_pids, &pid))
        return;

    k.major = key->dev >> 20;
    k.minor = key->dev & ((1U << 20) - 1);
    k.op = v->op;
    k.cgroup_id = v->cgroup_id;
    s = bpf_map_lookup_elem(&atlas_native_stats, &k);
    if (!s) {
        bpf_map_update_elem(&atlas_native_stats, &k, &zero, BPF_NOEXIST);
        s = bpf_map_lookup_elem(&atlas_native_stats, &k);
    }
    if (s) {
        __sync_fetch_and_add(&s->ios, 1);
        __sync_fetch_and_add(&s->bytes, v->bytes);
        __sync_fetch_and_add(&s->latency_ns, lat);
        if (error)
            __sync_fetch_and_add(&s->errors, 1);
        // A racing completion can lose a max update; it is a hint, not an accounting total.
        if (lat > s->max_latency_ns)
            s->max_latency_ns = lat;
    } else {
        count_drop(2);
    }

    if (lat < ATLAS_NATIVE_SLOW_IO_NS)
        return;
    e = bpf_ringbuf_reserve(&atlas_native_slow, sizeof(*e), 0);
    if (!e) {
        count_drop(3);
        return;
    }
    e->ts_ns = now;
    e->cgroup_id = v->cgroup_id;
    e->sector = key->sector;
    e->latency_ns = lat;
    e->bytes = v->bytes;
    e->major = k.major;
    e->minor = k.minor;
    e->op = v->op;
    e->pid = pid;
    e->error = error;
    __builtin_memcpy(e->comm, v->comm, sizeof(e->comm));
    bpf_ringbuf_submit(e, 0);
}

SEC("tracepoint/block/block_rq_complete")
int atlas_rq_complete(struct trace_event_raw_block_rq_completion *ctx)
{
    struct atlas_bio_key key = {};
    struct atlas_bio_inflight *v;
    struct atlas_bio_event *e;
    __u64 now = bpf_ktime_get_ns();
    int error = ctx->error;

    key.dev = ctx->dev;
    key.sector = ctx->sector;
    v = bpf_map_lookup_elem(&atlas_inflight, &key);
    if (!v)
        return 0;

    native_account(&key, v, now, error);

    e = bpf_ringbuf_reserve(&atlas_events, sizeof(*e), 0);
    if (!e) {
        count_drop(1);
        bpf_map_delete_elem(&atlas_inflight, &key);
        return 0;
    }
    e->issued_ns = v->issued_ns;
    e->completed_ns = now;
    e->queued_ns = v->inserted_ns ? v->issued_ns - v->inserted_ns : 0;
    e->bytes = v->bytes;
    e->op = v->op;
    // dev_t inside the kernel is MAJOR << 20 | MINOR.
    e->major = key.dev >> 20;
    e->minor = key.dev & ((1U << 20) - 1);
    e->pid = v->pid;
    e->pad = 0;
    e->cgroup_id = v->cgroup_id;
    __builtin_memcpy(e->comm, v->comm, sizeof(e->comm));
    bpf_ringbuf_submit(e, 0);
    bpf_map_delete_elem(&atlas_inflight, &key);
    return 0;
}

// NFS, ZFS and io_uring counters, plus the cgroup→volume map. Each attach is optional
// (src/bpf.rs); a host without the tracepoint keeps that family in programs_missing.
#include "atlas_cgroup.bpf.c"
#include "atlas_nfs.bpf.c"
#include "atlas_zfs.bpf.c"
#include "atlas_uring.bpf.c"

// Only non-GPL helpers are used, so the program loads under the project's own license.
char LICENSE[] SEC("license") = "Apache-2.0";
