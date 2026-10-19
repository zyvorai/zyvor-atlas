// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The CSI services over a real Unix socket, against a live in-process cluster (one metadata
//! node, one data node on localhost).
#![cfg_attr(not(target_os = "linux"), allow(unused_imports))]

use std::{collections::BTreeMap, net::TcpListener, path::PathBuf, time::Duration};

use atlas_native::{
    node::{DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig},
    NodeType, ROOT_INO,
};
use atlas_native_csi::{
    controller::ControllerService,
    identity::IdentityService,
    node::{NodeConfig as CsiNodeConfig, NodeService},
    proto::{
        controller_client::ControllerClient, identity_client::IdentityClient,
        node_client::NodeClient, volume_capability, volume_content_source, CapacityRange,
        ControllerExpandVolumeRequest, CreateSnapshotRequest, CreateVolumeRequest,
        DeleteSnapshotRequest, DeleteVolumeRequest, GetPluginCapabilitiesRequest,
        GetPluginInfoRequest, NodeGetCapabilitiesRequest, NodeGetInfoRequest,
        NodeGetVolumeStatsRequest, NodePublishVolumeRequest, ValidateVolumeCapabilitiesRequest,
        VolumeCapability, VolumeContentSource,
    },
    serve, DRIVER_NAME,
};
use atlas_native_fuse::{
    client::{Body, Client, ClientConfig, Retry},
    ops::{Ops, OpsConfig},
};
use hyper_util::rt::TokioIo;
use reqwest::Method;
use tonic::{
    transport::{Channel, Endpoint, Uri},
    Code,
};

const TOKEN: &str = "csi-test-token";

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

struct Cluster {
    td: tempfile::TempDir,
    meta: NativeNode,
    _data: NativeNode,
}

impl Cluster {
    fn start() -> Self {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), TOKEN).unwrap();
        let (data_l, meta_l) = (bind(), bind());
        let base = |id: &str| NodeConfig {
            node_id: id.into(),
            data_dir: td.path().join(id),
            http_listen: "127.0.0.1:0".parse().unwrap(),
            api_token_file: Some(td.path().join("token")),
            tls: None,
            http_tls: None,
            data_node: None,
            metadata: None,
            max_request_bytes: 1 << 20,
        };
        let mut cfg = base("d1");
        cfg.data_node = Some(DataNodeRole {
            listen: data_l.local_addr().unwrap(),
            devices: Vec::new(),
        });
        let spec = DataNodeSpec {
            id: "d1".into(),
            addr: data_l.local_addr().unwrap().to_string(),
            zone: None,
            rack: None,
            host: None,
            free_bytes: 1 << 30,
            devices: 1,
        };
        let data = NativeNode::start_with(
            cfg,
            Listeners {
                http: Some(bind()),
                data_node: Some(data_l),
                metadata: None,
            },
        )
        .unwrap();
        let mut cfg = base("m1");
        cfg.metadata = Some(MetadataRole {
            listen: meta_l.local_addr().unwrap(),
            peers: BTreeMap::from([("m1".into(), meta_l.local_addr().unwrap().to_string())]),
            bootstrap: None,
            data_nodes: vec![spec],
            replicas: 1,
            erasure: None,
            erasure_min_bytes: 64 << 10,
            compression: false,
            dedup: false,
            rebuild_delay_secs: 60,
            rebuild_bytes_per_sec: 0,
            scrub_bytes_per_sec: 0,
            tiering: None,
            extent_bytes: 64 << 10,
            tick_ms: 10,
            proposal_timeout_ms: 3000,
            repair_interval_secs: 0,
            gc_interval_secs: 0,
            groups: 1,
            cache_inodes: atlas_native::store::DEFAULT_CACHE_INODES,
            store_cache_bytes: atlas_native::store::DEFAULT_STORE_CACHE_BYTES,
        });
        let meta = NativeNode::start_with(
            cfg,
            Listeners {
                http: Some(bind()),
                data_node: None,
                metadata: Some(meta_l),
            },
        )
        .unwrap();
        Self {
            td,
            meta,
            _data: data,
        }
    }

    fn client(&self) -> Client {
        let mut cfg = ClientConfig::new(vec![format!("http://{}", self.meta.http_addr())]);
        cfg.token = Some(TOKEN.into());
        cfg.retry_for = Duration::from_secs(20);
        Client::new(cfg).unwrap()
    }

    /// The filesystem's byte quota, if it has one.
    fn max_bytes(&self, fs: &str) -> Option<u64> {
        self.client()
            .json(
                Method::GET,
                &format!("/v1/fs/{fs}/quota"),
                Body::Empty,
                Retry::Idempotent,
            )
            .unwrap()["max_bytes"]
            .as_u64()
    }

    fn ops(&self, fs: &str) -> Ops {
        Ops::new(
            self.client(),
            fs,
            OpsConfig {
                max_io_bytes: 1 << 20,
                ..OpsConfig::default()
            },
        )
    }
}

