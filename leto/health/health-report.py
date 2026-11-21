#!/usr/bin/env python3
"""Report the health of a local Leto build host."""

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import stat
import subprocess
import time
import tomllib


GIB = 1024 ** 3
CLASSES = ("new", "own-input", "dependency", "unchanged")
LOW_PRIORITY = ["nice", "-n", "19", "ionice", "-c3"]
PROJECT_PATH = re.compile(r"/(?:\.t3/worktrees|workspaces/leto-labs)/([^/]+)(?=/|$)")
GENERATED_SOURCE = re.compile(r"(?:^|/)(?:OUT_DIR(?:/|$)|build/[^/]+/out(?:/|$))")
CACHE_KEY = re.compile(r"[0-9a-f]{64}")


def command(args, *, timeout=10, env=None, input=None):
    return subprocess.run(args, check=True, capture_output=True, timeout=timeout,
                          env=env, input=input).stdout


def timestamp(value):
    if isinstance(value, str):
        try:
            result = datetime.fromisoformat(value)
            if result.tzinfo is not None:
                return result.astimezone(timezone.utc)
        except ValueError:
            pass
    return None


def byte_count(value):
    match = re.fullmatch(r"(\d+)([KMGT]?)", str(value).upper())
    if not match:
        raise ValueError("expected bytes or an integer with a K, M, G or T suffix")
    number, suffix = match.groups()
    return int(number) * 1024 ** (" KMGT".index(suffix) if suffix else 0)


def top(counts, limit=10):
    return dict(sorted(counts.items(), key=lambda item: (-item[1], item[0]))[:limit])


def load_config(args, warnings):
    try:
        config = tomllib.loads(args.config.read_text())
    except (OSError, ValueError) as error:
        warnings.append(f"Cannot read sccache config: {error}")
        config = {}
    cache = config.get("cache", {})
    disk = cache.get("disk", {}) if isinstance(cache, dict) else {}
    disk = disk if isinstance(disk, dict) else {}
    server = config.get("server", {})
    server = server if isinstance(server, dict) else {}
    try:
        cap = args.cache_cap if args.cache_cap is not None else byte_count(disk.get("size", 10 * GIB))
    except ValueError as error:
        warnings.append(f"Invalid disk cache size in {args.config}: {error}")
        cap = None
    cache_path = configured_path(disk.get("dir"), args.default_cache_dir, "cache.disk.dir", warnings)
    miss_log = configured_path(config.get("miss_explain"), args.default_miss_log, "miss_explain", warnings)
    linked_max = config.get("rust_cache_linked_max_entry_bytes", 256 * 1024 ** 2)
    if type(linked_max) is not int or linked_max < 0:
        warnings.append("Invalid rust_cache_linked_max_entry_bytes in sccache config")
        linked_max = None
    return {
        "path": str(args.config),
        "cache_dir": str(args.cache_dir or cache_path),
        "cache_cap_bytes": cap,
        "reflink": disk.get("reflink", False),
        "rust_cache_linked": config.get("rust_cache_linked", "off"),
        "rust_cache_linked_max_entry_bytes": linked_max,
        "miss_explain": str(args.miss_log or miss_log),
        "autostart": server.get("autostart", True),
        "shutdown_drain_secs": server.get("shutdown_drain_secs", 10),
        "connect_wait_secs": server.get("connect_wait_secs", 30),
    }


def configured_path(value, default, key, warnings):
    if isinstance(value, str) and value:
        return Path(value).expanduser()
    if value is not None:
        warnings.append(f"Invalid {key} path in sccache config")
    return default


def client_env(port):
    # The offline stats fallback constructs a local cache from the client config.
    # Give it an empty config and a path that cannot be created.
    env = {key: value for key, value in os.environ.items() if not key.startswith("SCCACHE_")}
    return dict(env, SCCACHE_CONF="/dev/null", SCCACHE_DIR="/dev/null/leto-build-health",
                SCCACHE_SERVER_PORT=str(port), SCCACHE_SERVER_AUTOSTART="0")


def count_map(value):
    if not isinstance(value, dict):
        return {}
    return {key: count for key, count in value.items() if type(count) is int and count >= 0}


