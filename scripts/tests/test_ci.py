"""Failure-path tests for the live smoke script and nightly result reader."""
import contextlib
import http.server
import importlib.util
import json
import os
import pathlib
import re
import subprocess
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("load_report", ROOT / "scripts/load-report.py")
LOAD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LOAD)


class SmokeTests(unittest.TestCase):
    def smoke(self, missing=None, cookie=False, mismatch=False, duplicate=False):
        headers = json.loads((ROOT / "scripts/security-headers.json").read_text())
        nonce = "AAAAAAAAAAAAAAAAAAAAAA=="
        headers["content-security-policy"] = ("default-src 'none'; img-src 'self' data:; "
            f"script-src 'nonce-{nonce}'; style-src 'self' 'nonce-{nonce}'; "
            "connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
        class Handler(http.server.BaseHTTPRequestHandler):
            attempts = 0
            def log_message(self, *_):
                pass
            def do_POST(self):
                self.do_GET()
            def do_GET(self):
                status = 200
                body = b"ok"
                if self.path.startswith("/admin") or self.path in ("/restricted", "/missing"):
                    status, body = 404, b"Not found"
                if self.path == "/restricted" and mismatch:
                    body = b"Restricted photo"
                if self.path == "/login":
                    Handler.attempts += 1
                    status = 403 if Handler.attempts <= 10 else 429
                # send_response_only avoids Python's own Server header.
                self.send_response_only(status)
                for key, value in headers.items():
                    if key != missing:
                        self.send_header(key, value)
                self.send_header("referrer-policy", "no-referrer" if self.path.startswith(("/admin/", "/login")) else "strict-origin-when-cross-origin")
                self.send_header("cross-origin-resource-policy", "cross-origin" if self.path.startswith("/i/") else "same-origin")
                if status == 429:
                    self.send_header("retry-after", "60")
                if cookie:
                    self.send_header("set-cookie", "unexpected=value")
                if duplicate:
                    self.send_header("x-content-type-options", "nosniff")
                self.end_headers()
                self.wfile.write(body)
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server, tempfile.TemporaryDirectory() as directory:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            routes = pathlib.Path(directory) / "routes.json"
            routes.write_text(json.dumps({"home": "/", "album": "/album", "photo": "/photo", "search": "/search?q=garden", "derivative": "/i/hash/small.jpeg", "restricted": "/restricted", "missing": "/missing"}))
            result = subprocess.run(["bash", "scripts/security-smoke.sh"], cwd=ROOT,
                env={**os.environ, "YHOTOS_BENCH_URL": f"http://127.0.0.1:{server.server_port}", "YHOTOS_BENCH_ROUTES": str(routes)}, capture_output=True, text=True)
            server.shutdown()
            thread.join()
            return result

    def test_valid_server_passes(self):
        result = self.smoke()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_removing_any_required_header_fails(self):
        headers = json.loads((ROOT / "scripts/security-headers.json").read_text())
        for header in [*headers, "content-security-policy"]:
            with self.subTest(header=header):
                result = self.smoke(missing=header)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("headers", result.stderr)

    def test_cookie_duplicate_and_private_existence_leaks_fail(self):
        for kwargs in ({"cookie": True}, {"duplicate": True}, {"mismatch": True}):
            with self.subTest(kwargs=kwargs):
                self.assertNotEqual(self.smoke(**kwargs).returncode, 0)


class SearchTests(unittest.TestCase):
    def smoke(self, fault=None):
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                semantic = self.path.endswith("?like=seed")
                photos = [{"id": f"photo{i}", "title": f"Garden view {i}", "score": 2 / 61}
                          for i in range(59 if semantic else 60)]
                if fault == "private":
                    photos[0]["id"] = "private"
                elif fault == "seed" and semantic:
                    photos[0]["id"] = "seed"
                elif fault == "single-list":
                    for photo in photos:
                        photo["score"] = 1 / 61
                data = {"total": len(photos), "photos": photos}
                if fault == "no-embeddings" and semantic:
                    data = {"total": 0, "photos": []}
                self.send_response_only(200)
                self.end_headers()
                self.wfile.write(json.dumps(data).encode())

        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server, tempfile.TemporaryDirectory() as directory:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            routes = pathlib.Path(directory) / "routes.json"
            routes.write_text(json.dumps({"photo": "/p/seed", "restricted": "/p/private", "photos": 60,
                "search_text": "/search?q=garden", "search_semantic": "/search?like=seed",
                "search": "/search?q=garden&like=seed"}))
            result = subprocess.run(["bash", "scripts/search-smoke.sh"], cwd=ROOT,
                env={**os.environ, "YHOTOS_BENCH_URL": f"http://127.0.0.1:{server.server_port}",
                     "YHOTOS_BENCH_ROUTES": str(routes)}, capture_output=True, text=True)
            server.shutdown()
            thread.join()
            return result

    def test_seeded_search_passes(self):
        result = self.smoke()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_embeddings_and_visibility_and_fusion_failures_are_rejected(self):
        for fault in ("no-embeddings", "private", "seed", "single-list"):
            with self.subTest(fault=fault):
                self.assertNotEqual(self.smoke(fault).returncode, 0)


class LoadTests(unittest.TestCase):
    def write(self, root):
        for name in ("album", "photo", "derivative", "search", "mixed"):
            (root / f"{name}.json").write_text(json.dumps({"errorDistribution": {}, "statusCodeDistribution": {"200": 1000}, "firstBytePercentiles": {"p50": 0.001, "p99": 0.002}}))
            (root / f"{name}.cpu.json").write_text('{"cores":0.5}')
        (root / "rss-kib.txt").write_text("10000\n11000\n")

    def test_absolute_and_previous_nightly_regressions(self):
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(None):
            root = pathlib.Path(directory)
            self.write(root)
            previous = root / "previous.json"
            self.assertFalse(LOAD.report(root, previous))
            previous.write_text((root / "load.json").read_text())
            data = json.loads((root / "derivative.json").read_text())
            data["firstBytePercentiles"]["p99"] = 0.0024
            (root / "derivative.json").write_text(json.dumps(data))
            self.assertTrue(LOAD.report(root, previous))
            data["firstBytePercentiles"]["p99"] = 0.006
            (root / "derivative.json").write_text(json.dumps(data))
            self.assertTrue(LOAD.report(root, root / "absent"))

    def test_fast_errors_are_not_measurements(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            self.write(root)
            data = json.loads((root / "search.json").read_text())
            data["statusCodeDistribution"] = {"404": 1000}
            (root / "search.json").write_text(json.dumps(data))
            with self.assertRaises(ValueError):
                LOAD.report(root, root / "absent")


class WorkflowTests(unittest.TestCase):
    def test_remote_actions_are_immutable_and_no_privileged_pr_trigger(self):
        for name in ("bench", "lighthouse", "fuzz", "nightly"):
            source = (ROOT / f".github/workflows/{name}.yml").read_text()
            self.assertNotIn("pull_request_target", source)
            for action in re.findall(r"uses:\s*([^\s]+)", source):
                self.assertRegex(action, r"^[^@]+@[0-9a-f]{40}$")
        writers = [name for name in ("bench", "lighthouse", "fuzz", "nightly")
                   if "pull-requests: write" in (ROOT / f".github/workflows/{name}.yml").read_text()]
        self.assertEqual(writers, ["bench"])


class FuzzTests(unittest.TestCase):
    def run_fuzz(self, changed, fail_target=""):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            log = root / "commands"
            (root / "git").write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$CHANGED_PATHS"\n')
            (root / "cargo").write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$*" >> "$FUZZ_LOG"\n[[ "$4" != "$FAIL_TARGET" ]]\n')
            for tool in ("git", "cargo"):
                (root / tool).chmod(0o755)
            result = subprocess.run(["bash", "scripts/fuzz-smoke.sh", "--changed", "a" * 40], cwd=ROOT,
                env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                     "CHANGED_PATHS": changed, "FUZZ_LOG": str(log), "FAIL_TARGET": fail_target,
                     "FUZZ_SECONDS": "60"}, capture_output=True, text=True)
            return result, log.read_text() if log.exists() else ""

    def test_changed_crates_select_their_parsers(self):
        result, commands = self.run_fuzz("crates/yhotos-storage/src/paths.rs")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("run safe_join", commands)
        self.assertIn("run range_header", commands)
        self.assertNotIn("run identify", commands)
        self.assertIn("-max_total_time=60", commands)

    def test_crashes_fail_but_do_not_skip_remaining_targets(self):
        result, commands = self.run_fuzz("Cargo.lock", "identify")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(commands.count("fuzz run"), 7)

    def test_current_search_and_map_parsers_run_when_their_crate_changes(self):
        result, commands = self.run_fuzz("crates/yhotos-http/src/search/query.rs")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("run search_query", commands)
        self.assertIn("run map_bbox", commands)

    def test_unrelated_changes_skip_parsers(self):
        result, commands = self.run_fuzz("docs/process.md")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands, "")


if __name__ == "__main__":
    unittest.main()
