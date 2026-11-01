# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
import unittest

from atlas_data.acl_sync import parse_getfacl, readers_from_acl, sync
from atlas_data.core import DataError

DUMP = """# file: models/llama
# owner: alice
# group: ml
user::rwx
user:bob:r--
user:eve:---
group::r-x
group:analysts:rwx
mask::r--
other::r--
default:user:zed:rwx
"""
MAP = {"user:alice": "alice-s", "user:bob": ["bob-s"], "group:ml": ["carol-s", "dan-s"]}


class AclSyncTest(unittest.TestCase):
    def test_parse_skips_default_entries(self):
        owner, group, entries = parse_getfacl(DUMP)
        self.assertEqual((owner, group), ("alice", "ml"))
        self.assertNotIn(("user", "zed", "rwx"), entries)

    def test_mapped_readers_only(self):
        r = readers_from_acl(DUMP, MAP)
        self.assertEqual(r["readers"], ["alice-s", "bob-s", "carol-s", "dan-s"])

    def test_unmapped_principal_is_dropped_not_widened(self):
        r = readers_from_acl(DUMP, MAP)
        self.assertEqual(r["dropped"], ["group:analysts"])

    def test_mask_removes_read_from_named_entries(self):
        text = DUMP.replace("mask::r--", "mask::---")
        r = readers_from_acl(text, MAP)
        self.assertEqual(r["readers"], ["alice-s"])  # the owner entry is not limited by the mask

    def test_world_readable_is_noted_and_not_expanded(self):
        r = readers_from_acl(DUMP, {"other": ["everyone"]})
        self.assertEqual(r["readers"], [])
        self.assertTrue(any("other::" in n for n in r["notes"]))

    def test_no_read_bit_no_reader(self):
        r = readers_from_acl(DUMP, {"user:eve": "eve-s"})
        self.assertEqual(r["readers"], [])

    def test_garbage_is_refused(self):
        with self.assertRaises(DataError):
            parse_getfacl("# owner: a\nuser::zzz\n")
        with self.assertRaises(DataError):
            parse_getfacl("user::rwx\n")

    def test_bad_subjects_refused(self):
        with self.assertRaises(DataError):
            readers_from_acl(DUMP, {"user:bob": [""]})

    def test_dry_run_does_not_call_the_service(self):
        calls = []

        class C:
            def call(self, action, **kw):
                calls.append(action)

        out = sync(C(), "art", DUMP, '{"user:bob": "bob-s"}')
        self.assertEqual((out["applied"], calls), (False, []))
        out = sync(C(), "art", DUMP, '{"user:bob": "bob-s"}', apply=True)
        self.assertEqual((out["applied"], calls), (True, ["artifacts.readers"]))


if __name__ == "__main__":
    unittest.main()
