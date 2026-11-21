#!/usr/bin/env python3
"""Build a weekly stage-0 report from the local runner's JSONL logs."""

import argparse
from collections import Counter, defaultdict
from datetime import date, datetime, timedelta, timezone
import json
import os
from pathlib import Path
from statistics import median
import tempfile
import sys


NS = 1_000_000_000
WEEK_NS = 7 * 86_400 * NS
STAGES = {"pure": "stage1", "files": "stage2_additional"}
CLASSES = ("pure", "files", "exec", "network", "other")


def identity(row):
    return row["project"], row["binary_id"], row["test"]


def eligible_attempt(row):
    return row.get("attempt") == 1 and not row.get("stress", False)


def observer_class(row):
    observed = row.get("observer")
    return observed.get("class") if isinstance(observed, dict) else None


def event_outcome(row):
    return tuple(sorted(row["outcome"].items()))


def measurements(data):
    if not data:
        return None
    modes = {}
    for run in data["runs"]:
        modes.setdefault(run["mode"], {"runs": []})["runs"].append(run)
    for mode in modes.values():
        mode["median_wall_s"] = median(row["wall_s"] for row in mode["runs"])
    if "plain" in modes:
        baseline = modes["plain"]
        by_pair = {run["pair"]: run["wall_s"] for run in baseline["runs"] if "pair" in run}
        for mode in modes.values():
            paired = [
                run["wall_s"] - by_pair[run["pair"]] for run in mode["runs"]
                if "pair" in run and run["pair"] in by_pair
            ]
            mode["paired_deltas_s"] = [round(delta, 4) for delta in paired]
            delta = (
                median(paired) if len(paired) == len(mode["runs"])
                else mode["median_wall_s"] - baseline["median_wall_s"]
            )
            mode["added_wall_s"] = round(delta, 4)
            mode["added_percent_of_gate"] = round(100 * delta / data["gate_budget_reference_s"], 3)
    return {**data, "modes": modes}


def counters():
    return {
        "runs": 0, "duration_ns": 0, "setup_ns": 0,
        "would_be": {
            name: {"count": 0, "seconds": 0.0}
            for name in ("same_outcome", *STAGES.values())
        },
    }


def count_hit(totals, category, row):
    totals["would_be"][category]["count"] += 1
    totals["would_be"][category]["seconds"] += row["duration_ns"] / NS