async fn channel(path: PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                Ok::<_, std::io::Error>(TokioIo::new(tokio::net::UnixStream::connect(path).await?))
            }
        }))
        .await
        .unwrap()
}

fn mount_cap() -> VolumeCapability {
    VolumeCapability {
        access_type: Some(volume_capability::AccessType::Mount(
            volume_capability::MountVolume::default(),
        )),
        access_mode: Some(volume_capability::AccessMode {
            mode: volume_capability::access_mode::Mode::MultiNodeMultiWriter as i32,
        }),
    }
}

fn create(name: &str, source: Option<volume_content_source::Type>) -> CreateVolumeRequest {
    CreateVolumeRequest {
        name: name.into(),
        capacity_range: Some(CapacityRange {
            required_bytes: 1 << 30,
            limit_bytes: 0,
        }),
        volume_capabilities: vec![mount_cap()],
        parameters: [("extentBytes".to_string(), "65536".to_string())].into(),
        secrets: Default::default(),
        volume_content_source: source.map(|t| VolumeContentSource { r#type: Some(t) }),
    }
}

fn expand(volume_id: &str, required_bytes: i64) -> ControllerExpandVolumeRequest {
    ControllerExpandVolumeRequest {
        volume_id: volume_id.into(),
        capacity_range: Some(CapacityRange {
            required_bytes,
            limit_bytes: 0,
        }),
        secrets: Default::default(),
        volume_capability: Some(mount_cap()),
    }
}

fn write_file(ops: &Ops, name: &str, data: &[u8]) {
    let f = ops
        .mknode(ROOT_INO, name, NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(f.ino, 0, data).unwrap();
    ops.flush(f.ino).unwrap();
}

fn read_file(ops: &Ops, name: &str) -> Vec<u8> {
    let f = ops.lookup(ROOT_INO, name).unwrap();
    ops.read(f.ino, 0, 1 << 20).unwrap()
}

#[test]
fn controller_lifecycle_over_the_socket() {
    let c = Cluster::start();
    let sock = c.td.path().join("csi/csi.sock");
    let controller = ControllerService::new(c.client());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = rt.spawn({
        let sock = sock.clone();
        async move {
            serve(
                &sock,
                IdentityService { controller: true },
                Some(controller),
                None,
                async {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap()
        }
    });
    while !sock.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ch = rt.block_on(channel(sock));
    let mut id = IdentityClient::new(ch.clone());
    let mut ctl = ControllerClient::new(ch.clone());
    let mut node = NodeClient::new(ch);

    let info = rt
        .block_on(id.get_plugin_info(GetPluginInfoRequest {}))
        .unwrap();
    assert_eq!(info.get_ref().name, DRIVER_NAME);
    let caps = rt
        .block_on(id.get_plugin_capabilities(GetPluginCapabilitiesRequest {}))
        .unwrap();
    assert_eq!(caps.get_ref().capabilities.len(), 2);

    // Create is idempotent: a retry returns the same volume.
    let pvc = "pvc-11111111-2222-3333-4444-555555555555";
    let v = rt.block_on(ctl.create_volume(create(pvc, None))).unwrap();
    let v = v.into_inner().volume.unwrap();
    assert_eq!((v.volume_id.as_str(), v.capacity_bytes), (pvc, 1 << 30));
    let again = rt.block_on(ctl.create_volume(create(pvc, None))).unwrap();
    assert_eq!(again.into_inner().volume.unwrap().volume_id, pvc);
    // The PVC's size is the filesystem's byte quota; expansion raises it and never lowers it.
    assert_eq!(c.max_bytes(pvc), Some(1 << 30));
    let grown = rt.block_on(ctl.controller_expand_volume(expand(pvc, 2 << 30)));
    let grown = grown.unwrap().into_inner();
    assert_eq!(
        (grown.capacity_bytes, grown.node_expansion_required),
        (2 << 30, false)
    );
    assert_eq!(c.max_bytes(pvc), Some(2 << 30));
    let same = rt.block_on(ctl.controller_expand_volume(expand(pvc, 1 << 30)));
    assert_eq!(same.unwrap().into_inner().capacity_bytes, 2 << 30);
    assert_eq!(c.max_bytes(pvc), Some(2 << 30));
    let err = rt
        .block_on(ctl.controller_expand_volume(expand("pvc-missing", 1 << 30)))
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    // enforceCapacity: "false" leaves the filesystem thin, and expansion keeps it so.
    let mut thin = create("pvc-thin", None);
    thin.parameters
        .insert("enforceCapacity".into(), "false".into());
    rt.block_on(ctl.create_volume(thin)).unwrap();
    assert_eq!(c.max_bytes("pvc-thin"), None);
    let t = rt.block_on(ctl.controller_expand_volume(expand("pvc-thin", 3 << 30)));
    assert_eq!(t.unwrap().into_inner().capacity_bytes, 3 << 30);
    assert_eq!(c.max_bytes("pvc-thin"), None);

    // A small volume refuses writes past its size until it is expanded.
    let mut small = create("pvc-small", None);
    small.capacity_range = Some(CapacityRange {
        required_bytes: 65536,
        limit_bytes: 0,
    });
    rt.block_on(ctl.create_volume(small)).unwrap();
    let ops = c.ops("pvc-small");
    let f = ops
        .mknode(ROOT_INO, "big", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(f.ino, 0, &[1u8; 65536]).unwrap();
    ops.flush(f.ino).unwrap();
    let over = ops
        .write(f.ino, 65536, &[2u8; 4096])
        .and_then(|_| ops.flush(f.ino));
    assert_eq!(over, Err(libc::EDQUOT));
    rt.block_on(ctl.controller_expand_volume(expand("pvc-small", 1 << 20)))
        .unwrap();
    ops.write(f.ino, 65536, &[2u8; 4096]).unwrap();
    ops.flush(f.ino).unwrap();
    // A name that isn't a valid id is hashed, consistently.
    let odd = rt
        .block_on(ctl.create_volume(create("Odd Name/with_chars", None)))
        .unwrap()
        .into_inner()
        .volume
        .unwrap()
        .volume_id;
    assert!(odd.starts_with("csi-"));

    let block = CreateVolumeRequest {
        volume_capabilities: vec![VolumeCapability {
            access_type: Some(volume_capability::AccessType::Block(Default::default())),
            access_mode: None,
        }],
        ..create("pvc-block", None)
    };
    let err = rt.block_on(ctl.create_volume(block)).unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);

    write_file(&c.ops(pvc), "model.bin", b"weights-v1");

    let snap_req = CreateSnapshotRequest {
        source_volume_id: pvc.into(),
        name: "snapshot-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        secrets: Default::default(),
        parameters: Default::default(),
    };
    let s1 = rt
        .block_on(ctl.create_snapshot(snap_req.clone()))
        .unwrap()
        .into_inner()
        .snapshot
        .unwrap();
    assert!(s1.ready_to_use && s1.creation_time.is_some());
    let s2 = rt
        .block_on(ctl.create_snapshot(snap_req.clone()))
        .unwrap()
        .into_inner()
        .snapshot
        .unwrap();
    assert_eq!(
        (&s1.snapshot_id, &s1.creation_time),
        (&s2.snapshot_id, &s2.creation_time)
    );
    // The same snapshot name on another volume is a conflict.
    let other = CreateSnapshotRequest {
        source_volume_id: odd.clone(),
        ..snap_req
    };
    let err = rt.block_on(ctl.create_snapshot(other)).unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);

    // Later writes to the source don't reach the snapshot's clone.
    write_file(&c.ops(pvc), "late.bin", b"after");
    let from_snap = rt
        .block_on(ctl.create_volume(create(
            "pvc-from-snap",
            Some(volume_content_source::Type::Snapshot(
                volume_content_source::SnapshotSource {
                    snapshot_id: s1.snapshot_id.clone(),
                },
            )),
        )))
        .unwrap();
    assert!(from_snap
        .get_ref()
        .volume
        .as_ref()
        .unwrap()
        .content_source
        .is_some());
    // The clone carries its own size, not the source's quota at snapshot time.
    assert_eq!(c.max_bytes("pvc-from-snap"), Some(1 << 30));
    let clone = c.ops("pvc-from-snap");
    assert_eq!(read_file(&clone, "model.bin"), b"weights-v1");
    assert_eq!(
        clone.lookup(ROOT_INO, "late.bin").unwrap_err(),
        libc::ENOENT
    );

    // A volume clone sees the source as it is now.
    rt.block_on(ctl.create_volume(create(
        "pvc-from-vol",
        Some(volume_content_source::Type::Volume(
            volume_content_source::VolumeSource {
                volume_id: pvc.into(),
            },
        )),
    )))
    .unwrap();
    assert_eq!(read_file(&c.ops("pvc-from-vol"), "late.bin"), b"after");

    let missing = rt
        .block_on(ctl.create_volume(create(
            "pvc-bad-src",
            Some(volume_content_source::Type::Snapshot(
                volume_content_source::SnapshotSource {
                    snapshot_id: "no-such-snapshot".into(),
                },
            )),
        )))
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);

    let validate = |vid: &str| ValidateVolumeCapabilitiesRequest {
        volume_id: vid.into(),
        volume_context: Default::default(),
        volume_capabilities: vec![mount_cap()],
        parameters: Default::default(),
        secrets: Default::default(),
    };
    let ok = rt
        .block_on(ctl.validate_volume_capabilities(validate(pvc)))
        .unwrap();
    assert!(ok.get_ref().confirmed.is_some());

    // Deletes are idempotent; the clones outlive their sources.
    for _ in 0..2 {
        rt.block_on(ctl.delete_snapshot(DeleteSnapshotRequest {
            snapshot_id: s1.snapshot_id.clone(),
            secrets: Default::default(),
        }))
        .unwrap();
        rt.block_on(ctl.delete_volume(DeleteVolumeRequest {
            volume_id: pvc.into(),
            secrets: Default::default(),
        }))
        .unwrap();
    }
    let err = rt
        .block_on(ctl.validate_volume_capabilities(validate(pvc)))
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(read_file(&clone, "model.bin"), b"weights-v1");

    // This process serves no node service.
    let err = rt
        .block_on(node.node_get_info(NodeGetInfoRequest {}))
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);

    stop_tx.send(()).unwrap();
    rt.block_on(server).unwrap();
}

// Mount detection reads /proc/self/mountinfo, so this is a Linux test: the node plugin only runs
// there (it mounts through FUSE).
#[cfg(target_os = "linux")]
#[test]
fn node_service_validates_before_mounting() {
    let td = tempfile::tempdir().unwrap();
    let sock = td.path().join("node.sock");
    let node = NodeService::new(CsiNodeConfig {
        node_id: "worker-1".into(),
        mount_binary: "/nonexistent/atlas-native-mount".into(),
        mount_args: Vec::new(),
        mount_timeout: Duration::from_secs(1),
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.spawn({
        let sock = sock.clone();
        async move {
            serve(
                &sock,
                IdentityService { controller: false },
                None,
                Some(node),
                std::future::pending(),
            )
            .await
            .unwrap()
        }
    });
    while !sock.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ch = rt.block_on(channel(sock));
    let mut id = IdentityClient::new(ch.clone());
    let mut node = NodeClient::new(ch);
    let caps = rt
        .block_on(id.get_plugin_capabilities(GetPluginCapabilitiesRequest {}))
        .unwrap();
    assert!(caps.get_ref().capabilities.is_empty());
    let info = rt
        .block_on(node.node_get_info(NodeGetInfoRequest {}))
        .unwrap();
    assert_eq!(info.get_ref().node_id, "worker-1");
    let ncaps = rt
        .block_on(node.node_get_capabilities(NodeGetCapabilitiesRequest {}))
        .unwrap();
    assert_eq!(ncaps.get_ref().capabilities.len(), 2);
    let stats = |path: &std::path::Path| NodeGetVolumeStatsRequest {
        volume_id: "pvc-1".into(),
        volume_path: path.to_string_lossy().into_owned(),
        staging_target_path: String::new(),
    };
    // A directory that isn't a mount is not a published volume.
    let err = rt
        .block_on(node.node_get_volume_stats(stats(td.path())))
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);

    let target = td.path().join("target").to_string_lossy().into_owned();
    let publish = |cap: VolumeCapability| NodePublishVolumeRequest {
        volume_id: "pvc-1".into(),
        publish_context: Default::default(),
        staging_target_path: String::new(),
        target_path: target.clone(),
        volume_capability: Some(cap),
        readonly: false,
        secrets: Default::default(),
        volume_context: Default::default(),
    };
    let block = VolumeCapability {
        access_type: Some(volume_capability::AccessType::Block(Default::default())),
        access_mode: None,
    };
    let err = rt
        .block_on(node.node_publish_volume(publish(block)))
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let mut cap = mount_cap();
    if let Some(volume_capability::AccessType::Mount(m)) = &mut cap.access_type {
        m.mount_flags = vec!["endpoint=http://elsewhere".into()];
    }
    let err = rt
        .block_on(node.node_publish_volume(publish(cap)))
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}
