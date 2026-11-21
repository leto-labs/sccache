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
from typing import NamedTuple
from uuid import uuid4


DEFAULT_COMMANDS = "check --workspace --all-targets;test --workspace --no-run"
CARGO = ["bash", "scripts/cargo.sh"]


class PrewarmError(Exception):
    def __init__(self, message, result="failed"):
        super().__init__(message)
        self.result = result


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


def ensure_compression(state, value, log):
    """Give the pre-warm tree a btrfs compression property.

    sccache clones pre-warm outputs into reflink cache entries, and a clone keeps
    the source extents' compression, so an uncompressed tree inflates the cache.
    New directories inherit the property; existing ones are set once here.
    """
    if not value:
        return
    try:
        if os.getxattr(state, "btrfs.compression") == value.encode():
            return
    except OSError:
        pass
    try:
        os.setxattr(state, "btrfs.compression", value.encode())
    except OSError as error:
        # Not btrfs, or a value the kernel rejects: nothing to inherit.
        log_event(log, f"compression property not set on {state}: {error.strerror}")
        return
    count = 0
    for root, dirs, _files in os.walk(state):
        dirs[:] = [d for d in dirs if not os.path.islink(os.path.join(root, d))]
        for name in dirs:
            path = os.path.join(root, name)
            try:
                if os.getxattr(path, "btrfs.compression") == value.encode():
                    continue
            except OSError:
                pass
            try:
                os.setxattr(path, "btrfs.compression", value.encode())
                count += 1
            except OSError:
                pass
    log_event(log, f"set compression={value} on {state} and {count} existing directories")


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


def command_plan(settings):
    commands = [shlex.split(part) for part in settings.get("PREWARM_COMMANDS", DEFAULT_COMMANDS).split(";")]
    if any(not args or args[0].startswith(("-", "+")) for args in commands):
        raise PrewarmError("PREWARM_COMMANDS must contain Cargo subcommands separated by semicolons")
    budgets = [float(part) for part in settings.get("PREWARM_COMMAND_TIMEOUTS", "900").split(";")]
    if (len(budgets) not in (1, len(commands))
            or any(budget <= 0 or not math.isfinite(budget) for budget in budgets)):
        raise PrewarmError("PREWARM_COMMAND_TIMEOUTS needs one positive finite value or one per command")
    if len(budgets) == 1:
        budgets *= len(commands)
    memory_max = settings.get("PREWARM_MEMORY_MAX", "8G")
    if not re.fullmatch(r"[1-9][0-9]*[KMGT]?", memory_max):
        raise PrewarmError("PREWARM_MEMORY_MAX must be positive bytes or a size such as 8G")
    return zip(commands, budgets, strict=True), memory_max


class CpuSample(NamedTuple):
    taken_at: float
    cpus: int
    total: int
    busy: int
    own_seconds: float
    from_cgroup: bool


def process_tree_ticks(proc):
    ticks = 0
    pending = [proc / "self"]
    seen = set()
    while pending:
        process = pending.pop()
        if process in seen:
            continue
        seen.add(process)
        try:
            # comm may contain spaces or parentheses. The fields after it start at field 3.
            fields = (process / "stat").read_bytes().rsplit(b")", 1)[1].split()
        except (FileNotFoundError, ProcessLookupError):
            continue
        # Include reaped children to retain their CPU time after a sample sees them exit.
        ticks += sum(map(int, fields[11:15]))
        for children in (process / "task").glob("*/children"):
            try:
                pending.extend(proc / pid for pid in children.read_text().split())
            except (FileNotFoundError, ProcessLookupError):
                continue
    return ticks


