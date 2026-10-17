<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Using Atlas storage from a Mac

A Mac can use Atlas storage in three ways. Managing a Mac's own disks is a different topic
([`MACOS.md`](MACOS.md)).

| Way | Use it for | Status |
|---|---|---|
| NFS to the [NFS gateway](NATIVE_NFS.md) | Shared files, scripts, build trees | Supported path |
| SMB to the [SMB gateway](NATIVE_SMB.md) | Finder access, mixed Mac/Windows teams | Supported path |
| `atlasctl` | Inventory, volumes, snapshots over the REST API | Released for Apple Silicon and Intel |
| `atlas-native-mount` (FUSE) | Direct native filesystem mount | **Not available on macOS** (see below) |

The commands below use the client tools that ship with macOS. They follow Apple's documented options and
the gateways' documented behaviour, but they have **not been run against a live gateway from a Mac**: no
native NFS or SMB gateway is deployed on the lab, and the gateways' ports (2049, 445) should not be exposed
to the internet for a test. Treat this page as the intended procedure and report anything that differs.

## NFS

The gateway serves NFSv4.1/4.2 and optionally v3 (Ganesha). macOS mounts NFSv4 and v3.

```bash
sudo mkdir -p /Volumes/team-share            # or any directory you own
sudo mount -t nfs -o vers=4,resvport <service-ip>:/team-share /Volumes/team-share
# NFSv3, if the gateway enables it:
sudo mount -t nfs -o vers=3,mountproto=tcp,resvport <service-ip>:/team-share /Volumes/team-share
umount /Volumes/team-share
```

Finder: Go > Connect to Server, `nfs://<service-ip>/team-share`. `resvport` uses a privileged source port,
which is the macOS default and what many servers expect. The export's `clients` list decides who may mount;
file ownership follows numeric uids, so a Mac user whose uid (usually 501) differs from the owner will see
mode-bit denials. Locks follow the gateway's rules in [`NATIVE_NFS.md`](NATIVE_NFS.md).

## SMB

The gateway runs Samba with Unix accounts (create the user and password through the Helm values).

```bash
mkdir -p ~/atlas-team
mount_smbfs //alice@<service-ip>/team ~/atlas-team      # prompts for the password
umount ~/atlas-team
```

Finder: Go > Connect to Server, `smb://<service-ip>/team`. Do not put the password on the command line, it
ends up in shell history. Save it in Keychain from the Finder prompt instead. Notes:

- Finder writes `.DS_Store` and `._*` files. The gateway does not enable Apple's SMB extensions
  (`vfs_fruit`), so they appear as ordinary files. Time Machine and Spotlight on the share are not supported.
- SMB 3.1.1 is negotiated; macOS signs by default, which costs some throughput.

## atlasctl

Release archives for `aarch64-apple-darwin` and `x86_64-apple-darwin` carry `atlasctl`. Point it at a gateway
with `--base-url` (see the README) to list pools and volumes, create snapshots and read alerts.

## Native FUSE mount on macOS: not now

`atlas-native-mount` is Linux-only, and making it run on macOS needs a FUSE implementation that is not
present on a stock Mac (macFUSE ships a kernel extension that needs approval in Security settings; FUSE-T
works over a local NFS server). A first reading of the code shows the work:

- `fuser` is only a dependency on Linux, and the mount binary is gated on `target_os = "linux"`
  (`crates/atlas-native-fuse/Cargo.toml`, `src/lib.rs`, `src/bin/atlas-native-mount.rs`);
- unmounting shells out to `fusermount3 -u` (`src/supervise.rs`), which macOS lacks (`umount` or
  `diskutil unmount` would replace it);
- `src/fuse.rs` relies on kernel capabilities that macFUSE reports differently (POSIX/flock locks, ACLs,
  export support) and on `O_DIRECT` and `XATTR_*` semantics that differ on macOS.

The pjdfstest suites and the `git clone` check that qualify the Linux mount would have to pass on macOS
before it could ship. That has **not been attempted**: neither macFUSE nor FUSE-T is installed here, and
installing a kernel extension is a system change outside what an automated session should do. Decision:
**no-go until someone with a Mac and a FUSE package runs that spike.** NFS and SMB above are the supported way.
