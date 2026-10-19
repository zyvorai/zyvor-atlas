# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Bounded authenticated HTTP API. RPC names are explicit, never dynamic object lookup."""
import base64
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import logging
import socket
import ssl
import threading
from urllib.parse import parse_qs, urlsplit

from .core import Actor, CHUNK, DataError, canonical, require
from .iceberg import Iceberg
from .recovery import Recovery

MAX_JSON = 8 * 1024 * 1024


def unbase64(text):
    require(isinstance(text, str) and len(text) <= 6 * 1024 * 1024, "invalid base64 payload")
    try:
        return base64.b64decode(text, validate=True)
    except (ValueError, TypeError) as exc:
        raise DataError("invalid base64 payload") from exc


def decode(data):
    def pairs(items):
        result = {}
        for k, v in items:
            require(k not in result, "duplicate JSON field")
            result[k] = v
        return result
    def invalid(value):
        raise DataError("non-finite JSON number")
    try:
        value = json.loads(data, object_pairs_hook=pairs, parse_constant=invalid)
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise DataError("invalid JSON") from exc
    require(isinstance(value, dict), "JSON body must be an object")
    return value


class Service:
    def __init__(self, store, credentials, profiles=None):
        self.store = store
        self.credentials = []
        require(isinstance(credentials, list) and credentials, "credential hashes required")
        for entry in credentials:
            from .core import digest, ident
            digest(entry["token_sha256"])
            ident(entry["tenant"])
            ident(entry["subject"])
            require(entry["role"] in ("admin", "operator", "viewer"), "unknown credential role")
            if "expires_at" in entry:
                from .core import number
                number(entry["expires_at"], 1)
            require(not any(c["token_sha256"] == entry["token_sha256"] for c in self.credentials), "duplicate credential hash")
            self.credentials.append(entry)
        iceberg, recovery = Iceberg(store), Recovery(store, profiles)
        self.actions = {
            "artifacts.publish": store.publish, "artifacts.list": store.artifacts,
            "artifacts.get": store.artifact, "artifacts.diff": store.diff, "artifacts.readers": store.set_readers,
            "nodes.heartbeat": store.node, "nodes.evict": store.evict, "nodes.invalidate": store.invalidate, "nodes.staged": store.mark_staged,
            "jobs.renew": store.renew_job, "jobs.release": store.release_job, "jobs.reserve": store.schedule, "jobs.status": store.job,
            "workspaces.create": store.workspace, "workspaces.change": store.workspace_change,
            "workspaces.review": store.workspace_review, "workspaces.publish": store.workspace_publish,
            "workspaces.discard": store.workspace_discard, "costs.rate": store.rate,
            "costs.estimate": store.estimate, "costs.meter": store.charge, "costs.report": store.costs,
            "recovery.drill": recovery.drill, "recovery.latest": recovery.latest,
            "iceberg.create": iceberg.create, "iceberg.describe": iceberg.describe,
            "iceberg.append": iceberg.append, "iceberg.rows": iceberg.rows,
            "iceberg.compact": iceberg.compact, "iceberg.expire": iceberg.expire,
            "audit.list": store.audit, "storage.gc": store.gc,
            "kv.put": lambda a, compatibility, data, ttl: store.kv_put(a, compatibility, unbase64(data), ttl),
            "kv.get": lambda a, compatibility: {"data": base64.b64encode(store.kv_get(a, compatibility)).decode()},
        }

    def authenticate(self, authorization):
        require(isinstance(authorization, str) and authorization.startswith("Bearer "), "bearer token required", 401)
        token = authorization[7:]
        require(32 <= len(token) <= 512, "invalid bearer token", 401)
        hashed = hashlib.sha256(token.encode()).hexdigest()
        match = None
        for entry in self.credentials:
            if hmac.compare_digest(entry["token_sha256"], hashed):
                match = entry
        require(match is not None, "invalid bearer token", 401)
        require(match.get("expires_at", float("inf")) > self.store.clock(), "bearer token expired", 401)
        return Actor(match["tenant"], match["subject"], match["role"])

    def call(self, a, action, arguments):
        require(action in self.actions, "unknown action", 404)
        require(isinstance(arguments, dict) and "a" not in arguments, "invalid arguments")
        try:
            return self.actions[action](a, **arguments)
        except TypeError as exc:
            raise DataError("invalid action arguments") from exc


def handler(service):
    class Handler(BaseHTTPRequestHandler):
        server_version = "AtlasData/1"
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def log_message(self, fmt, *args):
            # Do not log paths, headers, request data or supplied tokens.
            pass

        def reply(self, status, body, binary=False):
            data = body if binary else canonical(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/octet-stream" if binary else "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            self.execute(False)

        def do_POST(self):
            self.execute(True)

        def execute(self, post):
            try:
                parsed = urlsplit(self.path)
                if not post and parsed.path == "/healthz":
                    return self.reply(200, {"status": "ok", "api_version": 1})
                a = service.authenticate(self.headers.get("Authorization"))
                if not post:
                    require(parsed.path.startswith("/v1/chunks/"), "route not found", 404)
                    query = parse_qs(parsed.query)
                    require(set(query) == {"artifact"} and len(query["artifact"]) == 1, "artifact is required")
                    h = parsed.path.removeprefix("/v1/chunks/")
                    return self.reply(200, service.store.artifact_chunk(a, query["artifact"][0], h), True)
                require(not self.headers.get("Transfer-Encoding"), "chunked transfer not supported")
                lengths = self.headers.get_all("Content-Length", [])
                require(len(lengths) == 1 and lengths[0].isdigit(), "one Content-Length required", 411)
                length = int(lengths[0])
                maximum = CHUNK if parsed.path == "/v1/chunks" else MAX_JSON
                require(0 < length <= maximum, "request too large", 413)
                data = self.rfile.read(length)
                require(len(data) == length, "incomplete request")
                if parsed.path == "/v1/chunks":
                    result = service.store.put_chunk(a, data, self.headers.get("X-Content-SHA256"))
                else:
                    require(parsed.path.startswith("/v1/rpc/"), "route not found", 404)
                    result = service.call(a, parsed.path.removeprefix("/v1/rpc/"), decode(data))
                self.reply(200, result)
            except DataError as exc:
                self.reply(exc.status, {"error": str(exc)})
            except (BrokenPipeError, ConnectionResetError, socket.timeout):
                return
            except Exception:
                # Library exceptions may carry filesystem paths or credentials; keep them off the wire.
                logging.exception("Atlas data operation failed")
                self.reply(500, {"error": "operation failed; inspect service logs"})
    return Handler


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 32
    allow_reuse_address = True

    def __init__(self, address, service):
        self.slots = threading.BoundedSemaphore(32)
        super().__init__(address, handler(service))

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()


def serve(store, config, host="127.0.0.1", port=5120, cert=None, key=None):
    require(bool(cert) == bool(key), "certificate and key must be paired")
    require(cert or ipaddress.ip_address(host).is_loopback, "TLS required outside loopback")
    server = Server((host, port), Service(store, config["credentials"], config.get("recovery_profiles")))
    if cert:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.load_cert_chain(cert, key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()
