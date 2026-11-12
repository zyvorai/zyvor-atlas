// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//
// cgroup id → Atlas volume id. Userspace (the agent's PUT /io/cgroup) fills this map.
// Included by atlas_bio.bpf.c; it is a map only, not a tracepoint program. The value is
// 64 bytes so a volume id of at most 63 bytes plus a NUL fits (cgroup::VOLUME_ID_MAX).

struct atlas_cgroup_key {
    __u64 cgroup_id;
};

struct atlas_cgroup_val {
    char volume_id[64];
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct atlas_cgroup_key);
    __type(value, struct atlas_cgroup_val);
} atlas_cgroup SEC(".maps");
