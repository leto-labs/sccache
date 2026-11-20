#!/usr/bin/env python3
import argparse
from datetime import datetime, timezone
import fcntl
import json
import math
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
import tomllib


DEFAULT_COMMANDS = "check --workspace --all-targets;test --workspace --no-run"
CARGO = ["bash", "scripts/cargo.sh"]


class PrewarmError(Exception):
    pass


def timestamp():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def log_event(log, message):
    line = f"{timestamp()} {message}"
    print(line, flush=True)
    print(line, file=log, flush=True)


def read_config(path):
    config = {}
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        key = key.removeprefix("export ").strip()
        words = shlex.split(value, comments=True)
        if not separator or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) or len(words) > 1:
            raise PrewarmError(f"{path}:{number}: expected NAME=value (quote values containing spaces)")
        config[key] = words[0] if words else ""
    return config


def capture(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs)


def git(cwd, *args, log):
    return capture(["git", "-C", str(cwd), *args], stderr=log).strip()


def validate_checkout(checkout):
    if checkout.is_symlink() or (checkout.exists() and (checkout / ".git").is_symlink()):
        raise PrewarmError(f"refusing to use a symlink as the pre-warm checkout: {checkout}")
    if checkout.exists() and not (checkout / ".git").is_dir():
        raise PrewarmError(f"expected a dedicated clone at {checkout}")


def prepare_checkout(repo, checkout, ref, log):
    if not checkout.exists():
        git(checkout.parent, "clone", "--shared", "--no-checkout", "--no-tags", "--single-branch",
            "--branch", ref.removeprefix("refs/heads/"), "--", str(repo), str(checkout), log=log)
    if git(checkout, "remote", "get-url", "origin", log=log) != str(repo):
        raise PrewarmError(f"{checkout} was cloned from a different canonical repo")
    git(checkout, "fetch", "--no-tags", "--no-auto-maintenance", "--no-recurse-submodules",
        "origin", f"+{ref}:refs/prewarm/tip", log=log)
    tip = git(checkout, "rev-parse", "--verify", "refs/prewarm/tip^{commit}", log=log)
    git(checkout, "-c", "core.hooksPath=/dev/null", "checkout", "--detach", "--force",
        "--no-recurse-submodules", tip, log=log)
    return tip


def cargo_env(checkout, settings):
    env = {key: value for key, value in settings.items() if not key.startswith("PREWARM_")}
    for name in ("CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"):
        env.pop(name, None)
    config = checkout / ".cargo/config.toml"
    cargo_config = tomllib.loads(config.read_text()) if config.exists() else {}
    if "SCCACHE_BASEDIRS" not in cargo_config.get("env", {}):
        env["SCCACHE_BASEDIRS"] = str(checkout)
    return env


def sample_load(command):
    if not command:
        return os.getloadavg()[0]
    value = float(capture(["bash", "-c", command], timeout=5).strip())
    if not math.isfinite(value) or value < 0:
        raise PrewarmError("PREWARM_LOAD_CMD must print a finite, nonnegative load")
    return value


def signal_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass


def build(checkout, args, env, load, limit, poll, pause_budget, status, log, interrupted):
    command = [*CARGO, *args]
    log_event(log, f"running {shlex.join(command)}")
    started = time.monotonic()
    paused_at = None
    try:
        with subprocess.Popen(command, cwd=checkout, env=env, stdout=log,
                              stderr=subprocess.STDOUT, start_new_session=True) as process:
            try:
                while True:
                    interrupted()
                    timeout = poll
                    if paused_at is not None:
                        remaining = pause_budget - status["paused_seconds"] - (time.monotonic() - paused_at)
                        if remaining <= 0:
                            raise PrewarmError("maximum pause time reached; retrying on the next trigger")
                        timeout = min(poll, remaining)
                    try:
                        code = process.wait(timeout=timeout)
                    except subprocess.TimeoutExpired:
                        if paused_at is not None and timeout < poll:
                            raise PrewarmError("maximum pause time reached; retrying on the next trigger")
                        current = load()
                        if current > limit and paused_at is None:
                            signal_group(process, signal.SIGSTOP)
                            paused_at = time.monotonic()
                            log_event(log, f"paused pid={process.pid}: load {current:g} > {limit:g}")
                        elif current <= limit and paused_at is not None:
                            status["paused_seconds"] += time.monotonic() - paused_at
                            if status["paused_seconds"] >= pause_budget:
                                paused_at = None
                                raise PrewarmError("maximum pause time reached; retrying on the next trigger")
                            signal_group(process, signal.SIGCONT)
                            paused_at = None
                            log_event(log, f"resumed pid={process.pid}: load {current:g} <= {limit:g}")
                        continue
                    interrupted()
                    if code:
                        raise PrewarmError(f"{shlex.join(command)} exited with status {code}")
                    break
            finally:
                if process.returncode is None:
                    signal_group(process, signal.SIGKILL)
    finally:
        now = time.monotonic()
        if paused_at is not None:
            status["paused_seconds"] += now - paused_at
        elapsed = now - started
        status["build_seconds"] += elapsed
        log_event(log, f"finished {args[0]} after {elapsed:.2f}s")


