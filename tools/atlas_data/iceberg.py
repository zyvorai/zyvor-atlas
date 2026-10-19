# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Optional real Apache Iceberg adapter, private SQL catalog and Parquet warehouse.

Bounded compaction rewrites one table snapshot atomically; expiration is metadata
only. Physical orphan deletion is intentionally excluded from the online service.
"""
from .core import DataError, ident, number, require, sha


class Iceberg:
    def __init__(self, store):
        self.store = store

    def catalog(self, a):
        try:
            from pyiceberg.catalog import load_catalog
        except ImportError as exc:
            raise DataError("install tools/atlas_data/requirements-iceberg.txt", 503) from exc
        root = (self.store.root / "iceberg" / sha(a.tenant.encode())).resolve()
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        return load_catalog("atlas", type="sql", uri=f"sqlite:///{root / 'catalog.sqlite'}",
                            warehouse=(root / "warehouse").as_uri())

    def _table(self, a, name):
        a.admin()  # Table ACL federation is not implied by dataset reader grants.
        return self.catalog(a).load_table(("data", ident(name)))

    def create(self, a, name, fields):
        a.admin()
        ident(name)
        require(isinstance(fields, dict) and 0 < len(fields) <= 100, "1-100 fields required")
        self.catalog(a)  # fail with 503 before importing optional schema modules
        from pyiceberg.schema import Schema
        from pyiceberg.types import NestedField, LongType, DoubleType, StringType, BooleanType
        types = {"long": LongType, "double": DoubleType, "string": StringType, "boolean": BooleanType}
        require(all(ident(n) and t in types for n, t in fields.items()), "unsupported field type")
        with self.store.lock:
            catalog = self.catalog(a)
            catalog.create_namespace_if_not_exists("data")
            schema = Schema(*[NestedField(i, n, types[t](), required=False)
                              for i, (n, t) in enumerate(fields.items(), 1)])
            table = catalog.create_table(("data", name), schema,
                                         properties={"write.target-file-size-bytes": "134217728"})
            with self.store.transaction():
                self.store._audit(a, "iceberg.create", name)
            return self.describe_table(table)

    @staticmethod
    def describe_table(table):
        snap = table.current_snapshot()
        tasks = list(table.scan().plan_files())
        return {"name": table.name()[-1], "snapshot_id": snap.snapshot_id if snap else None,
                "snapshots": len(table.snapshots()), "data_files": len(tasks),
                "rows": sum(t.file.record_count for t in tasks),
                "data_bytes": sum(t.file.file_size_in_bytes for t in tasks)}

    def describe(self, a, name):
        with self.store.lock:
            return self.describe_table(self._table(a, name))

    def append(self, a, name, rows, expected_snapshot):
        a.admin()
        require(isinstance(rows, list) and 0 < len(rows) <= 10000 and all(isinstance(r, dict) for r in rows),
                "append needs 1-10000 row objects")
        with self.store.lock:
            table = self._table(a, name)
            current = table.current_snapshot()
            require((current.snapshot_id if current else None) == expected_snapshot, "snapshot conflict", 409)
            require(all(set(row) <= set(table.schema().column_names) for row in rows), "unknown table column")
            import pyarrow as pa
            arrow = pa.Table.from_pylist(rows, schema=table.schema().as_arrow())
            table.append(arrow, snapshot_properties={"atlas.subject": a.subject})
            with self.store.transaction():
                self.store._audit(a, "iceberg.append", name)
            return self.describe_table(table)

    def rows(self, a, name, limit=100):
        number(limit, 1, 10000)
        with self.store.lock:
            return self._table(a, name).scan(limit=limit).to_arrow().to_pylist()

    def compact(self, a, name, expected_snapshot, max_bytes=128 * 1024**2):
        a.admin()
        number(max_bytes, 1, 128 * 1024**2)
        with self.store.lock:
            table = self._table(a, name)
            before = self.describe_table(table)
            require(before["snapshot_id"] == expected_snapshot, "snapshot conflict", 409)
            # Bound rows and compressed input before materializing; then check Arrow bytes too.
            require(before["data_bytes"] <= max_bytes and before["rows"] <= 1000000,
                    "table exceeds bounded compaction limit; use an external engine", 413)
            if before["data_files"] <= 1:
                return {"before": before, "after": before, "rewritten": False}
            arrow = table.scan().to_arrow()
            require(arrow.nbytes <= max_bytes, "decoded table exceeds compaction limit", 413)
            table.overwrite(arrow.combine_chunks(), snapshot_properties={"atlas.operation": "compaction"})
            with self.store.transaction():
                self.store._audit(a, "iceberg.compact", name)
            return {"before": before, "after": self.describe_table(table), "rewritten": True}

    def expire(self, a, name, older_than_seconds, retain_last=2):
        a.admin()
        number(older_than_seconds, 3600, 315360000)
        number(retain_last, 2, 10000)
        with self.store.lock:
            table = self._table(a, name)
            ordered = sorted(table.snapshots(), key=lambda s: s.timestamp_ms, reverse=True)
            protected = {s.snapshot_id for s in ordered[:retain_last]}
            protected.update(r.snapshot_id for r in table.metadata.refs.values())
            cutoff = int((self.store.clock() - older_than_seconds) * 1000)
            expired = [s.snapshot_id for s in ordered if s.snapshot_id not in protected and s.timestamp_ms < cutoff]
            if expired:
                with table.maintenance.expire_snapshots() as update:
                    for snapshot in expired:
                        update.by_id(snapshot)
            with self.store.transaction():
                self.store._audit(a, "iceberg.expire", name)
            return {"expired_snapshots": expired, "physical_files_deleted": False}
