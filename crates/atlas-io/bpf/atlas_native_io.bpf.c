// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//
// Atlas Native eBPF ABI, included by atlas_bio.bpf.c (one object, one issue/complete join).
//
// Userspace (src/bpf.rs) keeps atlas_native_pids filled with the tgids of the Atlas Native
// processes. On completion of a request issued by one of them the kernel aggregates it into
// atlas_native_stats and, at or above ATLAS_NATIVE_SLOW_IO_NS, pushes an extent event onto
// atlas_native_slow. The loader pins all three under /sys/fs/bpf/atlas/native.
//
// Bounded cardinality is mandatory: a full stats map counts a drop instead of growing.

#ifndef ATLAS_NATIVE_IO_BPF_H
#define ATLAS_NATIVE_IO_BPF_H

// Must match NativeKey in src/bpf.rs.
struct atlas_native_io_key {
    __u32 major;
    __u32 minor;
    __u32 op;
    __u32 pad;
    __u64 cgroup_id;
};

// Must match NativeValue in src/bpf.rs.
struct atlas_native_io_value {
    __u64 ios;
    __u64 bytes;
    __u64 latency_ns;
    __u64 max_latency_ns;
    __u64 errors;
};

// Must match NativeSlowEvent in src/bpf.rs.
struct atlas_native_extent_event {
    __u64 ts_ns;
    __u64 cgroup_id;
    __u64 sector;
    __u64 latency_ns;
    __u32 bytes;
    __u32 major;
    __u32 minor;
    __u32 op;
    __u32 pid;
    __s32 error;
    char comm[16];
};

#define ATLAS_NATIVE_IO_STATS_MAX 4096
#define ATLAS_NATIVE_PIDS_MAX 1024
#define ATLAS_NATIVE_SLOW_IO_NS 5000000ULL

#endif
