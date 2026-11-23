#!/usr/bin/env python3
"""Summarize the Rust miss log and follow changed externs to their root crates."""

import argparse
from collections import Counter
import json
import os
import sys


def explain(lines):
    producers = {}
    for line in lines:
        if not line.strip():
            continue
        event = json.loads(line)
        scope = event["basedir"] or event["cwd"]
        roots, unresolved = set(), set()
        classification = event["classification"]
        if classification == "own-input":
            roots.add((event["crate_name"], event["identity"], event["key"]))
        elif classification == "dependency":
            for change in event["differences"]:
                cause = producers.get((scope, change.get("artifact")))
                if cause is None or not any(cause):
                    unresolved.add(change["name"])
                else:
                    cause_roots, cause_unresolved = cause
                    roots.update(cause_roots)
                    unresolved.update(cause_unresolved)
            event["roots"] = [
                {"crate_name": name, "identity": identity, "key": key}
                for name, identity, key in sorted(roots)
            ]
            event["unresolved_externs"] = sorted(unresolved)
        for artifact in event["artifacts"]:
            producers[scope, artifact] = roots, unresolved
        yield event


def print_text(event):
    scope = event["basedir"] or event["cwd"]
    print(f'{event["crate_name"]} [{event["classification"]}] ({scope})')
    for change in event["differences"]:
        previous = change["previous"] or "(added)"
        current = change["current"] or "(removed)"
        print(f'  {change["name"]}  {previous} -> {current}')
    for root in event.get("roots", ()):
        print(f'  root {root["crate_name"]} ({root["key"]})')
    for name in event.get("unresolved_externs", ()):
        print(f"  unresolved {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="output annotated JSON lines")
    parser.add_argument("log", nargs="?", default=os.environ.get("SCCACHE_MISS_EXPLAIN"),
                        help="log file (defaults to SCCACHE_MISS_EXPLAIN)")
    args = parser.parse_args()
    if not args.log:
        parser.error("specify a log file or set SCCACHE_MISS_EXPLAIN")
    try:
        with open(args.log, encoding="utf-8") as log:
            groups = Counter()
            for event in explain(log):
                if args.json:
                    print(json.dumps(event))
                else:
                    profile = event.get("profile") or "(unknown)"
                    kind = event.get("target_kind") or "(unknown)"
                    groups[profile, kind] += 1
                    print_text(event)
            if groups:
                print("\nMisses by profile and target kind")
                for (profile, kind), count in sorted(groups.items()):
                    print(f"  {profile} / {kind}  {count}")
    except (OSError, ValueError, KeyError) as err:
        sys.exit(f"{args.log}: {err}")


if __name__ == "__main__":
    main()
