#!/usr/bin/env python3
"""Collect devbox and worker build-health reports and publish a fleet snapshot."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory


HOSTS = ("devbox", "worker-01", "worker-02")
SCRIPT = Path(__file__).with_name("health-report.py")


def collect(host, script, timeout, deep=False):
    mode = ["--deep"] if deep else []
    if host == "devbox":
        args = [sys.executable, str(SCRIPT), "--format", "json", *mode]
        stdin = None
    else:
        args = ["tailscale", "ssh", f"leovigna@{host}", " ".join(["python3 - --format json", *mode])]
        stdin = script
    try:
        process = subprocess.run(args, input=stdin, capture_output=True, check=True, timeout=timeout)
        report = json.loads(process.stdout)
        if not isinstance(report, dict) or report.get("schema") != "leto-build-health/1":
            raise ValueError("unexpected host report schema")
        return report
    except subprocess.CalledProcessError as error:
        detail = error.stderr.decode(errors="replace").strip()[-500:] or f"exit {error.returncode}"
    except subprocess.TimeoutExpired:
        detail = f"timed out after {timeout} s"
    except (OSError, ValueError) as error:
        detail = str(error)
    return {"host": host, "runtime_seconds": None, "warnings": [f"Collection failed: {detail}"]}


def with_suffix(value, suffix=""):
    return f"{value}{suffix}" if value is not None else "—"


def cache_cell(cache):
    if cache.get("disk_pct") is not None:
        return f"{cache['disk_pct']}% on disk"
    free = cache.get("fs_free_bytes")
    tracked = with_suffix(cache.get("fill_pct"), "% tracked")
    return f"{tracked}, {free / 2**30:.0f} GiB free" if free is not None else tracked


def headline(report):
    if report.get("skipped"):
        return [f"skipped: {report['skipped']}", *["—"] * 6, str(len(report["warnings"])), "—"]
    stats = report.get("server", {}).get("stats") or {}
    misses = report.get("misses", {})
    counts = misses.get("classifications", {})
    cause = max(counts, key=counts.get) if any(counts.values()) else None
    lags = ", ".join(
        f"{project} ({with_suffix(values['lag_commits'], ' commits')}, {with_suffix(values['lag_minutes'], ' min')})"
        for project, values in report.get("prewarm", {}).items()
    )
    return [
        with_suffix(stats.get("hit_rate_pct", {}).get("rust"), "%"),
        with_suffix(misses.get("total")),
        f"{cause} ({counts[cause]})" if cause else "—",
        cache_cell(report.get("cache") or {}),
        with_suffix(round(((report.get("cache") or {}).get("written_last_hour") or {}).get("bytes", 0) / 2**30, 1)
                    if (report.get("cache") or {}).get("written_last_hour") else None, " GiB"),
        with_suffix(report.get("server", {}).get("disk_write_gib_per_hour"), " GiB/h"),
        lags or "—",
        str(len(report["warnings"])),
        with_suffix(report["runtime_seconds"], " s"),
    ]


def markdown(fleet):
    lines = ["# Fleet build health", "", fleet["generated_at"], "",
             "| Host | Rust hit rate | Misses (log tail, ≤24 h) | Top miss cause | Cache | Cache written, last hour | Disk writes incl. compiles | Pre-warm lag | Warnings | Runtime |",
             "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    warnings = []
    for host, report in fleet["hosts"].items():
        cells = [host, *headline(report)]
        lines.append("| " + " | ".join(cell.replace("|", "\\|") for cell in cells) + " |")
        for warning in report["warnings"]:
            warnings.append(f"- **{host}** · " + " ".join(warning.splitlines()))
    if warnings:
        lines.extend(["", "## Warnings", "", *warnings])
    return "\n".join(lines) + "\n"


def publish(fleet, directory, stamp):
    directory.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=directory, prefix=".collect-") as staging:
        staging = Path(staging)
        files = {"json": json.dumps(fleet, separators=(",", ":")) + "\n", "md": markdown(fleet)}
        for extension, content in files.items():
            (staging / f"{stamp}.{extension}").write_text(content)
            (staging / f"latest.{extension}").symlink_to(f"{stamp}.{extension}")
        lock = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for extension in files:
                snapshot = f"{stamp}.{extension}"
                (staging / snapshot).replace(directory / snapshot)
            for extension in files:
                latest = f"latest.{extension}"
                (staging / latest).replace(directory / latest)
        finally:
            os.close(lock)
    return directory / f"{stamp}.md"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, default=Path.home() / "data/core/build-health")
    parser.add_argument("--deep", action="store_true",
                        help="ask hosts for du/compsize scans too (at most daily, at low load)")
    parser.add_argument("--timeout", type=int, help="seconds per host (default: 30, or 900 with --deep)")
    args = parser.parse_args()
    args.timeout = args.timeout or (900 if args.deep else 30)
    if args.timeout < 1:
        parser.error("--timeout must be positive")
    if os.uname().nodename.split(".")[0] == "devbox":
        os.sched_setaffinity(0, os.sched_getaffinity(0) - set(range(16, 32)))
    script = SCRIPT.read_bytes()
    with ThreadPoolExecutor(max_workers=len(HOSTS)) as executor:
        pending = {host: executor.submit(collect, host, script, args.timeout, args.deep) for host in HOSTS}
        reports = {host: task.result() for host, task in pending.items()}
    now = datetime.now(timezone.utc)
    fleet = {"schema": "leto-build-health-fleet/1", "generated_at": now.isoformat(), "hosts": reports}
    output = publish(fleet, args.out_dir.expanduser(), now.strftime("%Y%m%dT%H%M%S.%fZ"))
    print(output)


if __name__ == "__main__":
    main()
