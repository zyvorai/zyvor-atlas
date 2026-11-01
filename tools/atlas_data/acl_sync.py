# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Turn a POSIX ACL (`getfacl` text) on a source directory into an artifact's reader list.

Fail closed: only principals that the operator's map names become readers, a principal that is not in
the map is dropped and reported, and the world (`other::`) is never expanded into subjects. The
function is pure; the CLI (`acl-sync`) shows the change first and applies it only with `--apply`.
"""
import json

from .core import DataError


def parse_getfacl(text):
    """Return (owner, group, entries). An entry is (kind, name, perms) with kind user|group|mask|other.

    `default:` entries describe inheritance for new files, not access to this path, so they are skipped.
    """
    owner = group = None
    entries = []
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip() if not raw.startswith("#") else raw.strip()
        if raw.startswith("# owner:"):
            owner = raw.split(":", 1)[1].strip()
            continue
        if raw.startswith("# group:"):
            group = raw.split(":", 1)[1].strip()
            continue
        if not line or line.startswith("#") or line.startswith("default:"):
            continue
        parts = line.split(":")
        if len(parts) != 3 or parts[0] not in ("user", "group", "mask", "other"):
            raise DataError(f"unrecognised ACL line: {raw!r}")
        perms = parts[2].split()[0]
        if len(perms) != 3 or any(c not in "rwx-" for c in perms):
            raise DataError(f"unrecognised ACL permissions: {raw!r}")
        entries.append((parts[0], parts[1], perms))
    if owner is None:
        raise DataError("getfacl text has no '# owner:' header")
    return owner, group, entries


def readers_from_acl(text, principal_map):
    """Return {"readers": [...], "dropped": [...], "notes": [...]} for a getfacl dump.

    principal_map maps "user:NAME" or "group:NAME" to one subject or a list of subjects.
    """
    owner, group, entries = parse_getfacl(text)
    mask = next((p for k, n, p in entries if k == "mask"), None)
    candidates = {}
    notes = []
    for kind, name, perms in entries:
        if kind == "other":
            if "r" in perms:
                notes.append("other:: grants read to everyone; it is not expanded into subjects")
            continue
        if kind == "mask":
            continue
        effective = perms
        # The mask limits every entry except the owner and "other".
        if mask is not None and (name or kind == "group"):
            effective = "".join(a if b != "-" else "-" for a, b in zip(perms, mask))
        if "r" not in effective:
            continue
        principal = f"{kind}:{name}" if name else (f"user:{owner}" if kind == "user" else f"group:{group}")
        if principal.endswith(":None"):
            continue
        candidates[principal] = effective
    readers, dropped = set(), []
    for principal in sorted(candidates):
        mapped = principal_map.get(principal)
        if mapped is None:
            dropped.append(principal)
            continue
        subjects = [mapped] if isinstance(mapped, str) else mapped
        if not isinstance(subjects, list) or not all(isinstance(s, str) and 0 < len(s) <= 200 for s in subjects):
            raise DataError(f"invalid subjects for {principal}")
        readers.update(subjects)
    return {"readers": sorted(readers), "dropped": dropped, "notes": notes}


def sync(client, artifact, getfacl_text, map_text, apply=False):
    """Compute the reader list from the ACL; set it on the artifact only when apply is true."""
    mapping = json.loads(map_text)
    if not isinstance(mapping, dict):
        raise DataError("principal map must be a JSON object")
    want = readers_from_acl(getfacl_text, mapping)
    result = {"artifact": artifact, "readers": want["readers"], "dropped": want["dropped"],
              "notes": want["notes"], "applied": False}
    if apply:
        client.call("artifacts.readers", key=artifact, readers=want["readers"])
        result["applied"] = True
    return result