def stats_snapshot(checkout, env, settings):
    binary = settings.get("PREWARM_SCCACHE_BIN", "sccache")
    data = json.loads(capture([binary, "--show-stats", "--stats-format=json"],
                              cwd=checkout, env=env, timeout=10))
    if data.get("effective_cache_mode") is None:
        raise PrewarmError("shared sccache server is unavailable")
    stats = data["stats"]
    return {
        "compile_requests": stats["compile_requests"],
        "cache_hits": sum(stats["cache_hits"]["counts"].values()),
        "cache_misses": sum(stats["cache_misses"]["counts"].values()),
        "cache_writes": stats["cache_writes"],
    }


def clean_checkout(checkout, env):
    metadata = json.loads(capture([*CARGO, "metadata", "--offline", "--no-deps", "--format-version", "1"],
                                  cwd=checkout, env=env))
    dirs = {Path(metadata[key]).resolve() for key in ("target_directory", "build_directory")
            if metadata.get(key)}
    for directory in dirs:
        if directory == checkout or not directory.is_relative_to(checkout):
            raise PrewarmError(f"refusing to clean an output path outside the dedicated checkout: {directory}")
    for directory in sorted(dirs):
        if directory.exists():
            shutil.rmtree(directory)


def disk_usage(checkout):
    sizes = {part: {} for part in ("checkout", "source", "target")}
    for key, flags in (("apparent_bytes", ["--apparent-size"]), ("allocated_bytes", [])):
        total = target = 0
        if checkout.exists():
            output = capture(["du", "--null", "--block-size=1", "--max-depth=1", *flags, "."],
                             cwd=checkout)
            counts = {}
            for entry in output.rstrip("\0").split("\0"):
                size, path = entry.split("\t", 1)
                counts[path] = int(size)
            total, target = counts["."], counts.get("./target", 0)
        sizes["checkout"][key] = total
        sizes["source"][key] = total - target
        sizes["target"][key] = target

    if checkout.exists() and capture(["stat", "-f", "-c", "%T", "."], cwd=checkout).strip() == "btrfs":
        for command in (["compsize", "-b", "."], ["sudo", "-n", "compsize", "-b", "."]):
            try:
                sizes["compsize"] = capture(command, cwd=checkout, stderr=subprocess.DEVNULL).strip()
            except (OSError, subprocess.SubprocessError):
                continue
            break
    return sizes