def aggregate(rows, start, end, known_flakes=(), benchmark=None):
    rows = sorted((row for row in rows if row["time_ns"] < end), key=lambda row: row["time_ns"])
    classes = defaultdict(Counter)
    recent_classes = defaultdict(set)
    for row in rows:
        if (purity := observer_class(row)) and row["time_ns"] >= start - WEEK_NS:
            test_id = identity(row)
            recent_classes[test_id].add(purity)
            if row["time_ns"] >= start:
                classes[test_id][purity] += 1

    tests = {}
    projects = {}
    totals = counters()
    observer_counts = Counter()
    prior = defaultdict(dict)
    qualifying = {}
    mismatches = {}
    rebuilds = defaultdict(Counter)
    last_binary = {}
    seen_in_run = set()
    known_flakes = set(known_flakes)

    for row in rows:
        now = row["time_ns"]
        project_name, binary_id, test_name = test_id = identity(row)
        key = row["key"]
        outcome = event_outcome(row)
        purity = observer_class(row)
        in_week = now >= start
        binary = row["worktree"], row["binary"]
        run_binary = (*binary, row["run_id"])
        if run_binary not in seen_in_run:
            seen_in_run.add(run_binary)
            previous = last_binary.get(binary)
            if in_week and previous and previous["mtime"] != row["mtime"]:
                counts = rebuilds[project_name]
                counts["rebuilds"] += 1
                counts["byte_identical"] += previous["digest"] == row["digest"]
            last_binary[binary] = row
        if in_week:
            project = projects.setdefault(project_name, counters())
            test = tests.setdefault(test_id, {
                **counters(), "project": project_name, "binary_id": binary_id, "test": test_name,
            })
            scopes = (totals, project, test)
            for scope in scopes:
                scope["runs"] += 1
                scope["duration_ns"] += row["duration_ns"]
                scope["setup_ns"] += row["setup_ns"]
            observer_counts[purity or row.get("observer", "missing")] += 1
            same = prior[key].get(outcome)
            if same:
                for scope in scopes:
                    count_hit(scope, "same_outcome", row)
            for different, before in prior[key].items():
                if different == outcome:
                    continue
                mismatch_id = (test_id, key, different, outcome)
                mismatch = mismatches.setdefault(mismatch_id, {
                    "project": project_name, "binary_id": binary_id, "test": test_name, "key": key,
                    "prior_outcome": dict(different), "outcome": dict(outcome),
                    "prior_run_id": before["run_id"], "run_id": row["run_id"],
                    "prior_time_ns": before["time_ns"], "time_ns": now, "count": 0,
                    "classification": mismatch_class(test_id, known_flakes, different, outcome),
                })
                mismatch["count"] += 1
            if row["outcome"] == {"exit": 0} and eligible_attempt(row) and test_id not in known_flakes:
                entry = qualifying.get(key)
                if (entry and len(recent_classes[test_id]) == 1 and len(prior[key]) == 1 and same
                        and now - entry[0] <= WEEK_NS and entry[2] != row["run_id"]
                        and entry[1] in recent_classes[test_id]):
                    for scope in scopes:
                        count_hit(scope, STAGES[entry[1]], row)
        prior[key][outcome] = row
        if row["outcome"] == {"exit": 0} and eligible_attempt(row) and purity in STAGES:
            qualifying[key] = (now, purity, row["run_id"])

    for test_id, test in tests.items():
        observed = classes[test_id]
        test["classes"] = dict(observed)
        test["purity"] = next(iter(observed)) if len(observed) == 1 else None
        test["stable"] = len(observed) == 1 if observed else None
    project_classes = defaultdict(Counter)
    for test in tests.values():
        verdict = test["purity"] or ("unstable" if test["stable"] is False else "unobserved")
        project_classes[test["project"]][verdict] += 1
    for name, project in projects.items():
        project["rebuilds"] = rebuild_totals(rebuilds[name])
        project["purity"] = dict(project_classes[name])
    all_rebuilds = sum(rebuilds.values(), Counter())
    return {
        "totals": totals, "observer": dict(observer_counts), "rebuilds": rebuild_totals(all_rebuilds),
        "distinct_keys": len(prior),
        "projects": projects,
        "tests": [value for _, value in sorted(tests.items())],
        "mismatches": list(mismatches.values()), "benchmark": measurements(benchmark),
    }


def rebuild_totals(counts):
    return {
        "byte_identical": counts["byte_identical"], "rebuilds": counts["rebuilds"],
        "rate": counts["byte_identical"] / counts["rebuilds"] if counts["rebuilds"] else None,
    }


FIELDS = ("time_ns", "key", "binary", "worktree", "run_id", "mtime", "digest", "duration_ns", "setup_ns",
          "outcome", "project", "binary_id", "test", "attempt", "stress")


def ancestors(path):
    parents = {"/"}
    while path and path != "/":
        path = os.path.dirname(path.rstrip("/"))
        parents.add(path or "/")
    return parents


def observed_class(row, observed):
    """The runner's class, except that looking up the checkout's ancestor
    directories (canonicalize, current_dir) does not make a test impure."""
    purity = observed.get("class")
    if (purity == "other" and (observed.get("reason") or "").startswith("read outside")
            and not observed.get("outside_writes")):
        allowed = ancestors(row.get("worktree") or "") | {"/tmp"}
        if all(path in allowed for path in observed.get("other_reads", [])):
            purity = "files" if observed.get("checkout_reads") else "pure"
    return purity


def slim(row):
    # Observed path sets dominate the records; aggregation needs only the class.
    kept = {field: row[field] for field in FIELDS if field in row}
    observed = row.get("observer")
    kept["observer"] = {"class": observed_class(row, observed)} if isinstance(observed, dict) else observed
    return kept


CANCEL_SIGNALS = {2, 15}