def language_counts(stats, key):
    counter = stats.get(key)
    return count_map(counter.get("counts")) if isinstance(counter, dict) else {}


def hit_rate(hits, misses):
    return round(100 * hits / (hits + misses), 2) if hits + misses else None


def server_report(args, now, warnings):
    env = client_env(args.port)
    server = {"available": False, "binary": str(args.binary), "port": args.port, "version": None,
              "binary_version": None, "sha256": None, "unit": args.unit, "unit_state": "unknown",
              "active_enter_timestamp": None, "main_pid": None, "uptime_seconds": None,
              "rss_bytes": None, "stats": None}
    try:
        with args.binary.open("rb") as binary:
            server["sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
        server["binary_version"] = command([args.binary, "--version"], env=env).decode().strip()
        server["version"] = server["binary_version"]
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        warnings.append(f"Cannot inspect sccache binary: {error}")

    try:
        fields = dict(line.split("=", 1) for line in command(
            ["systemctl", "--user", "show", args.unit,
             "-p", "ActiveState,ActiveEnterTimestamp,MainPID"],
            env=dict(os.environ, LC_ALL="C", TZ="UTC"),
        ).decode().splitlines() if "=" in line)
        server["unit_state"] = fields.get("ActiveState", "unknown")
        active_since = fields.get("ActiveEnterTimestamp")
        if active_since and active_since != "n/a":
            # With TZ=UTC systemctl prints e.g. Sat 2026-11-21 02:33:13 UTC.
            entered = datetime.strptime(active_since, "%a %Y-%m-%d %H:%M:%S %Z").replace(tzinfo=timezone.utc)
            server["active_enter_timestamp"] = entered.isoformat()
            if server["unit_state"] == "active":
                server["uptime_seconds"] = max(0, int((now - entered).total_seconds()))
        pid = int(fields.get("MainPID", "0"))
        if pid > 0:
            server["main_pid"] = pid
            status = (args.proc_root / str(pid) / "status").read_text()
            rss = re.search(r"^VmRSS:\s+(\d+)\s+kB$", status, re.MULTILINE)
            if rss:
                server["rss_bytes"] = int(rss[1]) * 1024
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        warnings.append(f"Cannot inspect sccache unit or process: {error}")
    if server["unit_state"] != "active":
        warnings.append(f"Systemd unit {args.unit} is {server['unit_state']}")
    if (server["rss_bytes"] or 0) > 8 * GIB:
        warnings.append("Server RSS exceeds 8 GiB")

    info = {}
    try:
        raw = (args.stats_json.read_bytes() if args.stats_json else
               command([args.binary, "--show-stats", "--stats-format=json"], env=env))
        info = json.loads(raw)
        if not isinstance(info, dict) or not isinstance(info.get("stats"), dict):
            raise ValueError("expected an sccache JSON stats object")
        if info.get("effective_cache_mode") is None:
            raise ValueError("client returned offline stats")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        warnings.append(f"Server down or unreachable on port {args.port}: {error}")
        return server, {}

    stats = info["stats"]
    hits = language_counts(stats, "cache_hits")
    misses = language_counts(stats, "cache_misses")
    rust_hits = sum(count for lang, count in hits.items() if lang.lower() == "rust")
    rust_misses = sum(count for lang, count in misses.items() if lang.lower() == "rust")
    server.update(available=True, version=info.get("version") or server["binary_version"], stats={
        "requests": stats.get("compile_requests"),
        "executed": stats.get("requests_executed"),
        "hits": hits,
        "misses": misses,
        "hit_rate_pct": {"rust": hit_rate(rust_hits, rust_misses),
                         "overall": hit_rate(sum(hits.values()), sum(misses.values()))},
        "not_cached": top(count_map(stats.get("not_cached"))),
        "cache_errors": language_counts(stats, "cache_errors"),
        "cache_write_errors": stats.get("cache_write_errors"),
        "cache_read_errors": stats.get("cache_read_errors"),
        "rust_incremental_not_stored": stats.get("rust_incremental_not_stored"),
        "rust_linked_entry_too_large": stats.get("rust_linked_entry_too_large"),
        "miss_explain": count_map(stats.get("miss_explain")),
    })
    return server, info


def du(args, *, timeout=60, input=None):
    process = subprocess.run([*LOW_PRIORITY, "du", "-s", "-B1", "-x", "-0", *args],
                             capture_output=True, timeout=timeout, input=input,
                             env=dict(os.environ, LC_ALL="C"))
    if process.returncode:
        # Cache eviction or incremental cleanup may unlink files during the walk.
        errors = process.stderr.splitlines()
        if not (process.stdout and errors and
                all(line.endswith(b": No such file or directory") for line in errors)):
            detail = process.stderr.decode(errors="replace").splitlines()
            raise OSError(detail[0] if detail else f"du exited with status {process.returncode}")
    return process.stdout


def count_entries(path):
    # Disk entries use <key[0]>/<key[1]>/<key> (src/cache/disk.rs).
    deadline = time.monotonic() + 2
    total = 0
    try:
        for first in read_dirs(path):
            if first.name not in "0123456789abcdef" or len(first.name) != 1:
                continue
            for second in read_dirs(first):
                if second.name not in "0123456789abcdef" or len(second.name) != 1:
                    continue
                if time.monotonic() >= deadline:
                    return None
                prefix = first.name + second.name
                with os.scandir(second) as entries:
                    for entry in entries:
                        if (entry.name.startswith(prefix) and CACHE_KEY.fullmatch(entry.name)
                                and entry.is_file(follow_symlinks=False)):
                            total += 1
    except OSError:
        return None
    return total if path.is_dir() and time.monotonic() < deadline else None


def cache_report(config, info, warnings):
    path = Path(config["cache_dir"]).absolute()
    tracked = info.get("cache_size")
    cap = info.get("max_cache_size") or config["cache_cap_bytes"]
    cache = {"dir": str(path), "cap_bytes": cap, "server_size_bytes": tracked,
             "apparent_bytes": None, "allocated_bytes": None,
             "btrfs_extent_bytes": None, "entry_count": None, "fill_pct": None,
             "fill_basis": None}
    for key, flags in (("allocated_bytes", []), ("apparent_bytes", ["--apparent-size"])):
        try:
            cache[key] = int(du([*flags, "-H", "--", path]).split(b"\t", 1)[0])
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            warnings.append(f"Cannot measure cache {key}: {error}")

    try:
        if shutil.which("compsize") and command(["stat", "-f", "-c", "%T", "--", path]).strip() == b"btrfs":
            output = command(["timeout", "-k", "1s", "58s", *LOW_PRIORITY,
                              "sudo", "-n", "compsize", "-b", "-x", path], timeout=60)
            total = next((line.split() for line in output.splitlines() if line.startswith(b"TOTAL")), [])
            if len(total) >= 5:
                cache["btrfs_extent_bytes"] = dict(zip(
                    ("disk_usage", "uncompressed", "referenced"), map(int, total[2:5])))
    except (OSError, ValueError, subprocess.SubprocessError):
        pass  # compsize is optional, including on hosts without passwordless sudo.
    cache["entry_count"] = count_entries(path)

    for basis in ("server_size_bytes", "allocated_bytes", "apparent_bytes"):
        used = cache[basis]
        if type(used) is int and type(cap) is int and cap > 0:
            cache["fill_basis"] = basis
            cache["fill_pct"] = round(100 * used / cap, 2)
            if 100 * used > 95 * cap:
                warnings.append(f"Cache exceeds 95% of cap ({cache['fill_pct']:.2f}%)")
            break
    return cache


def project_for(cwd):
    match = PROJECT_PATH.search(cwd)
    return match[1] if match else "(unattributed)"


def component_kind(name, event):
    if name.startswith("source:"):
        path = name.removeprefix("source:")
        if GENERATED_SOURCE.search(path):
            return "source:OUT_DIR"
        checkout = event.get("basedir")
        if not isinstance(checkout, str) or not checkout:
            cwd = event["cwd"]
            project = PROJECT_PATH.search(cwd)
            checkout = cwd[:project.end()] if project else cwd
            if project and "/.t3/worktrees/" in checkout:
                worktree = cwd[project.end():].split("/", 2)
                if len(worktree) > 1:
                    checkout += "/" + worktree[1]
        if not path.startswith("/") or path.startswith(checkout.rstrip("/") + "/"):
            return "source:checkout"
        return "source:absolute"
    if name.startswith("arg:"):
        # --cfg records its full value, whereas -C/-Z already name the option.
        return "arg:--cfg" if name.startswith(("arg:--cfg ", "arg:--cfg=")) else name
    if name == "arguments":
        return "arg:(legacy)"
    if name.startswith("compiler"):
        return "compiler"
    if name.startswith("staticlib:"):
        return "staticlib"
    return name


def miss_log_report(path, now, hours, warnings):
    try:
        since = now - timedelta(hours=hours)
    except OverflowError:
        since = datetime.min.replace(tzinfo=timezone.utc)
    projects = defaultdict(Counter)
    components = defaultdict(Counter)
    crates = defaultdict(Counter)
    crate_components = defaultdict(Counter)
    externs = defaultdict(Counter)
    files = {}
    seen = set()
    for filename in (path, path.with_name(path.name + ".1")):
        meta = {"path": str(filename), "size_bytes": None, "line_count": None,
                "oldest_timestamp": None, "newest_timestamp": None, "invalid_lines": 0}
        files["current" if filename == path else "previous"] = meta
        try:
            with filename.open("rb") as log:
                info = os.fstat(log.fileno())
                identity = (info.st_dev, info.st_ino)
                if identity in seen:
                    continue
                seen.add(identity)
                remaining = meta["size_bytes"] = info.st_size
                line_count = invalid = 0
                oldest = newest = None
                while remaining and (line := log.readline(remaining)):
                    remaining -= len(line)
                    # Exclude an append that had not finished when the file size was sampled.
                    if not line.endswith(b"\n"):
                        break
                    line_count += 1
                    try:
                        event = json.loads(line)
                        observed = timestamp(event["time"])
                        cwd, crate, classification = (event[key] for key in ("cwd", "crate_name", "classification"))
                        changes = event["differences"]
                        if (observed is None or not isinstance(cwd, str) or not isinstance(crate, str)
                                or not isinstance(classification, str) or not isinstance(changes, list)):
                            raise ValueError("invalid miss event")
                        names = {change["name"] for change in changes}
                        if any(not isinstance(name, str) for name in names):
                            raise ValueError("invalid component name")
                    except (ValueError, KeyError, TypeError):
                        invalid += 1
                        continue
                    oldest = observed if oldest is None else min(oldest, observed)
                    newest = observed if newest is None else max(newest, observed)
                    if not since <= observed <= now:
                        continue
                    project = project_for(cwd)
                    projects[project][classification] += 1
                    if classification == "own-input":
                        kinds = {component_kind(name, event) for name in names if not name.startswith("extern:")}
                        components[project].update(kinds)
                        crates[project][crate] += 1
                        crate_components[project, crate].update(kinds)
                    if classification == "dependency":
                        externs[project].update(name.removeprefix("extern:") for name in names
                                                if name.startswith("extern:"))
                meta.update(line_count=line_count, invalid_lines=invalid,
                            oldest_timestamp=oldest.isoformat() if oldest else None,
                            newest_timestamp=newest.isoformat() if newest else None)
                if invalid:
                    warnings.append(f"Miss log {filename.name} contains {invalid} invalid JSON lines")
        except FileNotFoundError:
            if filename == path and not path.with_name(path.name + ".1").is_file():
                warnings.append(f"Miss log missing: {path}")
        except OSError as error:
            warnings.append(f"Cannot read miss log {filename}: {error}")
    if (files["current"]["size_bytes"] or 0) > 500_000_000:
        warnings.append("Miss log exceeds 500 MB")

    by_project = {
        project: {
            "total": sum(counts.values()),
            "classifications": {**dict.fromkeys(CLASSES, 0), **counts},
            "unchanged": counts["unchanged"],
            "own_input_components": top(components[project]),
            "own_input_crates": [
                {"crate": crate, "count": count,
                 "main_component": next(iter(top(crate_components[project, crate], 1)), None)}
                for crate, count in top(crates[project]).items()
            ],
            "dependency_externs": top(externs[project]),
        } for project, counts in sorted(projects.items())
    }
    classifications = Counter()
    for counts in projects.values():
        classifications.update(counts)
    return {"hours": hours, "since": since.isoformat(), "total": sum(classifications.values()),
            "classifications": {**dict.fromkeys(CLASSES, 0), **classifications},
            "projects": by_project}, files


def read_prewarm_config(path):
    settings = {}
    for line in path.read_text().splitlines():
        key, separator, value = line.strip().removeprefix("export ").partition("=")
        if separator and key.strip() in ("PREWARM_REPO", "PREWARM_BRANCH"):
            words = shlex.split(value, comments=True)
            if len(words) != 1:
                raise ValueError(f"expected one value for {key.strip()}")
            settings[key.strip()] = words[0]
    return settings


def read_dirs(path):
    try:
        with os.scandir(path) as entries:
            return [Path(entry.path) for entry in entries if entry.is_dir(follow_symlinks=False)]
    except FileNotFoundError:
        return []


def prewarm_report(args, now, warnings):
    projects = {}
    try:
        names = {path.name for path in read_dirs(args.prewarm_state)}
        names.update(path.stem for path in args.prewarm_config.glob("*.conf"))
    except OSError as error:
        warnings.append(f"Cannot list pre-warm projects: {error}")
        return projects
    for name in sorted(names):
        state = args.prewarm_state / name
        project = {"result": None, "last_warmed_commit": None, "branch": None,
                   "lag_commits": None, "lag_minutes": None, "duration_seconds": None,
                   "paused_seconds": None, "disk": None}
        projects[name] = project
        try:
            status = json.loads((state / "status.json").read_bytes())
            if not isinstance(status, dict):
                raise ValueError("expected a JSON object")
            project.update({key: status.get(key) for key in ("result", "last_warmed_commit",
                            "duration_seconds", "paused_seconds", "disk")})
        except (OSError, ValueError) as error:
            warnings.append(f"Cannot read pre-warm status for {name}: {error}")
            continue

        warmed = status.get("last_warmed_commit")
        if not isinstance(warmed, str) or not re.fullmatch(r"[a-fA-F0-9]{7,64}", warmed):
            warnings.append(f"No valid last warmed commit for {name}")
            continue
        last_warmed_at = timestamp(status.get("finished_at")) if status.get("result") == "warmed" else None
        try:
            with (state / "last-warmed").open() as marker:
                if marker.read().strip() == warmed:
                    modified = os.fstat(marker.fileno()).st_mtime
                    last_warmed_at = datetime.fromtimestamp(modified, timezone.utc)
        except FileNotFoundError:
            pass
        except OSError as error:
            warnings.append(f"Cannot read last warmed marker for {name}: {error}")
        if last_warmed_at:
            minutes = max(0, (now - last_warmed_at).total_seconds() / 60)
            project["lag_minutes"] = round(minutes, 1)
            if minutes > 180:
                warnings.append(f"Pre-warm {name} last succeeded over 3 hours ago ({minutes:.0f} min)")

        try:
            settings = read_prewarm_config(args.prewarm_config / f"{name}.conf")
            repo = Path(settings["PREWARM_REPO"]).expanduser()
            project["branch"] = branch = settings.get("PREWARM_BRANCH", "main")
            env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
            revision = f"{warmed}..refs/heads/{branch}"
            count = command(["git", "-C", repo, "rev-list", "--count", "--end-of-options", revision, "--"],
                            env=dict(env, GIT_OPTIONAL_LOCKS="0"))
            project["lag_commits"] = int(count)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            warnings.append(f"Cannot determine pre-warm branch lag for {name}: {error}")
        if project["lag_commits"] is not None and project["lag_commits"] > 10:
            warnings.append(f"Pre-warm {name} is {project['lag_commits']} commits behind")
    return projects


def incremental_dirs(target):
    for child in read_dirs(target):
        direct = child / "incremental"
        if direct.is_dir() and not direct.is_symlink():
            yield direct
        else:
            # Custom target triples can place a profile one level deeper.
            for profile in read_dirs(child):
                nested = profile / "incremental"
                if nested.is_dir() and not nested.is_symlink():
                    yield nested


def incremental_size(target, deadline):
    sizes = Counter()
    for directory in incremental_dirs(target):
        sessions = read_dirs(directory)
        if not sessions:
            continue
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("incremental scan exceeded its time budget")
        names = b"".join(os.fsencode(session) + b"\0" for session in sessions)
        output = du(["--files0-from=-"], timeout=min(40, remaining), input=names)
        for record in output.split(b"\0"):
            if record:
                size, filename = record.split(b"\t", 1)
                kind = "sccache" if "-sccache-" in Path(os.fsdecode(filename)).name else "other"
                sizes[kind] += int(size)
    return sizes


def incremental_report(args, warnings):
    available = defaultdict(list)
    canonical = {}
    total_worktrees = 0
    try:
        for project in read_dirs(args.worktrees):
            for checkout in read_dirs(project):
                total_worktrees += 1
                target = checkout / "target"
                try:
                    info = target.lstat()
                    if stat.S_ISDIR(info.st_mode):
                        mtime = max(checkout.stat().st_mtime_ns, info.st_mtime_ns)
                        available[project.name].append((mtime, target))
                except FileNotFoundError:
                    continue
        for checkout in read_dirs(args.checkouts):
            target = checkout / "target"
            if target.is_dir() and not target.is_symlink():
                canonical[checkout.name] = target
    except OSError as error:
        warnings.append(f"Cannot list build targets: {error}")

    fast = total_worktrees > 100 if args.fast is None else args.fast
    targets = sorted((mtime, target, project) for project, entries in available.items()
                     for mtime, target in entries)
    selected = defaultdict(list)
    for _, target, project in reversed(targets[-20:] if fast else targets):
        selected[project].append(target)

    deadline = time.monotonic() + 90
    failed = []
    report = {}
    for project in sorted(available.keys() | canonical.keys()):
        sizes = Counter()
        worktrees = len(available[project])
        canonical_size = Counter()
        scanned = 0
        canonical_scanned = project not in canonical
        for target in ([canonical[project]] if project in canonical else []) + selected[project]:
            try:
                if time.monotonic() >= deadline:
                    raise TimeoutError("incremental scan exceeded its time budget")
                measured = incremental_size(target, deadline)
                if target == canonical.get(project):
                    canonical_size = measured
                    canonical_scanned = True
                else:
                    sizes.update(measured)
                    scanned += 1
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                failed.append(f"{target}: {error}")
        complete = canonical_scanned and scanned == len(selected[project]) and (scanned > 0 or not worktrees)
        report[project] = {
            "worktrees_with_target": worktrees,
            "worktrees_scanned": scanned,
            "canonical_scanned": project in canonical and canonical_scanned,
            "canonical_sccache_bytes": canonical_size["sccache"] if canonical_scanned and project in canonical else None,
            "canonical_other_bytes": canonical_size["other"] if canonical_scanned and project in canonical else None,
            "estimated": fast and scanned < worktrees,
            "sccache_bytes": round(sizes["sccache"] * worktrees / max(scanned, 1)) + canonical_size["sccache"]
            if complete else None,
            "other_bytes": round(sizes["other"] * worktrees / max(scanned, 1)) + canonical_size["other"]
            if complete else None,
        }
    if failed:
        warnings.append(f"Incremental disk scan skipped {len(failed)} targets (first: {failed[0]})")
    return {"fast": fast, "worktrees_found": total_worktrees, "projects": report}


def rotate_misses(path):
    # Serialize rotators on the directory. The server opens the path for each
    # append and any write already in flight keeps its original inode.
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.flock(directory, fcntl.LOCK_EX)
        try:
            info = path.lstat()
        except FileNotFoundError:
            return
        if not stat.S_ISREG(info.st_mode):
            raise ValueError(f"refusing to rotate a non-regular file: {path}")
        if not info.st_size:
            return
        path.replace(path.with_name(path.name + ".1"))
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, stat.S_IMODE(info.st_mode))
        except FileExistsError:
            # The next server append has already recreated the current log.
            return
        try:
            os.fchmod(fd, stat.S_IMODE(info.st_mode))
        finally:
            os.close(fd)
    finally:
        os.close(directory)


