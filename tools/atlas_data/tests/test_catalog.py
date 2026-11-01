# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
"""Catalog backend selection and the SQL translation used for Postgres. No database needed."""
import tempfile
import unittest
from pathlib import Path

from atlas_data.catalog import Row, SqliteCatalog, open_catalog, to_postgres


class TranslationTests(unittest.TestCase):
    def test_placeholders_and_insert_or_ignore(self):
        self.assertEqual(to_postgres("SELECT body FROM resources WHERE tenant=? AND kind=?"),
                         "SELECT body FROM resources WHERE tenant=%s AND kind=%s")
        self.assertEqual(to_postgres("INSERT OR IGNORE INTO chunk_owners VALUES (?,?,?)"),
                         "INSERT INTO chunk_owners VALUES (%s,%s,%s) ON CONFLICT DO NOTHING")

    def test_sum_is_cast_to_an_integer(self):
        self.assertEqual(to_postgres("SELECT COALESCE(SUM(size),0) FROM chunks"),
                         "SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM chunks")

    def test_other_statements_are_unchanged(self):
        sql = "INSERT INTO resources VALUES (%s,%s,%s,%s) ON CONFLICT(tenant,kind,id) DO UPDATE SET body=excluded.body"
        # A literal % would be read as a driver placeholder, so the translator refuses it instead of guessing.
        with self.assertRaises(ValueError):
            to_postgres(sql)
        self.assertEqual(to_postgres("DELETE FROM chunks WHERE tenant=? AND hash=?"),
                         "DELETE FROM chunks WHERE tenant=%s AND hash=%s")


class RowTests(unittest.TestCase):
    def test_index_name_and_dict_access(self):
        row = Row(["meter", "units"], ("gpu", 7))
        self.assertEqual((row[0], row["units"], row[-1]), ("gpu", 7, 7))
        self.assertEqual(dict(row), {"meter": "gpu", "units": 7})


class SelectionTests(unittest.TestCase):
    def test_default_is_sqlite_and_other_urls_are_refused(self):
        with tempfile.TemporaryDirectory() as d:
            catalog = open_catalog(Path(d))
            self.assertIsInstance(catalog, SqliteCatalog)
            catalog.close()
            with self.assertRaises(ValueError):
                open_catalog(Path(d), "mysql://nope")


if __name__ == "__main__":
    unittest.main()
