"""Offline parser/transaction tests; no network or database is needed."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("geo_import", Path(__file__).with_name("import.py"))
geo = importlib.util.module_from_spec(spec)
spec.loader.exec_module(geo)


class ImportTest(unittest.TestCase):
    def run_import(self, content):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "cities500.txt"
            source.write_text(content, encoding="utf-8")
            # Generated connection values stay in the environment, never psql argv.
            import uuid
            credential = str(uuid.uuid4())
            with patch.dict(os.environ, {"DATABASE_URL": f"postgres://tester:{credential}@127.0.0.1:55499/geo?sslmode=disable"}), \
                    patch.object(geo.sys, "argv", ["import.py", str(source)]), \
                    patch.object(geo.subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()):
                captured = {}

                def execute(argv, **kwargs):
                    captured.update(argv=argv, sql=kwargs["stdin"].read(), env=kwargs["env"])
                    return type("Result", (), {"returncode": 0})()
                run.side_effect = execute
                geo.main()
                self.assertNotIn(credential, " ".join(captured["argv"]))
                self.assertEqual(captured["env"]["PGPASSWORD"], credential)
                return captured

    def test_csv_quotes_names_and_commits_bulk_upsert(self):
        fields = ["1", 'City, "quoted"', "", "", "43.7", "-79.4", "P", "PPL", "CA", "", "08", "", "", "", "1000", "", "", "UTC", "2026-01-01"]
        captured = self.run_import("\t".join(fields) + "\n")
        self.assertIn('"City, ""quoted"""', captured["sql"])
        self.assertTrue(captured["sql"].startswith("BEGIN;"))
        self.assertIn("COPY geo_import", captured["sql"])
        self.assertIn("ON CONFLICT (geonameid) DO UPDATE", captured["sql"])
        self.assertTrue(captured["sql"].endswith("COMMIT;\n"))

    def test_malformed_empty_and_oversized_rows_fail_before_psql(self):
        for content in ["", "broken\trow\n", "x" * 65537]:
            with self.assertRaises(ValueError):
                self.run_import(content)


if __name__ == "__main__":
    unittest.main()
