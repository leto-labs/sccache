#!/usr/bin/env python3
"""Tests for the host report, log rotation and fleet publication."""

from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch


HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
REPORT = runpy.run_path(str(HERE / "health-report.py"))
FLEET = runpy.run_path(str(HERE / "fleet-report.py"))
NOW = datetime(2026, 10, 10, 12, tzinfo=timezone.utc)


class HealthReportTest(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = root = Path(temp.name)
        cache = root / "cache"
        shard = cache / "a" / "b"
        shard.mkdir(parents=True)
        (shard / ("ab" + "0" * 62)).write_bytes(b"x" * 8192)
        (shard / "other-file").touch()

        self.log = cache / "misses.jsonl"
        lines = (FIXTURES / "misses.jsonl").read_bytes().splitlines(keepends=True)
        self.log.with_suffix(".jsonl.1").write_bytes(b"".join(lines[:55]))
        self.log.write_bytes(b"".join(lines[55:]) + b"invalid\n{")

        config = root / "sccache.toml"
        config.write_text((FIXTURES / "config.toml").read_text()
                          .replace("@CACHE@", str(cache)).replace("@LOG@", str(self.log)))

        self.binary = root / "sccache"
        self.binary.write_text("#!/bin/sh\n"
                               "if [ \"$*\" = \"--version\" ]; then echo sccache test-client; exit; fi\n"
                               "[ \"$*\" = \"--show-stats --stats-format=json\" ] &&\n"
                               "[ \"$SCCACHE_SERVER_PORT\" = 4321 ] &&\n"
                               "[ \"$SCCACHE_SERVER_AUTOSTART\" = 0 ] &&\n"
                               "[ \"$SCCACHE_CONF\" = /dev/null ] &&\n"
                               "[ \"$SCCACHE_DIR\" = /dev/null/leto-build-health ] &&\n"
                               "[ -z \"$SCCACHE_START_SERVER$SCCACHE_SERVER_UDS\" ] &&\n"
                               "echo '{\"stats\":{},\"effective_cache_mode\":null}'\n")
        self.binary.chmod(0o755)

        mock_bin = root / "bin"
        mock_bin.mkdir()
        systemctl = mock_bin / "systemctl"
        systemctl.write_text("#!/bin/sh\n"
                             "printf '%s\\n' ActiveState=\"${UNIT_STATE:-active}\" "
                             "'ActiveEnterTimestamp=Sat 2026-10-10 11:58:00 UTC' MainPID=42\n")
        systemctl.chmod(0o755)
        mock_env = patch.dict(os.environ, PATH=f"{mock_bin}:{os.environ['PATH']}")
        mock_env.start()
        self.addCleanup(mock_env.stop)
        proc = root / "proc" / "42"
        proc.mkdir(parents=True)
        (proc / "status").write_text("VmRSS:\t9437184 kB\n")
        (proc / "io").write_text("rchar: 1\nread_bytes: 0\nwrite_bytes: 600000000000\ncancelled_write_bytes: 100000000000\n")

        prewarm_state = root / "prewarm"
        state = prewarm_state / "fairfill"
        state.mkdir(parents=True)
        prewarm_config = root / "prewarm-config"
        prewarm_config.mkdir()
        repo = root / "canonical repo"
        repo.mkdir()
        git_env = {"PATH": os.environ["PATH"], "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.com",
                   "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.com"}

        def git(*args):
            return subprocess.check_output(["git", "-C", repo, "-c", "core.hooksPath=/dev/null", *args],
                                           text=True, env=git_env).strip()

        git("init", "-q", "-b", "test-branch")
        git("commit", "-qm", "warmed", "--allow-empty")
        warmed = git("rev-parse", "HEAD")
        for index in range(12):
            git("commit", "-qm", f"new {index}", "--allow-empty")
        (state / "status.json").write_text((FIXTURES / "prewarm-status.json").read_text().replace("@WARMED@", warmed))
        marker = state / "last-warmed"
        marker.write_text(warmed + "\n")
        os.utime(marker, (NOW.timestamp() - 4 * 3600,) * 2)
        (prewarm_config / "fairfill.conf").write_text(
            f"export PREWARM_REPO='{repo}' # canonical\nPREWARM_BRANCH=test-branch\nPREWARM_JOBS=2\n")

        self.trees = root / "worktrees"
        self.checkouts = root / "checkouts"
        self.private = self.session(self.trees / "fairfill/tree/target/debug", "engine-sccache-123")
        self.other = self.session(self.checkouts / "fairfill/target/host-triple/release", "engine-xyz")
        self.session(self.trees / "fairfill/tree/target/host-triple/debug/nested", "too-deep")

        self.argv = ["--config", str(config), "--stats-json", str(FIXTURES / "stats.json"),
                     "--binary", str(self.binary), "--unit", "test-unit", "--proc-root", str(proc.parent),
                     "--prewarm-state", str(prewarm_state), "--prewarm-config", str(prewarm_config),
                     "--worktrees", str(self.trees), "--checkouts", str(self.checkouts)]

    @staticmethod
    def session(profile, name):
        path = profile / "incremental" / name
        path.mkdir(parents=True)
        (path / "data").write_bytes(b"x" * 4096)
        return path

    def report(self, *extra):
        return REPORT["make_report"](REPORT["parse_args"]([*self.argv, *extra]), now=NOW)

    def test_host_aggregates_and_warnings(self):
        report = self.report()
        self.assertEqual(report["schema"], "leto-build-health/1")
        server = report["server"]
        self.assertTrue(server["available"])
        self.assertEqual(server["unit"], "test-unit")
        self.assertEqual(server["version"], "test-daemon")
        self.assertEqual(server["binary_version"], "sccache test-client")
        self.assertEqual(server["sha256"], hashlib.sha256(self.binary.read_bytes()).hexdigest())
        self.assertEqual(server["uptime_seconds"], 120)
        self.assertEqual(server["disk_write_bytes"], 500_000_000_000)
        self.assertEqual(server["disk_write_gib_per_hour"], round(500_000_000_000 / 1024 ** 3 * 30, 1))
        self.assertTrue(any("GiB/h to disk" in warning for warning in report["warnings"]), report["warnings"])
        self.assertEqual(server["rss_bytes"], 9 * 1024 ** 3)
        stats = server["stats"]
        self.assertEqual((stats["requests"], stats["executed"]), (200, 170))
        self.assertEqual(stats["hits"], {"Rust": 90, "C/C++": 10})
        self.assertEqual(stats["misses"], {"Rust": 30, "C/C++": 20})
        self.assertEqual(stats["hit_rate_pct"], {"rust": 75, "overall": 66.67})
        self.assertEqual(list(stats["not_cached"])[:2], ["crate-type", "incremental session"])
        self.assertEqual(len(stats["not_cached"]), 10)
        self.assertEqual(stats["cache_errors"], {"Rust": 3})
        self.assertEqual((stats["cache_read_errors"], stats["cache_write_errors"]), (1, 2))
        self.assertEqual((stats["rust_incremental_not_stored"], stats["rust_linked_entry_too_large"]), (4, 5))
        self.assertEqual(stats["miss_explain"]["own-input"], 8)
        self.assertEqual(report["config"]["cache_cap_bytes"], 512 * 1024 ** 2)
        self.assertEqual(report["config"]["rust_cache_linked"], "tests")
        self.assertEqual(report["config"]["rust_cache_linked_max_entry_bytes"], 1234567)
        self.assertFalse(report["config"]["autostart"])
        self.assertTrue(report["config"]["reflink"])
        self.assertEqual(report["config"]["shutdown_drain_secs"], 300)
        cache = report["cache"]
        self.assertEqual((cache["cap_bytes"], cache["fill_pct"], cache["fill_basis"]),
                         (1000, 102, "server_size_bytes"))
        self.assertGreaterEqual(cache["apparent_bytes"], 8192)
        self.assertGreaterEqual(cache["allocated_bytes"], 8192)
        self.assertEqual(cache["entry_count"], 1)

        misses = report["misses"]
        self.assertEqual(misses["total"], 234)
        self.assertEqual(misses["classifications"], {"new": 61, "own-input": 64, "dependency": 74, "unchanged": 35})
        fairfill = misses["projects"]["fairfill"]
        self.assertEqual(fairfill["classifications"], {"new": 42, "own-input": 48, "dependency": 55, "unchanged": 21})
        self.assertEqual(fairfill["unchanged"], 21)
        self.assertEqual(fairfill["own_input_components"], {
            "env:FEATURE": 36, "source:checkout": 30, "arg:-C opt-level": 18, "arg:--cfg": 12,
            "compiler": 12, "cwd": 12, "source:OUT_DIR": 12, "source:absolute": 12, "staticlib": 12,
        })
        self.assertEqual(fairfill["own_input_crates"][0],
                         {"crate": "engine", "count": 36, "main_component": "env:FEATURE"})
        self.assertEqual(fairfill["dependency_externs"], {"engine": 55, "router": 13})
        self.assertEqual(misses["projects"]["longbow"]["own_input_components"],
                         {"source:checkout": 16, "source:OUT_DIR": 16, "arg:(legacy)": 16})
        self.assertEqual(self.report("--hours", "1")["misses"]["total"], 173)

        log = report["miss_log"]
        self.assertEqual(log["current"]["size_bytes"], self.log.stat().st_size)
        self.assertEqual(log["current"]["line_count"], 182)
        self.assertEqual(log["current"]["oldest_timestamp"], "2026-10-09T11:59:00+00:00")
        self.assertEqual(log["current"]["newest_timestamp"], "2026-10-10T12:01:00+00:00")
        self.assertEqual(log["previous"]["line_count"], 55)
        warm = report["prewarm"]["fairfill"]
        self.assertEqual((warm["result"], warm["lag_commits"], warm["lag_minutes"]), ("deferred", 12, 240))
        self.assertEqual((warm["duration_seconds"], warm["paused_seconds"]), (11.25, 2.5))
        self.assertEqual(warm["disk"]["checkout"]["allocated_bytes"], 4096)
        incremental = report["incremental"]["projects"]["fairfill"]
        self.assertEqual(incremental["worktrees_scanned"], 1)
        self.assertTrue(incremental["canonical_scanned"])
        for key, path in (("sccache_bytes", self.private), ("other_bytes", self.other)):
            expected = int(subprocess.check_output(["du", "-s", "-B1", str(path)]).split()[0])
            self.assertEqual(incremental[key], expected)

        for message in ("RSS exceeds", "tracked size exceeds cap", "of cap on disk", "over 3 hours", "12 commits behind", "invalid JSON"):
            self.assertTrue(any(message in warning for warning in report["warnings"]), report["warnings"])
        text = REPORT["markdown"](report)
        for content in ("## Server", "## Cache", "## Misses · last 24 h", "## Miss log",
                        "## Pre-warm", "## Incremental sessions", "## Warnings", "75.0%", "env:FEATURE (36)"):
            self.assertIn(content, text)

    def test_cli_reports_offline_without_asking_to_start_a_server(self):
        argv = self.argv[:2] + self.argv[4:]
        env = dict(os.environ, UNIT_STATE="inactive", SCCACHE_START_SERVER="1",
                   SCCACHE_SERVER_AUTOSTART="1", SCCACHE_SERVER_UDS="/tmp/should-not-use")
        proc = subprocess.run([sys.executable, HERE / "health-report.py", *argv, "--port", "4321"],
                              capture_output=True, check=True, env=env)
        report = json.loads(proc.stdout)
        self.assertFalse(report["server"]["available"])
        self.assertIsNone(report["server"]["stats"])
        self.assertTrue(any("client returned offline stats" in warning for warning in report["warnings"]))
        self.assertIn("Systemd unit test-unit is inactive", report["warnings"])

        out = self.root / "report.md"
        proc = subprocess.run([sys.executable, HERE / "health-report.py", *self.argv, "--format", "markdown",
                               "--out", out], capture_output=True, check=True)
        self.assertEqual(proc.stdout, b"")
        self.assertIn("102.0%", out.read_text())

    def test_cache_over_cap_on_du_alone_is_not_reported_as_tracked(self):
        # Without daemon stats (a timeout at high load), du is the only size, and
        # du counts reflink-shared entries at file length.
        warnings = []
        cache = REPORT["cache_report"]({"cache_dir": str(self.root / "cache"), "cache_cap_bytes": 1000}, {}, warnings)
        self.assertEqual(cache["fill_basis"], "allocated_bytes")
        self.assertGreater(cache["fill_pct"], 101)
        self.assertGreaterEqual(cache["written_last_hour"]["entries"], 1)
        self.assertGreater(cache["written_last_hour"]["bytes"], 0)
        self.assertFalse(any("tracked size" in warning for warning in warnings), warnings)
        self.assertTrue(any("of cap on disk" in warning for warning in warnings), warnings)

    def test_automatic_fast_scan_extrapolates_by_project(self):
        for index in range(101):
            project = "recent" if index else "old"
            target = self.trees / project / f"tree-{index}" / "target"
            target.mkdir(parents=True)
            for path in (target.parent, target):
                os.utime(path, (index + 10, index + 10))
        for target in (self.private.parents[2], self.private.parents[3]):
            os.utime(target, (9, 9))
        func = REPORT["incremental_report"]
        with patch.dict(func.__globals__, incremental_size=lambda *_: {"sccache": 100, "other": 3}):
            summary = func(REPORT["parse_args"](self.argv), [])
            full = func(REPORT["parse_args"]([*self.argv, "--no-fast"]), [])
        self.assertTrue(summary["fast"])
        self.assertEqual(summary["worktrees_found"], 102)
        recent = summary["projects"]["recent"]
        self.assertTrue(recent["estimated"])
        self.assertEqual((recent["worktrees_scanned"], recent["sccache_bytes"], recent["other_bytes"]),
                         (20, 10000, 300))
        self.assertIsNone(summary["projects"]["old"]["sccache_bytes"])
        self.assertFalse(full["fast"])
        self.assertEqual(full["projects"]["recent"]["worktrees_scanned"], 100)
        self.assertEqual(full["projects"]["old"]["sccache_bytes"], 100)

    def test_large_log_warning_without_scanning_sparse_fixture(self):
        # Supply a large snapshot size while retaining the fixture's small contents.
        real_fstat = os.fstat
        func = REPORT["miss_log_report"]

        class SizedSnapshot:
            def __init__(self, fd):
                info = real_fstat(fd)
                self.st_dev, self.st_ino, self.st_size = info.st_dev, info.st_ino, 500_000_001

        warnings = []
        with patch.object(os, "fstat", side_effect=SizedSnapshot):
            func(self.log, NOW, 24, warnings)
        self.assertIn("Miss log exceeds 500 MB", warnings)


class RotationTest(unittest.TestCase):
    def test_cli_rotates_an_explicit_temp_log_without_a_config(self):
        with TemporaryDirectory() as temp:
            directory = Path(temp)
            log = directory / "misses.jsonl"
            log.write_text("archive\n")
            log.chmod(0o664)
            unrelated = directory / "entry"
            unrelated.write_text("keep\n")
            subprocess.run([sys.executable, HERE / "health-report.py", "--rotate", "--miss-log", log,
                            "--config", directory / "absent-config"], check=True, umask=0o077)
            self.assertEqual(log.read_text(), "")
            self.assertEqual(log.stat().st_mode & 0o777, 0o664)
            self.assertEqual(log.with_suffix(".jsonl.1").read_text(), "archive\n")
            self.assertEqual(unrelated.read_text(), "keep\n")

    def test_rename_preserves_inflight_and_future_writes(self):
        with TemporaryDirectory() as temp:
            log = Path(temp) / "misses.jsonl"
            previous = log.with_suffix(".jsonl.1")
            previous.write_bytes(b"expired\n")
            log.write_bytes(b"current\n")
            log.chmod(0o640)
            with log.open("ab", buffering=0) as open_before_rotation:
                REPORT["rotate_misses"](log)
                self.assertEqual(log.read_bytes(), b"")
                self.assertEqual(log.stat().st_mode & 0o777, 0o640)
                open_before_rotation.write(b"in flight\n")
            REPORT["rotate_misses"](log)
            with log.open("ab") as new_append:
                new_append.write(b"new\n")
            self.assertEqual(previous.read_bytes(), b"current\nin flight\n")
            self.assertEqual(log.read_bytes(), b"new\n")

    def test_writer_recreating_log_between_rename_and_create(self):
        with TemporaryDirectory() as temp:
            log = Path(temp) / "misses.jsonl"
            log.write_text("archive\n")
            open_file = os.open

            def race(path, flags, *args):
                if path == log and flags & os.O_EXCL:
                    log.write_text("server appended\n")
                return open_file(path, flags, *args)

            with patch.object(os, "open", side_effect=race):
                REPORT["rotate_misses"](log)
            self.assertEqual(log.read_text(), "server appended\n")
            self.assertEqual(log.with_suffix(".jsonl.1").read_text(), "archive\n")


class FleetTest(unittest.TestCase):
    def test_remote_script_and_latest_links(self):
        report = {"schema": "leto-build-health/1", "host": "worker-01",
                  "runtime_seconds": 1.23, "warnings": [], "misses": {
                      "total": 7, "classifications": {"new": 2, "dependency": 5}}}
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, json.dumps(report).encode(), b"")) as run:
            self.assertEqual(FLEET["collect"]("worker-01", b"host script", 60), report)
        run.assert_called_once_with(["tailscale", "ssh", "leovigna@worker-01", "python3 - --format json"],
                                    input=b"host script", capture_output=True, check=True, timeout=60)

        fleet = {"schema": "leto-build-health-fleet/1", "generated_at": NOW.isoformat(),
                 "hosts": {"worker-01": report, "worker-02": {"warnings": ["unavailable"],
                                                             "runtime_seconds": None}}}
        with TemporaryDirectory() as temp:
            directory = Path(temp)
            for stamp in ("first", "second"):
                FLEET["publish"](fleet, directory, stamp)
            self.assertEqual((directory / "latest.json").readlink(), Path("second.json"))
            self.assertEqual((directory / "latest.md").readlink(), Path("second.md"))
            self.assertEqual(json.loads((directory / "latest.json").read_text()), fleet)
            text = (directory / "latest.md").read_text()
            self.assertIn("| worker-01 | — | 7 | dependency (5) | — | — | — | — | 0 | 1.23 s |", text)
            self.assertIn("worker-02** · unavailable", text)
            self.assertTrue((directory / "first.md").exists())


if __name__ == "__main__":
    unittest.main()
