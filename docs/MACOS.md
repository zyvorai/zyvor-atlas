<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# macOS storage

Atlas can manage the disks of a Mac, internal and external, through the `atlas-driver-macos` backend.
This first slice is **read-only discovery**: capacity, usage, mounts and drive health appear in the
inventory, the console and the MCP tools like any other backend. Volume lifecycle (erase, add/delete
volumes, snapshots) and a Mac agent for fleets come in later slices (see the roadmap below).

## What is reported

| macOS | Atlas |
|---|---|
| A Mac | one cluster, `macos://<host>` |
| APFS container (`disk3`) | pool, `kind = apfs_container`; `max_bytes` = container capacity, `used_bytes` = capacity minus free |
| APFS volume (`Macintosh HD`, `Data`, an external `Backup`) | filesystem volume; size is its quota if one is set, else the container capacity; `used_bytes` = bytes in use; mount point in `storage_class_name` |
| Partition with another filesystem (exFAT stick, HFS+, FAT) | pool of `kind = partition` with one volume (usage is not reported by `diskutil list`) |
| Disk health | SMART status of the disk behind the pool: `Failing` makes the pool and cluster **critical**; `Not Supported` (most USB bridges) is not penalised and not counted |

Device class comes from `diskutil info`: `nvme` (Apple fabric/NVMe), `ssd` or `hdd`. Ids use the volume or
container UUID, so a drive keeps its Atlas id when it is replugged and macOS renumbers it.
Mounted disk images (`hdiutil attach`) are not devices and are left out unless
`ATLAS_MACOS_ALLOW_DISK_IMAGES=1` (meant for tests on throwaway images).

## Enabling it

The real driver runs `diskutil` **locally**, so the gateway has to run on the Mac (a Linux container
cannot see a Mac's disks):

```bash
ATLAS_MACOS_ENABLE=1 ATLAS_MACOS_DRIVER_MODE=real ATLAS_MACOS_HOST=$(scutil --get LocalHostName) \
  ./atlas-gateway
```

| Variable | Default | Meaning |
|---|---|---|
| `ATLAS_MACOS_ENABLE` | off | Register the `bkd_macos` backend. |
| `ATLAS_MACOS_DRIVER_MODE` | `fake` | `fake` maps recorded plists (an internal disk, a USB SSD, an exFAT card, a disk image), no commands run. `real` runs `diskutil`. |
| `ATLAS_MACOS_HOST` | `localhost` (`mac01.zyvor.lab` in fake mode) | Display name. |
| `ATLAS_MACOS_ALLOW_DISK_IMAGES` | off | Also report mounted disk images. |

The inventory is refreshed at start and on the monitor interval, so plugging in or ejecting a drive shows
up on the next scan. If `diskutil` is missing the backend reports unreachable; the driver never
fabricates disks. `POST /backends` cannot create a macOS backend; use the variables above.

## Safety

Everything here only reads. Commands are fixed argument arrays (`diskutil list|apfs list|info -plist`),
never shell strings, with a 30 s timeout. The write slice will add hard refusals that nothing can
override: internal disks, the boot container and system volumes, OS-owned snapshots, mounted volumes in use.

## Roadmap

1. Read-only discovery (this document).
2. Volume lifecycle with hard refusals: erase an external disk, add/delete APFS volumes, resize a
   container, eject, local snapshots; admin only, typed confirmation, jobs, audit.
3. A Mac agent and a remote driver, so a central gateway manages many Macs.
4. Macs as clients of Atlas storage (NFS/SMB mounts, and the native filesystem if a macFUSE build proves out).