def make_report(args, *, now=None):
    started = time.monotonic()
    now = now or datetime.now(timezone.utc)
    warnings = []
    config = load_config(args, warnings)
    server, info = server_report(args, now, warnings)
    cache = cache_report(config, info, warnings)
    misses, log = miss_log_report(Path(config["miss_explain"]), now, args.hours, warnings)
    prewarm = prewarm_report(args, now, warnings)
    incremental = incremental_report(args, warnings)
    return {"schema": "leto-build-health/1", "host": socket.gethostname(),
            "generated_at": now.isoformat(), "runtime_seconds": round(time.monotonic() - started, 3),
            "config": config, "server": server, "cache": cache, "misses": misses,
            "miss_log": log, "prewarm": prewarm, "incremental": incremental, "warnings": warnings}


def display(value, suffix=""):
    return "—" if value is None else f"{value}{suffix}"


def gib(value):
    return display(round(value / GIB, 2) if value is not None else None, " GiB")


def md_cell(value):
    return display(value).replace("|", "\\|").replace("\r", " ").replace("\n", " ")


def table(headers, rows):
    def row(values):
        return "| " + " | ".join(md_cell(value) for value in values) + " |\n"

    return row(headers) + row(["---"] * len(headers)) + "".join(row(values) for values in rows)


