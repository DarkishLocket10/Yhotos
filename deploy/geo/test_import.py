"""Parser tests plus an isolated PostgreSQL import when DATABASE_URL is set."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
import uuid
from unittest.mock import patch
from urllib.parse import urlsplit, urlunsplit, parse_qsl, urlencode

spec = importlib.util.spec_from_file_location("geo_import", Path(__file__).with_name("import.py"))
geo = importlib.util.module_from_spec(spec)
spec.loader.exec_module(geo)


class ImportTest(unittest.TestCase):
    def run_import(self, content):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "cities500.txt"
            source.write_text(content, encoding="utf-8")
            # Generated connection values stay in the environment, never psql argv.
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


@unittest.skipUnless(os.environ.get("DATABASE_URL"), "DATABASE_URL required for real COPY test")
class PostgresImportTest(unittest.TestCase):
    def test_missing_region_imports_as_empty_text_and_reimports(self):
        # The real migration's constraints and generated point apply. A unique
        # schema keeps the operator's gazetteer and parallel tests untouched.
        schema = "geo_test_" + uuid.uuid4().hex
        env = geo.database_env()

        def query(sql):
            result = geo.subprocess.run(
                ["psql", "-X", "--no-password", "-v", "ON_ERROR_STOP=1", "-qAt"],
                input=sql, text=True, capture_output=True, env=env, check=False,
            )
            self.assertEqual(result.returncode, 0, "PostgreSQL test statement failed")
            return result.stdout.strip()

        query(f"CREATE SCHEMA {schema};")
        self.addCleanup(query, f"DROP SCHEMA {schema} CASCADE;")
        query(f"CREATE TABLE {schema}.gazetteer (LIKE public.gazetteer INCLUDING ALL);")
        scoped_env = dict(env, PGOPTIONS=env.get("PGOPTIONS", "") + f" -c search_path={schema},public")
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "cities500.txt"
            fields = ["1", 'City, "quoted"', "", "", "43.7", "-79.4", "P", "PPL", "CA", "", "", "", "", "", "1000", "", "", "UTC", "2026-01-01"]
            source.write_text("\t".join(fields) + "\n", encoding="utf-8")
            # main parses DATABASE_URL again, so carry test-only search_path in
            # that URL's options too, without exposing connection values in argv.
            url = urlsplit(os.environ["DATABASE_URL"])
            params = dict(parse_qsl(url.query, keep_blank_values=True))
            params["options"] = scoped_env["PGOPTIONS"]
            scoped_env["DATABASE_URL"] = urlunsplit(url._replace(query=urlencode(params)))
            for _ in range(2):
                result = geo.subprocess.run(
                    ["bash", str(Path(__file__).resolve().parents[2] / "scripts/geo-import.sh"), str(source)],
                    text=True, capture_output=True, env=scoped_env, check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Imported 1 cities", result.stdout)
                self.assertEqual(query(
                    f"SELECT count(*)=1 AND bool_and(admin1='' AND admin1 IS NOT NULL "
                    f"AND point[0]=lon AND point[1]=lat) FROM {schema}.gazetteer;"
                ), "t")
                self.assertEqual(query(f"SELECT name FROM {schema}.gazetteer;"), fields[1])


if __name__ == "__main__":
    unittest.main()