def mismatch_class(test_id, known_flakes, *outcomes):
    # SIGINT/SIGTERM come from a cancelled run (Ctrl-C, nextest fail-fast), not the test.
    if any(dict(outcome).get("signal") in CANCEL_SIGNALS for outcome in outcomes):
        return "cancelled run"
    return "known flake" if test_id in known_flakes else "unclassified nondeterminism"


def select_logs(directory, kind, first_day, last_day, max_bytes):
    """Day files in [first_day, last_day], newest first, within max_bytes.

    Returns (path, bytes to read from the end) in chronological order and
    whether older data was left out.
    """
    chosen, budget, truncated = [], max_bytes, False
    paths = sorted((directory / kind).glob("*.jsonl"), reverse=True)
    for path in paths:
        if not first_day <= path.stem <= last_day:
            continue
        if budget <= 0:
            truncated = True
            break
        size = path.stat().st_size
        chosen.append((path, min(size, budget)))
        truncated = truncated or size > budget
        budget -= size
    return chosen[::-1], truncated


def logs(selected, kind, stats=None):
    errors = 0
    for path, length in selected:
        with path.open("rb") as source:
            size = os.fstat(source.fileno()).st_size
            if length < size:
                source.seek(size - length)
                source.readline()
            for line in source:
                try:
                    yield slim(json.loads(line))
                except ValueError:
                    errors += 1
            if stats is not None:
                stats["bytes_read"] = stats.get("bytes_read", 0) + length
    if errors:
        print(f"Skipped {errors} malformed {kind} records.", file=sys.stderr)


def table_text(value):
    return str(value).replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")


