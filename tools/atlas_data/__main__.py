# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""PYTHONPATH=tools python3 -m atlas_data --help"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

from .api import serve
from .client import Client
from .core import DataError, Store


def main(argv=None):
    p = argparse.ArgumentParser(description="Atlas AI data services: models, jobs, agents, KV, recovery, costs, Iceberg")
    p.add_argument("--url", default="http://127.0.0.1:5120")
    p.add_argument("--ca")
    commands = p.add_subparsers(dest="command", required=True)
    server = commands.add_parser("serve")
    server.add_argument("--root", type=Path, required=True)
    server.add_argument("--config", type=Path, required=True)
    server.add_argument("--host", default="127.0.0.1")
    server.add_argument("--port", type=int, default=5120)
    server.add_argument("--cert")
    server.add_argument("--key")
    server.add_argument("--capacity-bytes", type=int, default=100 * 1024**3)
    commands.add_parser("token-hash", help="hash ATLAS_DATA_TOKEN for the server credential registry")
    call = commands.add_parser("call", help="call an explicit API action with JSON arguments")
    call.add_argument("action")
    call.add_argument("--json", type=Path, required=True)
    ingest = commands.add_parser("ingest")
    ingest.add_argument("kind", choices=("model", "dataset"))
    ingest.add_argument("name")
    ingest.add_argument("revision")
    ingest.add_argument("directory", type=Path)
    export = commands.add_parser("export")
    export.add_argument("artifact")
    export.add_argument("directory", type=Path)
    worker = commands.add_parser("stage")
    worker.add_argument("job")
    worker.add_argument("node")
    worker.add_argument("cache", type=Path)
    args = p.parse_args(argv)
    try:
        if args.command == "serve":
            store = Store(args.root, capacity_bytes=args.capacity_bytes)
            try:
                serve(store, json.loads(args.config.read_text()), args.host, args.port, args.cert, args.key)
            finally:
                store.close()
            return 0
        token = os.environ.get("ATLAS_DATA_TOKEN", "")
        if args.command == "token-hash":
            if len(token) < 32:
                raise DataError("ATLAS_DATA_TOKEN needs at least 32 characters")
            result = {"token_sha256": hashlib.sha256(token.encode()).hexdigest()}
        else:
            client = Client(args.url, token, args.ca)
            if args.command == "call":
                result = client.call(args.action, **json.loads(args.json.read_text()))
            elif args.command == "ingest":
                result = client.call("artifacts.publish", kind=args.kind, name=args.name,
                                     revision=args.revision, files=client.upload(args.directory))
            elif args.command == "export":
                result = client.download(args.artifact, args.directory)
            else:
                result = client.stage(args.job, args.node, args.cache)
        print(json.dumps(result, indent=2))
        return 0
    except (DataError, OSError, ValueError) as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
