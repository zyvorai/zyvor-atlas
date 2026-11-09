<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Sealing training checkpoints

`atlasctl checkpoint seal` freezes a finished training checkpoint on an atlas-native filesystem:
it checks the checkpoint is complete, takes a snapshot of the filesystem, retention-locks the
snapshot (`docs/WORM.md`) and prunes older snapshots it made itself. It is glue over REST calls the
gateway already has; no server endpoint was added.

```
atlasctl checkpoint seal --volume vol_native_fs_<id> --root /mnt/train \
    --path /runs/llama/iter_0001000 --preset megatron --lock-days 7 --keep-last 5
```

Unit and integration tested (`crates/atlasctl/src/checkpoint.rs` including the lock-skip and duplicate decisions, `crates/atlasctl/tests/checkpoint_seal.rs`:
the built binary against an in-process gateway on the fixture native driver, with hand-made DCP,
Megatron and Orbax directory layouts). **Never run against a real training job, real PyTorch,
Megatron-LM or Orbax output, or a native cluster.** The layouts below come from reading the
frameworks' source, not from inspecting their output.

## What it does

1. **Completion check** (see below). If the checkpoint does not look complete the command exits 3
   and creates nothing.
2. **Snapshot** `POST /volumes/{id}/snapshots` with `if_not_exists`, named
   `ckpt.<run>.<step>.<hash>` (below). If a snapshot of that volume already has the name, it is
   reused.
3. **Lock** `POST /snapshots/{id}/lock` for `--lock-days` (default 7; `0` skips the lock; at most
   3650). Needs the admin role. A lock can be extended and never shortened. The call is skipped when
   the gateway already reports the snapshot locked (`locked_until`) for at least `--lock-days` from
   now.
4. **Prune** with `--keep-last N`: of the snapshots this command made for the same run on this
   volume, keep the newest N and `DELETE /snapshots/{id}` the rest. The gateway refuses a locked
   snapshot (HTTP 409) until its retention ends, and one with clones; those are reported under
   `retained` and kept. The snapshot sealed by this invocation is never deleted.

The result is JSON on stdout (progress goes to stderr). `--dry-run` runs the completion check and
the reads, prints what would be created, locked (the current `locked_until_unix` and whether a lock
call would be made) and deleted, and changes nothing.

| Exit | Meaning |
|---|---|
| 0 | sealed (or dry run planned) |
| 1 | error: bad arguments, volume missing or not a filesystem, gateway refused |
| 2 | command-line usage error (from `clap`) |
| 3 | refused: the completion marker is missing; nothing was created |
| 4 | sealed, but deleting an old snapshot failed for a reason other than a lock |

If the snapshot was created but the lock call failed, the error says so (exit 1); re-running reuses
the snapshot and retries the lock. When the lock call is skipped the result says so
(`"lock": {"locked": true, "skipped": true, ...}`). If a race made a second same-named snapshot and
this call removed its own (`"duplicate_removed": true`), `snapshot_id` is the surviving one and
`created` is `false`.

## Whole-filesystem snapshots

An atlas-native filesystem snapshot covers the **whole filesystem** (`docs/NATIVE_FS.md`), not a
directory. `--path` decides the name and what is checked; the snapshot also holds everything else
on the volume at that moment, including a half-written checkpoint of another run. Give each run
(or at least each run you prune independently) its own filesystem if that matters. Locking
holds the extents of everything in the snapshot, so a long `--lock-days` costs space.

## Naming and idempotency

`ckpt.<run>.<step>.<hash>`:

- `run` is `--run-id`, else the checkpoint's parent directory (`/runs/llama` gives `runs-llama`);
- `step` is the checkpoint directory's name (`iter_0001000` gives `iter-0001000`);
- `hash` is 8 hex digits of FNV-1a over the normalised full path, so `/r/step_1` and `/r/step-1`
  (same slugs) do not collide. It is not a security feature.

Slugs keep letters and digits and turn everything else into `-`. `--keep-last` deletes only names
that parse as exactly this form, in the same `run`; a snapshot made any other way is never touched.
That prefix is a convention, not a permission: anyone can create a snapshot with such a name.

Re-running on the same checkpoint reuses the snapshot. The gateway reports a snapshot's lock expiry
as `locked_until` (`docs/WORM.md`), so a re-run whose `--lock-days` the lock already covers makes no
lock call at all, and a longer `--lock-days` extends it to *now + `--lock-days`*. "Covers" is exact:
a lock made seven days ago for seven days no longer reaches seven days from now, so a plain re-run a
while later extends it again by the elapsed time. A shorter `--lock-days` on a re-run is not an
error either way (the gateway's 409 "cannot be shortened" is still handled, for a gateway that does
not report `locked_until` or a lock set directly on a node, which `locked_until` does not show).

The snapshot is created with `if_not_exists`, so a seal that finds the name already taken (by a
concurrent seal) gets that snapshot back. That check is best effort (snapshot names are not unique
in the catalog and there is no unique index): two seals can still both pass it. So after creating,
the command lists the volume's snapshots again; if several share the name, the oldest (creation
time, then id) is the keeper, and this call deletes **its own** newer snapshot, never one it did not
make and never a locked one. If the other seal was the newer one, it makes the same decision on its
side. A tie in creation time can leave both in place; `docs/API.md` and the `if_not_exists` note
there describe the residual race, which includes two gateway replicas on Postgres.

Pruning orders snapshots by creation time (second resolution), then by the trailing number of the
step name, then by name.

## How the completion check works

`atlasctl` is a REST client and the checkpoint is inside the filesystem. The gateway has no file
stat or file read route for filesystems (block data routes refuse them), and adding one would mean
a new driver method and endpoint. So the check reads the checkpoint through a mount:

