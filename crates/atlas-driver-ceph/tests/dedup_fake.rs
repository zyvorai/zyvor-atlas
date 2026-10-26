// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use atlas_api_types::DedupEstimateOptions;
use atlas_driver_ceph::FakeCephDriver;
use atlas_driver_core::StorageDriver;

#[tokio::test]
async fn fake_estimate_finds_the_shared_base_layer() {
    let d = FakeCephDriver::new("bkd_fake");
    let e = d
        .estimate_dedup("rbd-nvme-prod", DedupEstimateOptions::default())
        .await
        .unwrap();
    // 8 images x (6 shared + 2 unique) blocks = 64 scanned; 6 + 16 = 22 distinct.
    assert!(
        (e.dedup_ratio - 64.0 / 22.0).abs() < 1e-9,
        "{}",
        e.dedup_ratio
    );
    assert_eq!(e.method, "block_hash_scan");
    assert_eq!(e.coverage, 1.0);
    assert!(!e.lower_bound);
    assert!(e.zero_bytes.unwrap() > 0);
}

#[tokio::test]
async fn estimates_only_rbd_pools() {
    let d = FakeCephDriver::new("bkd_fake");
    let err = d
        .estimate_dedup("cephfs-data0", DedupEstimateOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");
    assert!(d
        .estimate_dedup("no-such-pool", DedupEstimateOptions::default())
        .await
        .is_err());
}
