# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Metadata catalog backends for the data service: SQLite (one process, the default) or Postgres.

`Store` talks to a small connection-like object: `execute(sql, params)` with `?` placeholders,
`begin()` / `commit()` / `rollback()`, `IntegrityError` and rows readable by index or by column name.
SQLite runs it directly. Postgres translates the few SQLite-isms the store uses and serialises write
transactions across replicas with a transaction-scoped advisory lock, which gives the same
"one writer at a time" behaviour as SQLite's `BEGIN IMMEDIATE`, so the store's read-check-write code
(capacity, quotas, reservations) stays correct with several service replicas on one database.

Chunk bytes are not in the catalog. Replicas need the same `--root` (a shared volume) to see them.
"""
from __future__ import annotations

import re
import sqlite3

# One advisory-lock key for every write transaction of the data service.
WRITE_LOCK_KEY = 0x41544C44  # "ATLD"

SQLITE_SCHEMA = '''
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS resources (
  tenant TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL,
  body TEXT NOT NULL, PRIMARY KEY(tenant,kind,id));
CREATE TABLE IF NOT EXISTS chunks (
  tenant TEXT NOT NULL, hash TEXT NOT NULL, size INTEGER NOT NULL,
  PRIMARY KEY(tenant,hash));
CREATE TABLE IF NOT EXISTS chunk_owners (
  tenant TEXT NOT NULL, hash TEXT NOT NULL, subject TEXT NOT NULL,
  PRIMARY KEY(tenant,hash,subject));
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, tenant TEXT NOT NULL,
  subject TEXT NOT NULL, action TEXT NOT NULL, resource TEXT NOT NULL,
  at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS ledger (
  tenant TEXT NOT NULL, event_id TEXT NOT NULL, job TEXT NOT NULL,
  meter TEXT NOT NULL, units INTEGER NOT NULL, micros INTEGER NOT NULL,
  PRIMARY KEY(tenant,event_id));
'''

POSTGRES_SCHEMA = [
    '''CREATE TABLE IF NOT EXISTS resources (
  tenant TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL,
  body TEXT NOT NULL, PRIMARY KEY(tenant,kind,id))''',
    '''CREATE TABLE IF NOT EXISTS chunks (
  tenant TEXT NOT NULL, hash TEXT NOT NULL, size BIGINT NOT NULL,
  PRIMARY KEY(tenant,hash))''',
    '''CREATE TABLE IF NOT EXISTS chunk_owners (
  tenant TEXT NOT NULL, hash TEXT NOT NULL, subject TEXT NOT NULL,
  PRIMARY KEY(tenant,hash,subject))''',
    '''CREATE TABLE IF NOT EXISTS events (
  seq BIGSERIAL PRIMARY KEY, tenant TEXT NOT NULL,
  subject TEXT NOT NULL, action TEXT NOT NULL, resource TEXT NOT NULL,
  at DOUBLE PRECISION NOT NULL)''',
    '''CREATE TABLE IF NOT EXISTS ledger (
  tenant TEXT NOT NULL, event_id TEXT NOT NULL, job TEXT NOT NULL,
  meter TEXT NOT NULL, units BIGINT NOT NULL, micros BIGINT NOT NULL,
  PRIMARY KEY(tenant,event_id))''',
]

_OR_IGNORE = re.compile(r"^\s*INSERT OR IGNORE INTO\b(.*)$", re.S | re.I)
_SUM = re.compile(r"COALESCE\(SUM\((\w+)\),\s*0\)", re.I)


def to_postgres(sql: str) -> str:
    """The Postgres form of one SQL statement the store issues (`?` placeholders, INSERT OR IGNORE, SUM)."""
    if "%" in sql:
        raise ValueError("a literal % in store SQL would be read as a placeholder")
    sql = sql.replace("?", "%s")
    m = _OR_IGNORE.match(sql)
    if m:
        sql = "INSERT INTO" + m.group(1) + " ON CONFLICT DO NOTHING"
    # SUM over BIGINT is NUMERIC in Postgres (a Decimal in Python); the store compares plain integers.
    return _SUM.sub(r"CAST(COALESCE(SUM(\1),0) AS BIGINT)", sql)


class Row(tuple):
    """A result row readable by index or column name, and convertible with `dict(row)`."""

    def __new__(cls, names, values):
        row = super().__new__(cls, values)
        row._names = tuple(names)
        return row

    def __getitem__(self, key):
        if isinstance(key, str):
            return super().__getitem__(self._names.index(key))
        return super().__getitem__(key)

    def keys(self):
        return list(self._names)


class SqliteCatalog:
    IntegrityError = sqlite3.IntegrityError

    def __init__(self, path):
        self.db = sqlite3.connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.executescript(SQLITE_SCHEMA)

    def execute(self, sql, params=()):
        return self.db.execute(sql, params)

    def begin(self):
        self.db.execute("BEGIN IMMEDIATE")

    def commit(self):
        self.db.commit()

    def rollback(self):
        self.db.rollback()

    def close(self):
        self.db.close()


class PostgresCatalog:
    def __init__(self, url):
        try:
            import psycopg
        except ImportError as exc:  # pragma: no cover - depends on the install
            raise RuntimeError("a postgres:// catalog needs the psycopg package (pip install 'psycopg[binary]')") from exc
        self._psycopg = psycopg
        self.IntegrityError = psycopg.errors.IntegrityError
        self.db = psycopg.connect(url, autocommit=True, row_factory=self._row_factory)
        for statement in POSTGRES_SCHEMA:
            self.db.execute(statement)

    @staticmethod
    def _row_factory(cursor):
        names = [c.name for c in cursor.description or []]
        return lambda values: Row(names, values)

    def execute(self, sql, params=()):
        return self.db.execute(to_postgres(sql), params)

    def begin(self):
        self.db.execute("BEGIN")
        self.db.execute("SELECT pg_advisory_xact_lock(%s)", (WRITE_LOCK_KEY,))

    def commit(self):
        self.db.execute("COMMIT")

    def rollback(self):
        self.db.execute("ROLLBACK")

    def close(self):
        self.db.close()


def open_catalog(root, database_url=None):
    """Postgres when `database_url` is a postgres:// URL, otherwise SQLite in `root`."""
    if database_url and database_url.split(":", 1)[0] in ("postgres", "postgresql"):
        return PostgresCatalog(database_url)
    if database_url:
        raise ValueError("the data catalog URL must be postgres:// (leave it unset for SQLite)")
    return SqliteCatalog(root / "data.sqlite")
