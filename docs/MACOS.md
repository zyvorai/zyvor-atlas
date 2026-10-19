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

## Managing external disks

Admin only, local to the Mac the gateway runs on, each operation a job with an audit entry
(`macos.*.requested`). Erase and delete need the target typed back.

| Route | Does |
|---|---|
| `GET /macos/devices` | Whole disks; `erase_refusal` says why Atlas would not erase one. |
| `POST /macos/disks/erase` `{device, confirm_device, name}` | `diskutil eraseDisk APFS <name> GPT <device>` |
| `POST /macos/containers/{container}/volumes` `{name, quota_bytes?, reserve_bytes?}` | `diskutil apfs addVolume` |
| `POST /macos/containers/{container}/resize` `{size_bytes, confirm_device}` | `diskutil apfs resizeContainer` (grow or shrink; never below what the container holds, at least 64 MiB) |
| `POST /macos/volumes/{device}/delete` `{confirm_device}` | `diskutil apfs deleteVolume` |
| `POST /macos/disks/{device}/eject` | `diskutil eject` |

Volume names are 1-32 characters of letters, digits, space, `_`, `.`, `-`, starting with a letter or digit.
After a job, discovery runs again so the inventory matches the Mac.

## Safety

Commands are fixed argument arrays, never shell strings, never `-force`, with a 30 s timeout. Before
anything runs, the route and then the job (which may run later) check the live layout against refusals
that no request field can override:

- internal disks (`Internal=true`);
- any disk holding the running system (a volume mounted at `/`, `/System/...` or `/private/...`);
- any disk with a volume mounted outside `/Volumes/` (erase);
- APFS volumes with a system role (System, Data, Preboot, Recovery, VM, Update, xART, Hardware, ISC, Backup)
  and volumes mounted outside `/Volumes/`;
- mounted disk images, unless `ATLAS_MACOS_ALLOW_DISK_IMAGES=1` (for throwaway test images);
- identifiers that are not exactly `diskN` / `diskNsM`.

In fake mode the same checks run against the recorded fixtures and nothing is executed. The Disks page in the console has a
Mac section (erase, add volume, resize, eject; the target is typed back for destructive actions). Local snapshots
are not offered: `diskutil apfs` can list and delete snapshots but not create one for a volume, and the only creator
(`tmutil localsnapshot`) snapshots the whole system.

## A fleet of Macs

Each Mac runs `atlas-mac-agent`, a small read-only HTTP service over the same driver; a central gateway
(on any OS) polls them. Release archives for macOS (`atlas-mac-agent-<tag>-<target>`) come with each release.

On a Mac (as root; the token is printed once and kept in `/usr/local/etc/atlas-mac-agent/token`):

```bash
sudo scripts/install-mac-agent.sh ./atlas-mac-agent 0.0.0.0:5112 /path/cert.pem /path/key.pem
```

The agent refuses to listen on a non-loopback address without a token and TLS. Settings:
`ATLAS_MAC_BIND` (default `127.0.0.1:5112`), `ATLAS_MAC_MODE` (`fake` default, `real`), `ATLAS_MAC_HOST`,
`ATLAS_MAC_TOKEN` or `ATLAS_MAC_TOKEN_FILE`, `ATLAS_MAC_TLS_CERT` / `ATLAS_MAC_TLS_KEY`,
`ATLAS_MAC_ALLOW_DISK_IMAGES`. Endpoints: `GET /health` (open), `GET /v1/discover`, `GET /v1/metrics`
(bearer token).

On the gateway, list the agents and restart:

```bash
ATLAS_MACOS_AGENTS=studio=https://studio.lan:5112,laptop=https://laptop.lan:5112
ATLAS_MACOS_AGENT_TOKEN_FILE=/etc/atlas/mac-token      # sent to every agent; or ATLAS_MACOS_AGENT_TOKEN
ATLAS_MACOS_AGENT_CA_CERT=/etc/atlas/mac-ca.pem        # optional extra trusted root
```

Each entry becomes a backend `bkd_macos_<name>` (dashes become underscores) and is rescanned on the monitor
interval. An unreachable agent is an error in the log, never an empty inventory, and a bad entry stops
startup. Redirects are not followed, so the token only goes to the configured URL. The same token is used
for every agent; use one token per fleet.

### Lifecycle on a remote Mac

The lifecycle routes act on the Mac the gateway runs on by default. Add `?backend=bkd_macos_<name>` (the
backend id of an `ATLAS_MACOS_AGENTS` entry) to run any of them on that Mac through its agent, including
`GET /macos/devices`. The job carries the backend id, so it runs on the right Mac and the audit entry records it.

This is **off by default and two-sided**. The agent must be started with `ATLAS_MAC_ALLOW_WRITES=1` and a
token (it refuses to start with writes and no token, and the write routes do not exist otherwise). It exposes
`GET /v1/devices` and `POST /v1/ops/{erase,add-volume,delete-volume,eject}`. The refusals above run **on the
Mac**, against its live layout, for every request, so a gateway that misjudges the layout cannot make the agent
erase an internal or system disk. A body with `"dry_run": true` runs the refusals only; the gateway route uses
that for its fast 409, and a refusal reaches the caller as 409, a bad name as 400, an unreachable agent as 503.
Device ids belong to each Mac (`disk4` on one is not `disk4` on another), which is why the backend must be named.

Verified two ways. Against an in-process agent with fixture data and fake ops: refusals, auth, job dispatch,
audit. And on a real Mac (macOS 27.2, 2026-10-19), the agent in `real` mode with writes on, against a throwaway
2 GB disk image: the internal disk, the boot container and a system volume were refused (409); on the image,
`addVolume` with a quota, `deleteVolume`, `resizeContainer` (shrink to 1 GB and back to 2 GB, with diskutil's own
fsck passing), `eraseDisk` and `eject` all worked, and a too-small container surfaced diskutil's own error
(-69493) as a 400. Not verified: a second Mac over the network with TLS, a physical external drive, and the console
page against a live gateway (it has component tests only).

`ATLAS_MAC_ALLOW_WRITES` and `ATLAS_MAC_ALLOW_DISK_IMAGES` accept `1`/`true`/`yes`.

## Roadmap

1. Read-only discovery.
2. Volume lifecycle with hard refusals: erase, add/delete volumes, eject (this document). Still to come:
   container resize, local snapshots, a Disks page.
3. A Mac agent and a remote driver (read-only above, lifecycle behind `ATLAS_MAC_ALLOW_WRITES`). Container resize and a Disks page are in. Still to come: local snapshots (see above).
4. Macs as clients of Atlas storage: NFS/SMB mounts and `atlasctl` are documented in [`MACOS_CLIENT.md`](MACOS_CLIENT.md); the native FUSE mount is a no-go until a spike on a Mac says otherwise.