class ResourceGate:
    def __init__(self, settings):
        self.proc = Path(settings.get("PREWARM_PROC_ROOT", "/proc"))
        cgroup_root = Path(settings.get("PREWARM_CGROUP_ROOT", "/sys/fs/cgroup"))
        self.cpu_stat = None
        try:
            for line in (self.proc / "self/cgroup").read_text().splitlines():
                if line.startswith("0::/") and line != "0::/":
                    self.cpu_stat = cgroup_root / line[4:] / "cpu.stat"
                    break
        except OSError:
            pass
        self.clock_ticks = os.sysconf("SC_CLK_TCK")
        self.min_spare = float(settings.get("PREWARM_MIN_SPARE_CPUS", 8))
        self.min_mem = float(settings.get("PREWARM_MIN_MEM_AVAILABLE", 0.15))
        self.max_pressure = float(settings.get("PREWARM_MAX_CPU_PRESSURE", 80))
        self.pause_after = float(settings.get("PREWARM_PAUSE_AFTER", 60))
        self.resume_after = float(settings.get("PREWARM_RESUME_AFTER", 30))
        self.poll = float(settings.get("PREWARM_POLL_SECONDS", 5))
        if (not all(math.isfinite(value) and value >= 0 for value in
                    (self.min_spare, self.pause_after, self.resume_after, self.poll))
                or self.poll == 0 or not 0 <= self.min_mem < 0.95
                or not 0 <= self.max_pressure <= 100):
            raise PrewarmError("invalid resource gate thresholds or sample interval")

    def cpu_sample(self, build_running=False):
        with (self.proc / "stat").open() as stat:
            # Guest time is already included in user and nice.
            counters = list(map(int, stat.readline().split()[1:9]))
            cpus = 0
            for line in stat:
                if not line.startswith("cpu"):
                    break
                cpus += 1
        total = sum(counters)
        busy = total - counters[3] - counters[4]
        # The command scope is a sibling of the service, so only the tree includes both.
        if not build_running and self.cpu_stat is not None:
            try:
                for line in self.cpu_stat.read_text().splitlines():
                    name, _, value = line.partition(" ")
                    if name == "usage_usec":
                        return CpuSample(time.monotonic(), cpus, total, busy, int(value) / 1e6, True)
            except (OSError, ValueError):
                pass
            self.cpu_stat = None
        own = process_tree_ticks(self.proc) / self.clock_ticks
        return CpuSample(time.monotonic(), cpus, total, busy, own, False)

    def spare_cpus(self, before, after):
        total = after.total - before.total
        if (total <= 0 or after.cpus != before.cpus
                or after.from_cgroup != before.from_cgroup):
            return None
        elapsed = after.taken_at - before.taken_at
        busy = after.cpus * (after.busy - before.busy) / total
        own = max(0, after.own_seconds - before.own_seconds) / elapsed
        return after.cpus - max(0, busy - own)

    def cpu_shortage(self, spare):
        return f"spare CPU {spare:.2f} < {self.min_spare:g}"

    def memory_or_pressure_blocker(self, resuming=False):
        mem = {}
        with (self.proc / "meminfo").open() as meminfo:
            for line in meminfo:
                name, _, value = line.partition(":")
                if name in ("MemTotal", "MemAvailable"):
                    mem[name] = int(value.split()[0])
                    if len(mem) == 2:
                        break
        available = mem["MemAvailable"] / mem["MemTotal"]
        limit = self.min_mem + 0.05 if resuming else self.min_mem
        memory_ok = available > limit if resuming else available >= limit
        if not memory_ok:
            return f"available memory {available:.1%}, threshold {limit:.1%}"
        if self.max_pressure:
            try:
                with (self.proc / "pressure/cpu").open() as cpu_pressure:
                    for line in cpu_pressure:
                        kind, *averages = line.split()
                        if kind == "some":
                            values = dict(field.split("=", 1) for field in averages)
                            pressure = float(values["avg60"])
                            if pressure > self.max_pressure:
                                return f"CPU pressure avg60 {pressure:g} > {self.max_pressure:g}"
                            break
            except OSError:
                pass
        return None

    def startup_blocker(self, interrupted):
        interrupted()
        before = self.cpu_sample()
        time.sleep(self.poll)
        interrupted()
        spare = self.spare_cpus(before, self.cpu_sample())
        if spare is None:
            return "CPU usage sample unavailable"
        if spare < self.min_spare:
            return self.cpu_shortage(spare)
        return self.memory_or_pressure_blocker()


def signal_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass


def stop_scope(unit, env, log):
    control = ["systemctl", "--user"]
    stop = subprocess.run([*control, "stop", unit], env=env, capture_output=True, text=True, timeout=15)
    state = capture([*control, "show", "--property=LoadState,Result", unit], env=env,
                    stderr=log, timeout=10)
    properties = dict(line.split("=", 1) for line in state.splitlines())
    result = properties.get("Result")
    if result and result != "success":
        subprocess.run([*control, "reset-failed", unit], env=env, stdout=log,
                       stderr=log, timeout=10)
    if stop.returncode and properties.get("LoadState") != "not-found":
        raise PrewarmError(f"could not stop command scope {unit}: {stop.stderr.strip()}")
    return result