- `--root DIR`: where the filesystem is mounted on this machine (`atlas-native-mount`, or NFS). The
  directory checked is `DIR/<path>`. Run the command where the training job's mount is.
- `--assume-complete`: no check; you vouch for it (for example, rank 0 after the framework's save
  call returned on every rank). Use it when no mount is available.

The check is only as good as what that mount shows. It cannot confirm `--root` is the mount of
`--volume` (a wrong `--root` checks the wrong files), and a mount caches attributes and names for
`--ttl-ms`, so a marker written by another node may not be visible yet (close-to-open
consistency): the failure mode is a spurious refusal, so retry. Time passes between the check and the
snapshot, and a writer that rewrites files in that window is not noticed. A marker says the
framework finished; it does not validate the contents.

Choose one rule:

| Flag | Sealed when |
|---|---|
| `--marker FILE` | `FILE` (relative, no `..`) is a file inside the checkpoint directory |
| `--preset dcp` | `.metadata` and at least one `*.distcp` are in the directory and `.metadata.tmp` is not |
| `--preset megatron` | the directory is `iter_NNNNNNN` or `release`, and `latest_checkpointed_iteration.txt` in its parent holds that iteration (or `release`) |
| `--preset orbax` | `commit_success.txt` is in the directory and its name does not contain `.orbax-checkpoint-tmp` |

## Layouts, as verified from source

Read on 2026-11-08 from the default branches at the commits named. Not run.

**PyTorch `torch.distributed.checkpoint` (DCP), `FileSystemWriter`**
([filesystem.py @ cba4a42](https://github.com/pytorch/pytorch/blob/cba4a42aa487a7222d612ea4f402e857c028d607/torch/distributed/checkpoint/filesystem.py)).
Data files are `f"{prefix}{file_count}.distcp"` where the prefix is `__{rank}_`, so
`__0_0.distcp` in the checkpoint directory (`_metadata_fn = ".metadata"`, `DEFAULT_SUFFIX = ".distcp"`).
`finish()` writes `.metadata.tmp`, optionally fsyncs, **removes an existing `.metadata`, then renames**
the temp file to `.metadata`. So `.metadata` appears last and by rename, which makes it a sound marker
for a new directory. Two caveats from the same code: the directory itself is visible from the first
write (there is no atomic visibility of the directory), and when a directory is *overwritten* the
old `.metadata` stays until `finish()`, so reuse of a directory defeats the marker. The preset
refuses while a `.metadata.tmp` exists for that reason. Use a fresh directory per step. With
`use_collectives=False` each rank writes `__{rank}.metadata` instead of `.metadata`; the preset does not
cover that, use `--marker`.

**Megatron-LM**
([megatron/training/checkpointing.py @ 7d2c1a1](https://github.com/NVIDIA/Megatron-LM/blob/7d2c1a12a056ee89607f92ad93802bb3c88707d3/megatron/training/checkpointing.py)).
`get_checkpoint_name` gives `release` or `'iter_{:07d}'.format(iteration)`; `get_checkpoint_tracker_filename`
is `<save dir>/latest_checkpointed_iteration.txt`. Rank 0 writes the tracker after the
data is saved (after a `torch.distributed.barrier()` for synchronous saves, in the finalize
callback for async saves) with `f.write('release' if release else str(iteration))`. The tracker is
written **in place** (no temp file or rename) and overwritten every save, so it names only the latest
checkpoint: it is a completion marker for the newest `iter_*` directory and for no other. The preset
therefore refuses an older iteration; seal each checkpoint soon after it is saved. Local
(non-persistent) checkpoints do not write it.

**Orbax**
([checkpoint/orbax/checkpoint/_src/path/atomicity.py and atomicity_types.py @ 4090970](https://github.com/google/orbax/tree/409097018af8d3f37af861611c29ef80185c3aae/checkpoint/orbax/checkpoint/_src/path)).
`TMP_DIR_SUFFIX = '.orbax-checkpoint-tmp'` and `COMMIT_SUCCESS_FILE = 'commit_success.txt'`.
`AtomicRenameTemporaryPath` (the default for non-GCS paths, per `atomicity_defaults.py`) writes
`commit_success.txt` into the temporary directory and then renames it to the final name;
`CommitFileTemporaryPath` (default for GCS paths) writes `commit_success.txt` into the final directory.
So `commit_success.txt` marks a committed checkpoint in both modes, and a name containing the temp
suffix is uncommitted. The temp name is built as the final name plus the suffix; the code searches for
the suffix with `find`, so more text may follow it, which is why the preset tests "contains". This
was read on the default branch only; whether older Orbax releases write the file in rename mode is not
confirmed, so for an old Orbax use `--marker`.

### Not confirmed

- Output of a real DCP, Megatron or Orbax run, including `torch.save`-format and `fsdp_dtensor`
  Megatron variants, other Megatron-based stacks (NeMo, Megatron-Bridge) and DCP async writers.
- The Orbax docs pages (`async_checkpointing`) say nothing about the temp directory, rename or
  commit file; only the source does.
- Behaviour on a native filesystem mounted through FUSE or NFS during a real multi-node save (for
  example a directory rename under load).

## Roles

Create needs `operator`, the lock needs `admin`, delete needs `operator`. A tenant-scoped token can
only seal volumes of its tenant. Pass `--token` / `ATLAS_TOKEN` as for any `atlasctl` command.

## Not covered

- Which snapshot holds a *good* checkpoint: nothing verifies the contents or runs a restore.
- A restore command. Clone or restore the snapshot (`atlasctl clone-snapshot`, `restore-snapshot`)
  and read the checkpoint from the clone.
- A lock set directly on a node rather than through the gateway: `locked_until` does not show it.