def summarize(counts):
    return ", ".join(f"{name} ({count})" for name, count in counts.items()) or None


def markdown(report):
    server, cache, config = (report[key] for key in ("server", "cache", "config"))
    stats = server["stats"] or {}
    rates = stats.get("hit_rate_pct", {})
    misses = report["misses"]
    prewarm = report["prewarm"]
    incremental = report["incremental"]
    sections = [f"# Build health · {report['host']}",
                f"{report['generated_at']} · {report['runtime_seconds']} s",
                "## Server",
                table(["Unit / PID", "Version", "Binary SHA-256", "Uptime", "RSS"], [[
                    f"{server['unit_state']} / {display(server['main_pid'])}", server["version"], server["sha256"],
                    display(server["uptime_seconds"], " s"), gib(server["rss_bytes"])]])
                + "\n" + table(["Requests", "Executed", "Hits", "Misses", "Rust hit rate", "Overall hit rate"], [[
                    stats.get("requests"), stats.get("executed"), summarize(stats.get("hits", {})),
                    summarize(stats.get("misses", {})), display(rates.get("rust"), "%"),
                    display(rates.get("overall"), "%")]])
                + "\n" + table(["Non-cacheable", "Cache errors", "Read / write errors",
                                 "Incremental / linked entries skipped", "Miss explain"], [[
                    summarize(stats.get("not_cached", {})), summarize(stats.get("cache_errors", {})),
                    f"{display(stats.get('cache_read_errors'))} / {display(stats.get('cache_write_errors'))}",
                    f"{display(stats.get('rust_incremental_not_stored'))} / "
                    f"{display(stats.get('rust_linked_entry_too_large'))}", summarize(stats.get("miss_explain", {}))]]),
                "## Cache",
                table(["Directory", "Cap", "Apparent", "Allocated", "btrfs extents", "Entries", "Fill", "Reflink"], [[
                    cache["dir"], gib(cache["cap_bytes"]), gib(cache["apparent_bytes"]), gib(cache["allocated_bytes"]),
                    gib((cache["btrfs_extent_bytes"] or {}).get("disk_usage")), cache["entry_count"],
                    display(cache["fill_pct"], "%"), config["reflink"]]]) + "\n" + table(
                    ["Linked stage", "Linked entry cap", "Shutdown drain", "Autostart", "Connect wait"], [[
                        config["rust_cache_linked"], gib(config["rust_cache_linked_max_entry_bytes"]),
                        display(config["shutdown_drain_secs"], " s"), config["autostart"],
                        display(config["connect_wait_secs"], " s")]]),
                f"## Misses · last {misses['hours']:g} h",
                table(["Project", "Total", "Classifications", "Own inputs", "Own-input crates", "Dependency externs"], [
                    [name, project["total"], summarize(project["classifications"]),
                     summarize(project["own_input_components"]), ", ".join(
                         f"{entry['crate']} ({entry['count']}, {display(entry['main_component'])})"
                         for entry in project["own_input_crates"]) or None, summarize(project["dependency_externs"])]
                    for name, project in misses["projects"].items()
                ]),
                "## Miss log",
                table(["File", "Bytes", "Lines", "Oldest", "Newest"], [
                    [entry[key] for key in ("path", "size_bytes", "line_count", "oldest_timestamp", "newest_timestamp")]
                    for entry in report["miss_log"].values()
                ]),
                "## Pre-warm",
                table(["Project", "Result", "Warmed commit", "Commits behind", "Age (min)", "Duration (s)",
                       "Paused (s)", "Checkout (allocated)"], [
                    [name, entry["result"], entry["last_warmed_commit"], entry["lag_commits"], entry["lag_minutes"],
                     entry["duration_seconds"], entry["paused_seconds"],
                     gib(((entry["disk"] or {}).get("checkout") or {}).get("allocated_bytes"))]
                    for name, entry in prewarm.items()
                ]),
                "## Incremental sessions",
                table(["Project", "Worktrees scanned / with targets", "Canonical scanned",
                       "Private sccache", "Other", "Estimated"], [
                    [name, f"{entry['worktrees_scanned']} / {entry['worktrees_with_target']}",
                     entry["canonical_scanned"], gib(entry["sccache_bytes"]), gib(entry["other_bytes"]),
                     entry["estimated"]] for name, entry in incremental["projects"].items()
                ])]
    if report["warnings"]:
        sections.extend(["## Warnings", "\n".join(f"- {md_cell(warning)}" for warning in report["warnings"])])
    return "\n\n".join(section.rstrip() for section in sections) + "\n"


