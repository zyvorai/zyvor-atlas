<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# External Ceph import

Atlas can register a Ceph cluster it did not install. The gateway shells out to `ceph` and `rbd`
on the host that runs it, with the monitor list, config file and keyring path you give it. The
keyring file stays on disk. Atlas stores the path in `connection_ref` and does not read the key.

This is a live `RealCephDriver`. It is not the fixture driver, and it does not create OSDs or a
Rook cluster. `POST /backends` without these fields still inserts a `pending` Ceph row, which has
no driver.

## `POST /api/atlas/v1/backends`

Admin. At least one of `mon_host` or `conf_path` is required. `keyring_path` and `ceph_user` are
optional. Paths must be absolute, must not contain `..`, and must already be files on the gateway.

```json
{
  "name": "site-ceph",
  "backend_type": "ceph",
  "mode": "external",
  "mon_host": "10.1.0.1:6789,v2:10.1.0.2:3300",
  "conf_path": "/etc/ceph/site.conf",
  "keyring_path": "/etc/ceph/ceph.client.admin.keyring",
  "ceph_user": "client.admin"
}
```

The response is `status: active`. `connection_ref` looks like
`ceph:mon_host=10.1.0.1:6789;conf=/etc/ceph/site.conf;keyring=/etc/ceph/ceph.client.admin.keyring;name=client.admin`.
Discovery runs immediately. A monitor that does not answer fails that pass after 30 seconds; the
backend row stays active so a later `POST /api/atlas/v1/backends/{id}/discover` can retry. No
fixture pools are invented.

`mon_host` accepts a comma-separated list of `host`, `host:port`, `v1:` / `v2:` addresses, and
`[ipv6]:port`. The driver passes it as `--mon-host`, plus `--conf`, `--keyring` and `--name` when
those fields are set. Arguments are an array. They are not a shell string.

Block reads and writes (`read_volume` / `write_volume`) use `conf_path`. A body that only sets
`mon_host` can discover the cluster through the CLI, and block I/O returns a validation error
until a `ceph.conf` is supplied. Put `mon_host` and the keyring path in that file, or pass both
`conf_path` and `keyring_path`.

The primary backend (`bkd_ceph_lab`, `ATLAS_CEPH_DRIVER_MODE`) is unchanged. Jobs that call
`ceph` / `rbd` without a driver still use the process default (`CEPH_CONF` or
`/etc/ceph/ceph.conf`).

## Environment

Set these on the gateway process to register the cluster at startup, without a POST. Any of the
first three turns it on. A missing file or a bad monitor string refuses to boot.

| Variable | Meaning |
|---|---|
| `ATLAS_CEPH_EXTERNAL_MON_HOST` | Same syntax as `mon_host` |
| `ATLAS_CEPH_EXTERNAL_CONF` | Absolute path to `ceph.conf` |
| `ATLAS_CEPH_EXTERNAL_KEYRING` | Absolute path to the client keyring |
| `ATLAS_CEPH_EXTERNAL_USER` | Client name (`admin` or `client.admin`) |
| `ATLAS_CEPH_EXTERNAL_ID` | Backend id (default `bkd_ceph_external`; must not be `bkd_ceph_lab`) |
| `ATLAS_CEPH_EXTERNAL_NAME` | Display name (default `external-ceph`) |

The gateway image or host needs the `ceph` and `rbd` binaries, and the keyring file mounted in,
mode `0600`, owned by the gateway user. Do not put the key in the environment or in the database.
