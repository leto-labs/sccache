#!/usr/bin/env bash
set -euo pipefail
exec python3 - "$@" <<'PY'
import argparse
import filecmp
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def collect_tests(checkout, target, env):
    artifacts = set()
    command = ["cargo", "test", "--workspace", "--no-run", "--message-format=json"]
    with subprocess.Popen(command, cwd=checkout, env=env, stdout=subprocess.PIPE, text=True) as cargo:
        for line in cargo.stdout:
            message = json.loads(line)
            if message["reason"] == "compiler-message":
                rendered = message["message"].get("rendered")
                if rendered:
                    print(rendered, file=sys.stderr, end="")
            if (
                message["reason"] == "compiler-artifact"
                and message["profile"]["test"]
                and message["executable"]
            ):
                artifacts.add(Path(message["executable"]).resolve().relative_to(target))
        if cargo.wait():
            raise subprocess.CalledProcessError(cargo.returncode, command)
    return artifacts


def stripped(objcopy, executable, output):
    subprocess.run(
        [objcopy, "--strip-debug", "--remove-section=.note.gnu.build-id", executable, output],
        check=True,
    )


def compare(restored, rebuilt, saved, target, scratch, objcopy, remapped):
    counters = dict.fromkeys(("identical", "differs", "missing"), 0)
    normalized = counters.copy()
    for artifact in sorted(restored | rebuilt):
        before, after = saved / artifact, target / artifact
        if artifact not in restored or artifact not in rebuilt or not (before.is_file() and after.is_file()):
            raw = normal = "missing"
            absent = "restored" if artifact not in restored or not before.is_file() else "rebuilt"
            print(f"{artifact}\tmissing from {absent} build")
        else:
            raw = normal = "identical"
            if not filecmp.cmp(before, after, shallow=False):
                raw = "differs"
                stripped(objcopy, before, scratch / "restored.stripped")
                stripped(objcopy, after, scratch / "rebuilt.stripped")
                filecmp.clear_cache()
                if not filecmp.cmp(scratch / "restored.stripped", scratch / "rebuilt.stripped", shallow=False):
                    normal = "differs"
            print(f"{artifact}\t{raw}\tstripped: {normal}")
        counters[raw] += 1
        normalized[normal] += 1

    for label, results in (("Bytes", counters), ("Stripped", normalized)):
        print(f"{label}: " + ", ".join(f"{count} {status}" for status, count in results.items()))
    return bool(normalized["differs"] or normalized["missing"] or (remapped and counters["differs"]))


def main():
    parser = argparse.ArgumentParser(
        prog="linked-soak.sh", description="Compare cached and locally linked Cargo test harnesses"
    )
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--run", action="store_true", help="run the restored harnesses before rebuilding")
    parser.add_argument("--objcopy", default="objcopy", help="objcopy command used for ELF normalization")
    args = parser.parse_args()
    checkout = args.checkout.resolve(strict=True)
    objcopy = shutil.which(args.objcopy)
    if objcopy is None:
        parser.error(f"could not find {args.objcopy}")

    env = os.environ.copy()
    env["RUSTC_WRAPPER"] = env.get("RUSTC_WRAPPER") or "sccache"
    env.setdefault("SCCACHE_BASEDIRS", str(checkout))
    for name in ("SCCACHE_RECACHE", "SCCACHE_NO_CACHE"):
        env.pop(name, None)

    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version=1", "--no-deps"], cwd=checkout, env=env
    ))
    target = Path(metadata["target_directory"]).resolve()
    build_dir = Path(metadata.get("build_directory", target)).resolve()
    if not build_dir.is_relative_to(target):
        parser.error(f"Cargo's build directory must be under the target directory ({target})")
    if target.exists() and any(target.iterdir()):
        parser.error(f"Cargo target directory must be empty for the first build ({target})")
    target.parent.mkdir(parents=True, exist_ok=True)
    remapped = env.get("SCCACHE_BASEDIRS_REMAP_PATHS", "").lower() in ("1", "on", "true")

    with tempfile.TemporaryDirectory(prefix=".linked-soak-", dir=target.parent) as tmp:
        scratch = Path(tmp)
        saved = scratch / "harnesses"
        print("Building against the configured sccache server", flush=True)
        restored = collect_tests(checkout, target, env)
        if not restored:
            raise ValueError("Cargo produced no test harnesses")
        run_failed = args.run and subprocess.run(
            ["cargo", "test", "--workspace", "--tests"], cwd=checkout, env=env
        ).returncode != 0
        for artifact in restored:
            source = target / artifact
            if source.is_file():
                snapshot = saved / artifact
                snapshot.parent.mkdir(parents=True, exist_ok=True)
                os.link(source, snapshot)
        # Preserve the target's pathname for both Cargo and the linked cache key.
        old_target = scratch / "old-target"
        target.rename(old_target)
        shutil.rmtree(old_target)

        print("Rebuilding without reading or writing the cache", flush=True)
        rebuilt = collect_tests(checkout, target, env | {"SCCACHE_NO_CACHE": "1"})
        differs = compare(restored, rebuilt, saved, target, scratch, objcopy, remapped)
        return int(run_failed or differs)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"linked-soak: {error}")
PY