def positive_hours(text):
    value = float(text)
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError("hours must be a positive, finite duration")
    return value


def parse_args(argv=None):
    home = Path.home()
    cache = Path(os.getenv("XDG_CACHE_HOME") or home / ".cache")
    config = Path(os.getenv("XDG_CONFIG_HOME") or home / ".config")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    parser.add_argument("--out", type=Path, help="write to FILE instead of stdout")
    parser.add_argument("--hours", type=positive_hours, default=24)
    parser.add_argument("--fast", action=argparse.BooleanOptionalAction,
                        help="sample 20 worktree targets (automatic above 100 worktrees)")
    parser.add_argument("--rotate", action="store_true", help="rotate only the miss log, keeping one previous file")
    parser.add_argument("--config", type=Path, default=config / "sccache/config")
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--cache-cap", type=byte_count, help="override the configured cap when offline")
    parser.add_argument("--miss-log", type=Path)
    parser.add_argument("--binary", type=Path, default=home / ".cargo/bin/sccache")
    parser.add_argument("--port", type=int, default=4226)
    parser.add_argument("--stats-json", type=Path, help="read a captured stats response instead of querying sccache")
    parser.add_argument("--unit", default="sccache")
    parser.add_argument("--proc-root", type=Path, default=Path("/proc"))
    parser.add_argument("--prewarm-state", type=Path, default=cache / "leto-maintenance/prewarm")
    parser.add_argument("--prewarm-config", type=Path, default=config / "leto-prewarm")
    parser.add_argument("--worktrees", type=Path, default=home / ".t3/worktrees")
    parser.add_argument("--checkouts", type=Path, default=home / "workspaces/leto-labs")
    parser.set_defaults(default_cache_dir=cache / "sccache", default_miss_log=cache / "sccache/explain/misses.jsonl")
    args = parser.parse_args(argv)
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    for key, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, key, value.expanduser())
    return args


def main():
    args = parse_args()
    if socket.gethostname().split(".")[0] == "devbox":
        os.sched_setaffinity(0, os.sched_getaffinity(0) - set(range(16, 32)))
    os.nice(19)
    if args.rotate:
        path = args.miss_log
        if path is None:
            warnings = []
            config = load_config(args, warnings)
            if warnings:
                # A config parse failure leaves the live log location uncertain.
                raise SystemExit("\n".join(warnings))
            path = Path(config["miss_explain"])
        if path.parent.is_dir():
            rotate_misses(path)
        return
    report = make_report(args)
    output = markdown(report) if args.format == "markdown" else json.dumps(report, separators=(",", ":")) + "\n"
    if args.out:
        args.out.write_text(output)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