def build(checkout, args, env, gate, budget, memory_max, pause_budget, status, log, interrupted):
    command = [*CARGO, *args]
    entry = {"command": shlex.join(command), "budget_seconds": budget, "result": "failed"}
    status["commands"].append(entry)
    unit = f"leto-prewarm-command-{uuid4().hex}.scope"
    script_dir = Path(__file__).resolve().parent
    bounded = script_dir / "leto-bounded"
    if not bounded.is_file():
        bounded = script_dir.parent / "bounded.sh"
    scope = [bounded, "--scope-unit", unit, "--mem", memory_max,
             "--timeout", str(math.ceil(budget) + 5), "--"]
    log_event(log, f"running {entry['command']} (budget {budget:g}s, memory {memory_max})")
    started = time.monotonic()
    deadline = started + budget
    paused_at = None
    cpu_streak_since = None
    try:
        previous = gate.cpu_sample(build_running=True)
        with subprocess.Popen([*scope, *command], cwd=checkout, env=env, stdout=log,
                              stderr=subprocess.STDOUT, start_new_session=True) as process:
            try:
                while True:
                    interrupted()
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise PrewarmError(f"command time budget reached ({budget:g}s)", "timed_out")
                    timeout = min(gate.poll, remaining)
                    if paused_at is not None:
                        remaining = pause_budget - status["paused_seconds"] - (time.monotonic() - paused_at)
                        if remaining <= 0:
                            raise PrewarmError("maximum pause time reached; retrying on the next trigger")
                        timeout = min(timeout, remaining)
                    try:
                        code = process.wait(timeout=timeout)
                    except subprocess.TimeoutExpired:
                        if timeout < gate.poll:
                            continue
                        current = gate.cpu_sample(build_running=True)
                        spare = gate.spare_cpus(previous, current)
                        paused = paused_at is not None
                        cpu_ready = spare is not None and spare >= gate.min_spare
                        tracking_cpu = cpu_ready if paused else spare is not None and not cpu_ready
                        if not tracking_cpu:
                            cpu_streak_since = None
                        elif cpu_streak_since is None:
                            cpu_streak_since = previous.taken_at
                        previous = current
                        streak = current.taken_at - cpu_streak_since if cpu_streak_since is not None else -1
                        reason = gate.memory_or_pressure_blocker(resuming=paused)
                        if not paused and reason is None and streak >= gate.pause_after:
                            reason = gate.cpu_shortage(spare)
                        if not paused and reason is not None:
                            signal_group(process, signal.SIGSTOP)
                            paused_at = time.monotonic()
                            cpu_streak_since = None
                            log_event(log, f"paused pid={process.pid}: {reason}")
                        elif paused and reason is None and streak >= gate.resume_after:
                            status["paused_seconds"] += time.monotonic() - paused_at
                            if status["paused_seconds"] >= pause_budget:
                                paused_at = None
                                raise PrewarmError("maximum pause time reached; retrying on the next trigger")
                            signal_group(process, signal.SIGCONT)
                            paused_at = None
                            cpu_streak_since = None
                            log_event(log, f"resumed pid={process.pid}: spare CPU {spare:.2f}")
                        continue
                    interrupted()
                    if code:
                        raise PrewarmError(f"{entry['command']} exited with status {code}")
                    break
            finally:
                if process.returncode is None:
                    signal_group(process, signal.SIGKILL)
                    process.wait()
                scope_result = stop_scope(unit, env, log)
                if scope_result == "oom-kill":
                    raise PrewarmError(f"command scope OOM kill (MemoryMax={memory_max})", "oom")
                if scope_result == "timeout":
                    raise PrewarmError(f"command scope runtime limit reached ({budget:g}s budget)", "timed_out")
        entry["result"] = "succeeded"
    except PrewarmError as error:
        entry["result"] = error.result
        raise
    finally:
        now = time.monotonic()
        if paused_at is not None:
            status["paused_seconds"] += now - paused_at
        elapsed = now - started
        status["build_seconds"] += elapsed
        entry["duration_seconds"] = round(elapsed, 3)
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
    gate = ResourceGate(settings)
    pause_budget = float(settings.get("PREWARM_MAX_PAUSE_SECONDS", 900))
    if jobs < 1 or pause_budget < 0 or not math.isfinite(pause_budget):
        raise PrewarmError("jobs must be positive and the pause budget must be finite and nonnegative")
    commands, memory_max = command_plan(settings)

    reason = gate.startup_blocker(interrupted)
    if reason is not None:
        log_event(log, f"deferred {tip}: {reason}")
        return "deferred"

    tip = prepare_checkout(repo, checkout, ref, log)
    status["tip_commit"] = tip
    if tip == status["last_warmed_commit"]:
        log_event(log, f"already warmed {tip}")
        return "unchanged"

    env = cargo_env(checkout, settings)
    env.update(SCCACHE_CLIENT_SIDE="required", SCCACHE_SERVER_AUTOSTART="false")
    before = stats_snapshot(checkout, env, settings)
    log_event(log, f"warming {tip} with {jobs} private Cargo jobs")
    try:
        for args, budget in commands:
            reason = gate.startup_blocker(interrupted)
            if reason is not None:
                status["deferred_command"] = shlex.join(args)
                log_event(log, f"deferred {tip} before {status['deferred_command']}: {reason}")
                return "deferred"
            job_position = args.index("--") if "--" in args else len(args)
            args[job_position:job_position] = ["-j", str(jobs)]
            build(checkout, args, env, gate, budget, memory_max, pause_budget, status, log, interrupted)
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
                  "build_seconds": 0.0, "paused_seconds": 0.0, "commands": [], "disk": None}
        try:
            status["last_warmed_commit"] = marker.read_text().strip() if marker.exists() else None
            validate_checkout(checkout)
            config = read_config(configdir / "leto-prewarm" / f"{project}.conf")
            settings = {**os.environ, **config}
            ensure_compression(state, settings.get("PREWARM_COMPRESSION", "zstd:1"), log)
            if clean:
                if checkout.exists():
                    clean_checkout(checkout, cargo_env(checkout, settings))
                status["result"] = "cleaned"
            else:
                status["result"] = prewarm(checkout, marker, settings, status, log, interrupted)
        except (PrewarmError, OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            if isinstance(error, PrewarmError):
                status["result"] = error.result
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
        return int("error" in status)


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