def write_atomic(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(data)
    temporary.replace(path)


def prewarm(checkout, marker, settings, status, log, interrupted):
    repo = Path(settings["PREWARM_REPO"]).expanduser().resolve(strict=True)
    ref = f"refs/heads/{settings.get('PREWARM_BRANCH', 'main')}"
    if not repo.is_dir() or repo == checkout or repo.is_relative_to(checkout):
        raise PrewarmError(f"invalid canonical checkout: {repo}")

    git(repo, "check-ref-format", ref, log=log)
    tip = git(repo, "rev-parse", "--verify", f"{ref}^{{commit}}", log=log)
    status["tip_commit"] = tip
    if tip == status["last_warmed_commit"]:
        log_event(log, f"already warmed {tip}")
        return "unchanged"

    cpus = len(os.sched_getaffinity(0))
    jobs = int(settings.get("PREWARM_JOBS", max(1, cpus // 4)))
    limit = float(settings.get("PREWARM_LOAD_LIMIT", cpus / 2))
    poll = float(settings.get("PREWARM_POLL_SECONDS", 5))
    pause_budget = float(settings.get("PREWARM_MAX_PAUSE_SECONDS", 900))
    if (jobs < 1 or poll <= 0 or limit < 0 or pause_budget < 0
            or not all(math.isfinite(value) for value in (limit, poll, pause_budget))):
        raise PrewarmError("jobs and poll interval must be positive, and load and pause limits nonnegative")
    commands = [shlex.split(part) for part in settings.get("PREWARM_COMMANDS", DEFAULT_COMMANDS).split(";")]
    if any(not args or args[0].startswith(("-", "+")) for args in commands):
        raise PrewarmError("PREWARM_COMMANDS must contain Cargo subcommands separated by semicolons")

    def load():
        return sample_load(settings.get("PREWARM_LOAD_CMD"))

    interrupted()
    current = load()
    if current > limit:
        log_event(log, f"deferred {tip}: load {current:g} > {limit:g}")
        return "deferred"

    tip = prepare_checkout(repo, checkout, ref, log)
    status["tip_commit"] = tip
    if tip == status["last_warmed_commit"]:
        log_event(log, f"already warmed {tip}")
        return "unchanged"

    env = cargo_env(checkout, settings)
    before = stats_snapshot(checkout, env, settings)
    log_event(log, f"warming {tip} with {jobs} private Cargo jobs")
    try:
        for args in commands:
            interrupted()
            current = load()
            if current > limit:
                log_event(log, f"deferred {tip}: load {current:g} > {limit:g}")
                return "deferred"
            job_position = args.index("--") if "--" in args else len(args)
            args[job_position:job_position] = ["-j", str(jobs)]
            build(checkout, args, env, load, limit, poll, pause_budget, status, log, interrupted)
        interrupted()
        write_atomic(marker, f"{tip}\n")
        status["last_warmed_commit"] = tip
        return "warmed"
    finally:
        try:
            after = stats_snapshot(checkout, env, settings)
            delta = {key: after[key] - value for key, value in before.items()}
            if min(delta.values()) < 0:
                raise PrewarmError("sccache counters went backwards")
            status["sccache_delta"] = delta
            log_event(log, f"shared sccache delta {json.dumps(delta)}")
        except (PrewarmError, OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            log_event(log, f"shared sccache delta unavailable: {error}")


def run(project, clean):
    os.umask(0o077)
    os.setpriority(os.PRIO_PROCESS, 0, 19)
    subprocess.run(["ionice", "-c3", "-p", str(os.getpid())], check=True)
    # Inherited Git repository overrides could make a write in the clone hit the canonical repo.
    for name in capture(["git", "rev-parse", "--local-env-vars"]).splitlines():
        os.environ.pop(name, None)
    cachedir = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    configdir = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    state = (cachedir / "leto-maintenance/prewarm" / project).resolve()
    state.mkdir(parents=True, exist_ok=True)
    checkout = state / "checkout"
    marker = state / "last-warmed"
    stop_signal = None

    def request_stop(signum, _frame):
        nonlocal stop_signal
        stop_signal = signum

    def interrupted():
        if stop_signal is not None:
            raise PrewarmError(f"interrupted by {signal.Signals(stop_signal).name}")

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, request_stop)

    with (state / "prewarm.lock").open("a") as lock, (state / "prewarm.log").open("a") as log:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log_event(log, f"{project}: pre-warm already running")
            return 0

        started = time.monotonic()
        status = {"result": "failed", "tip_commit": None, "last_warmed_commit": None,
                  "build_seconds": 0.0, "paused_seconds": 0.0, "disk": None}
        try:
            status["last_warmed_commit"] = marker.read_text().strip() if marker.exists() else None
            validate_checkout(checkout)
            config = read_config(configdir / "leto-prewarm" / f"{project}.conf")
            settings = {**os.environ, "SCCACHE_CLIENT_SIDE": "1", **config}
            if clean:
                if checkout.exists():
                    clean_checkout(checkout, cargo_env(checkout, settings))
                status["result"] = "cleaned"
            else:
                status["result"] = prewarm(checkout, marker, settings, status, log, interrupted)
        except (PrewarmError, OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            status["error"] = str(error)
            log_event(log, f"{project}: {error}")
        finally:
            try:
                validate_checkout(checkout)
                status["disk"] = disk_usage(checkout)
            except (PrewarmError, OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
                log_event(log, f"disk usage unavailable: {error}")
            else:
                log_event(log, f"disk {json.dumps(status['disk'])}")
            status["finished_at"] = timestamp()
            status["duration_seconds"] = time.monotonic() - started
            for key in ("duration_seconds", "build_seconds", "paused_seconds"):
                status[key] = round(status[key], 3)
            write_atomic(state / "status.json", json.dumps(status, indent=2) + "\n")
            log_event(log, f"{project}: {status['result']} tip={status['tip_commit']} "
                      f"last_warmed={status['last_warmed_commit']} in {status['duration_seconds']:.2f}s "
                      f"(build {status['build_seconds']:.2f}s, paused {status['paused_seconds']:.2f}s)")
        return int(status["result"] == "failed")


def main():
    parser = argparse.ArgumentParser(description="Pre-warm the shared sccache from a dedicated clone.")
    parser.add_argument("project", help="project config name, without .conf")
    parser.add_argument("--clean", action="store_true", help="remove the dedicated clone's build output")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", args.project):
        parser.error("project must start with an alphanumeric character and use only letters, digits, _, ., or -")
    try:
        return run(args.project, args.clean)
    except (OSError, subprocess.SubprocessError) as error:
        print(f"prewarm: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