def markdown(week, report):
    totals = report["totals"]
    lines = [
        f"# Test cache shadow report · {week}", "",
        f"{totals['runs']:,} recorded test invocations across {len(report['tests']):,} test identities. "
        f"Key and log setup took {totals['setup_ns'] / NS:.2f} cumulative process-seconds.", "",
        "| Project | Tests run | Pure | Files | Exec | Network | Other | Unstable | Unobserved |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for name, project in sorted(report["projects"].items()):
        counts = project["purity"]
        values = [counts.get(purity, 0) for purity in (*CLASSES, "unstable", "unobserved")]
        lines.append(f"| {table_text(name)} | {project['runs']:,} | " + " | ".join(map(str, values)) + " |")
    rebuild = report["rebuilds"]
    rate = f" ({rebuild['rate']:.1%})" if rebuild["rate"] is not None else ""
    lines += [
        "", f"Byte-identical rebuilds · {rebuild['byte_identical']:,} of {rebuild['rebuilds']:,}{rate}.", "",
        "| Would-be hit | Invocations | Test process-seconds |", "|---|---:|---:|",
    ]
    for category, label in [
        ("same_outcome", "Same key and outcome"),
        ("stage1", "Stage 1 · pure passes"),
        ("stage2_additional", "Stage 2 · additional file candidates"),
    ]:
        hit = totals["would_be"][category]
        lines.append(f"| {label} | {hit['count']:,} | {hit['seconds']:.2f} |")
    cancelled = [row for row in report["mismatches"] if row["classification"] == "cancelled run"]
    mismatches = [row for row in report["mismatches"] if row["classification"] != "cancelled run"]
    lines += ["", "Stage 2 figures are candidates before file manifest verification.", "",
              "## Mismatches", "", f"{len(mismatches):,} distinct key/outcome changes, "
              f"plus {len(cancelled):,} from cancelled runs (SIGINT/SIGTERM), not listed."]
    for row in mismatches[:20]:
        lines.append(f"- {table_text(row['project'])} / {table_text(row['binary_id'])} / "
                     f"`{table_text(row['test']).replace(chr(96), '')}` · {row['prior_outcome']} → "
                     f"{row['outcome']} · {row['classification']} ({row['count']:,}).")
    if len(mismatches) > 20:
        lines.append(f"- {len(mismatches) - 20:,} more in the JSON.")
    lines += ["", "## Overhead", ""]
    if benchmark := report["benchmark"]:
        reference = benchmark["gate_budget_reference_s"]
        lines += [
            f"Gate budget reference · {reference:g} s. Added wall uses the median paired difference "
            "when every run has a plain control in its pair.", "",
            "| Mode | Wall runs (s) | Median (s) | Added wall (s) | Share of gate |",
            "|---|---|---:|---:|---:|",
        ]
        for mode, result in benchmark["modes"].items():
            values = ", ".join(f"{run['wall_s']:.2f}" for run in result["runs"])
            lines.append(f"| {table_text(mode)} | {values} | {result['median_wall_s']:.2f} | "
                         f"{result['added_wall_s']:+.2f} | {result['added_percent_of_gate']:+.2f}% |")
        if notes := benchmark.get("notes"):
            lines.extend(["", notes])
    else:
        lines.append("Provide paired run timings with `--benchmark` to measure wall overhead.")
    lines += ["", f"Observer results · {', '.join(f'{key} {value:,}' for key, value in sorted(report['observer'].items()))}.",
              f"Runner errors in the week · {report['runner_errors']:,}.", ""]
    source = report.get("input")
    if source and source["truncated"]:
        lines[-1:-1] = [f"Input capped at {source['max_bytes'] >> 20} MiB of logs, newest first; "
                        f"older records were left out.", ""]
    return "\n".join(lines)


def write_report(output, stem, report):
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output) as temporary:
        temporary = Path(temporary)
        for suffix, content in (
            ("json", json.dumps(report, indent=2, sort_keys=True) + "\n"),
            ("md", markdown(stem, report)),
        ):
            name = f"{stem}.{suffix}"
            (temporary / name).write_text(content, encoding="utf-8")
            (temporary / f"latest.{suffix}").symlink_to(name)
            os.replace(temporary / name, output / name)
            os.replace(temporary / f"latest.{suffix}", output / f"latest.{suffix}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    now = datetime.now(timezone.utc)
    parser.add_argument("--week", default=now.strftime("%G-W%V"), help="UTC ISO week, e.g. 2026-W47")
    parser.add_argument("--store", type=Path, default=Path(os.environ.get(
        "LETO_TEST_CACHE_DIR", Path.home() / ".cache/leto-test-results/v0")))
    parser.add_argument("--output", type=Path, default=Path.home() / "data/core/test-cache")
    parser.add_argument("--known-flakes", type=Path, help="JSON array of project, binary_id and test objects")
    parser.add_argument("--benchmark", type=Path, help="JSON containing gate_budget_reference_s and runs (mode, wall_s)")
    parser.add_argument("--max-bytes", type=int, default=256 << 20,
                        help="read at most this many log bytes, newest first (default 256 MiB)")
    options = parser.parse_args()
    year, week = options.week.split("-W")
    monday = date.fromisocalendar(int(year), int(week), 1)
    monday = datetime.combine(monday, datetime.min.time(), timezone.utc)
    start = int(monday.timestamp()) * NS
    end = int((monday + timedelta(days=7)).timestamp()) * NS
    known = map(identity, json.loads(options.known_flakes.read_text())) if options.known_flakes else ()
    benchmark = json.loads(options.benchmark.read_text()) if options.benchmark else None
    # The purity history reaches one week back from the report week.
    first_day = (monday - timedelta(days=7)).date().isoformat()
    last_day = (monday + timedelta(days=6)).date().isoformat()
    runs, truncated = select_logs(options.store, "runs", first_day, last_day, options.max_bytes)
    read = {}
    report = aggregate(logs(runs, "runs", read), start, end, known, benchmark)
    errors, _ = select_logs(options.store, "errors", first_day, last_day, 16 << 20)
    report["runner_errors"] = sum(start <= row["time_ns"] < end for row in logs(errors, "errors"))
    report["input"] = {"bytes_read": read.get("bytes_read", 0), "max_bytes": options.max_bytes,
                       "truncated": truncated, "first_file": runs[0][0].name if runs else None}
    report["week"] = options.week
    write_report(options.output, options.week, report)
    print(options.output / f"{options.week}.md")


if __name__ == "__main__":
    main()
