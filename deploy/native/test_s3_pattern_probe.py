# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Offline tests for s3-pattern-probe.py: python3 -m unittest deploy/native/test_s3_pattern_probe.py

The botocore cross-check runs only when botocore is installed; everything else needs only the standard library.
"""
import datetime
import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("probe", os.path.join(HERE, "s3-pattern-probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

AK, SK = "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
WHEN = datetime.datetime(2026, 11, 8, 9, 30, 0, tzinfo=datetime.timezone.utc)


class KeyShape(unittest.TestCase):
    def test_matches_lmcache_encode_storage_key(self):
        # "{model}@{rank:08x}@{group:x}@{hash}", slashes in the model kept (lmcache/v1/distributed/key_codec.py).
        self.assertEqual(probe.lmcache_key("meta-llama/Llama-3.1-8B", 1, 10, "ab" * 32), "meta-llama/Llama-3.1-8B@00000001@a@" + "ab" * 32)


class Signer(unittest.TestCase):
    def test_authorization_shape_and_determinism(self):
        a = probe.sign_headers("GET", "h:9000", "/b/k%40x", [], {}, probe.hashlib.sha256(b"").hexdigest(), AK, SK, "us-east-1", WHEN)
        b = probe.sign_headers("GET", "h:9000", "/b/k%40x", [], {}, probe.hashlib.sha256(b"").hexdigest(), AK, SK, "us-east-1", WHEN)
        self.assertEqual(a, b)
        self.assertTrue(a["authorization"].startswith("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261108/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="))
        self.assertEqual(a["x-amz-date"], "20261108T093000Z")

    def test_signature_changes_with_every_input(self):
        base = ("GET", "h", "/b/k", [], {}, "0" * 64, AK, SK, "us-east-1", WHEN)
        ref = probe.sign_headers(*base)["authorization"]
        for i, alt in ((1, "other"), (2, "/b/other"), (3, [("a", "b")]), (5, "1" * 64), (7, "x" + SK), (8, "eu-west-1")):
            args = list(base)
            args[i] = alt
            self.assertNotEqual(probe.sign_headers(*args)["authorization"], ref, i)

    def test_matches_botocore_when_installed(self):
        try:
            from botocore.auth import S3SigV4Auth
            from botocore.awsrequest import AWSRequest
            from botocore.credentials import Credentials
        except ImportError:
            self.skipTest("botocore not installed")
        body = b"hello"
        digest = probe.hashlib.sha256(body).hexdigest()
        req = AWSRequest(method="PUT", url="http://h:9000/b/org/m%40x?partNumber=2&uploadId=u%2F1", data=body, headers={"Content-Type": "application/octet-stream"})
        req.headers["X-Amz-Content-SHA256"] = digest
        auth = S3SigV4Auth(Credentials(AK, SK), "s3", "us-east-1")
        # botocore reads the clock; freeze it by signing with the date it chose and re-signing ours with that date.
        auth.add_auth(req)
        amz = req.headers["X-Amz-Date"]
        when = datetime.datetime.strptime(amz, "%Y%m%dT%H%M%SZ").replace(tzinfo=datetime.timezone.utc)
        mine = probe.sign_headers("PUT", "h:9000", "/b/org/m%40x", [("partNumber", "2"), ("uploadId", "u/1")], {"Content-Type": "application/octet-stream"}, digest, AK, SK, "us-east-1", when)
        theirs = req.headers["Authorization"]
        if isinstance(theirs, bytes):
            theirs = theirs.decode()
        self.assertEqual(mine["authorization"].split("Signature=")[1], theirs.split("Signature=")[1])


class Addressing(unittest.TestCase):
    def make(self, mode):
        return probe.S3("http://127.0.0.1:9000", "kv", AK, SK, "us-east-1", mode, "s3.lab.test")

    def test_path_style(self):
        s = self.make("path")
        self.assertEqual(s.path("org/m@1"), "/kv/org/m%401")
        self.assertEqual(s.path(), "/kv")
        self.assertEqual(s.host_header, "127.0.0.1:9000")

    def test_virtual_style(self):
        s = self.make("virtual")
        self.assertEqual(s.path("org/m@1"), "/org/m%401")
        self.assertEqual(s.path(), "/")
        self.assertEqual(s.host_header, "kv.s3.lab.test:9000")

    def test_virtual_needs_domain(self):
        with self.assertRaises(ValueError):
            probe.S3("http://x:1", "kv", AK, SK, "r", "virtual", None)


class Listing(unittest.TestCase):
    def test_parse_with_namespace(self):
        xml = b'<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>true</IsTruncated><Contents><Key>a@1</Key></Contents><Contents><Key>b</Key></Contents><NextContinuationToken>t+/=</NextContinuationToken></ListBucketResult>'
        self.assertEqual(probe.parse_list(xml), (["a@1", "b"], "t+/=", True))


class LinkRace(unittest.TestCase):
    def test_local_directory_passes(self):
        probe.FAILS.clear()
        with tempfile.TemporaryDirectory() as d:
            ns = type("A", (), {"dir": d, "rounds": 5, "threads": 4})()
            self.assertEqual(probe.run_link_race(ns), 0)
            self.assertEqual(os.listdir(d), [])


if __name__ == "__main__":
    unittest.main()
