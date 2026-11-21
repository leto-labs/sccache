#!/usr/bin/env bash
set -euo pipefail
exec python3 - "$@" <<'PY'
import argparse
import filecmp
from functools import partial
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile


TESTS = "Test harnesses"
BINS = "Bins"
EXAMPLES = "Examples"
OTHER = "Other executables"
SCRIPTS = "Build-script executables"
OUTPUT = "Build-script output"


def cargo_messages(command, checkout, env):
    command = ["cargo", *command, "--message-format=json"]
    messages = []
    # Let Cargo finish writing before interpreting any of its messages.
    with tempfile.TemporaryFile() as output:
        cargo = subprocess.run(command, cwd=checkout, env=env, stdout=output)
        output.seek(0)
        for line in output:
            try:
                message = json.loads(line)
            except ValueError:
                print(line.decode("utf-8", errors="replace"), file=sys.stderr, end="")
                continue
            if not isinstance(message, dict):
                continue
            reason = message.get("reason")
            if reason == "compiler-message":
                diagnostic = message.get("message")
                rendered = diagnostic.get("rendered") if isinstance(diagnostic, dict) else None
                if rendered:
                    print(rendered, file=sys.stderr, end="")
            elif reason in ("compiler-artifact", "build-script-executed"):
                messages.append(message)
    cargo.check_returncode()
    return messages


def executable_path(path, target):
    if path.is_file() and os.access(path, os.X_OK):
        resolved = path.resolve()
        if resolved.is_relative_to(target):
            return resolved.relative_to(target)
    return None


def reported_build_script(message, target):
    if message.get("executable"):
        executable = executable_path(Path(message["executable"]), target)
        if executable is not None:
            return executable
    for filename in message.get("filenames", ()):
        path = Path(filename)
        if path.name.startswith("build-script-"):
            executable = executable_path(path, target)
            if executable is not None:
                return executable
    return None


def nearby_build_script(out_dir, target):
    execution_dir = (target / out_dir).parent
    for name in ("build-script-build", "build-script-build.exe"):
        executable = executable_path(execution_dir / name, target)
        if executable is not None:
            return executable

    # Cargo can use different hashes for compiling and running the same build script.
    package = execution_dir.name.rpartition("-")[0]
    candidates = set()
    for directory in execution_dir.parent.glob(f"{package}-*"):
        if directory.name.rpartition("-")[0] == package:
            for path in directory.glob("build-script-*"):
                executable = executable_path(path, target)
                if executable is not None:
                    candidates.add(executable)
    return candidates.pop() if len(candidates) == 1 else None


def build_script_executables(compiled_scripts, script_runs, target):
    scripts = {}
    for package in compiled_scripts.keys() | script_runs.keys():
        executables = {reported_build_script(message, target) for message in compiled_scripts.get(package, ())}
        if not executables or None in executables:
            fallback = {nearby_build_script(out_dir, target) for out_dir in script_runs.get(package, ())}
            if fallback:
                executables.discard(None)
                executables.update(fallback)
            else:
                executables.add(None)
        scripts[package] = executables
    return scripts


def collect(checkout, target, env, all_targets):
    artifacts = {TESTS: set()}
    compiled_scripts = {}
    script_runs = {}
    bins = {}
    commands = [["test", "--workspace", "--no-run"]]
    if all_targets:
        commands.insert(0, ["build", "--workspace", "--bins", "--examples"])
        artifacts.update({category: set() for category in (BINS, EXAMPLES, OTHER, SCRIPTS, OUTPUT)})
    messages = [message for command in commands for message in cargo_messages(command, checkout, env)]
    for message in messages:
        if message["reason"] == "build-script-executed":
            if all_targets:
                out_dir = Path(message["out_dir"]).resolve().relative_to(target)
                script_runs.setdefault(message["package_id"], set()).add(out_dir)
                artifacts[OUTPUT].add(out_dir.with_name("output"))
            continue
        harness = message["profile"]["test"]
        if not all_targets and not harness:
            continue
        kind = message["target"]["kind"]
        if all_targets and "custom-build" in kind:
            compiled_scripts.setdefault(message["package_id"], []).append(message)
        elif message["executable"]:
            path = Path(message["executable"]).resolve().relative_to(target)
            category = TESTS if harness or "test" in kind else OTHER
            if category == OTHER and "bin" in kind:
                category = BINS
                bins.setdefault(message["target"]["name"], set()).add((message["package_id"], path))
            if category == OTHER and "example" in kind:
                category = EXAMPLES
            artifacts[category].add(path)
    scripts = build_script_executables(compiled_scripts, script_runs, target)
    if all_targets:
        artifacts[SCRIPTS].update(path for paths in scripts.values() for path in paths if path is not None)
    out_dirs = {out_dir for dirs in script_runs.values() for out_dir in dirs}
    return artifacts, out_dirs, bins, scripts


def stripped(objcopy, executable, output):
    subprocess.run(
        [objcopy, "--strip-debug", "--remove-section=.note.gnu.build-id", executable, output],
        check=True,
    )


def compare_executable(before, after, objcopy, scratch):
    if filecmp.cmp(before, after, shallow=False):
        return "identical", "identical"
    stripped_before = scratch / "restored.stripped"
    stripped_after = scratch / "rebuilt.stripped"
    stripped(objcopy, before, stripped_before)
    stripped(objcopy, after, stripped_after)
    filecmp.clear_cache()
    same = filecmp.cmp(stripped_before, stripped_after, shallow=False)
    return "differs", "identical" if same else "differs"


def path_normalizer(checkout, target, warm_checkout, warm_target):
    prefixes = {str(checkout): "<CHECKOUT>", str(target): "<TARGET>"}
    if warm_checkout is not None:
        prefixes.update({str(warm_checkout): "<CHECKOUT>", str(warm_target): "<TARGET>"})
    # A target inside a checkout must match before the checkout itself.
    paths = "|".join(re.escape(path) for path in sorted(prefixes, key=len, reverse=True))
    pattern = re.compile(r"(?:" + paths + r")(?![\w.-])")

    def normalize(data):
        if b"\0" in data:
            return data
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            return data
        return pattern.sub(lambda match: prefixes[match[0]], text).encode("utf-8")

    return normalize


def compare_data(before, after, normalize):
    if before.is_symlink() != after.is_symlink():
        return "differs", "differs"
    if before.is_symlink():
        before_bytes, after_bytes = (os.fsencode(os.readlink(path)) for path in (before, after))
    elif filecmp.cmp(before, after, shallow=False):
        return "identical", "identical"
    else:
        before_bytes, after_bytes = before.read_bytes(), after.read_bytes()
    if before_bytes == after_bytes:
        return "identical", "identical"
    same = normalize(before_bytes) == normalize(after_bytes)
    return "differs", "identical" if same else "differs"


def tree_files(directory):
    return {
        path.relative_to(directory)
        for path in directory.rglob("*")
        if path.is_symlink() or path.is_file()
    }


def compare_tree(before, after, out_dir, normalize):
    restored, rebuilt = tree_files(before), tree_files(after)
    for path in sorted(rebuilt - restored):
        print(f"{out_dir / path}\tadded in rebuilt OUT_DIR")
    for path in sorted(restored - rebuilt):
        print(f"{out_dir / path}\tremoved from rebuilt OUT_DIR")
    raw_differs = normalized_differs = restored != rebuilt
    for path in sorted(restored & rebuilt):
        raw, normal = compare_data(before / path, after / path, normalize)
        if raw == "differs":
            print(f"{out_dir / path}\tchanged in rebuilt OUT_DIR\tnormalized: {normal}")
            raw_differs = True
            normalized_differs |= normal == "differs"
    return (
        "differs" if raw_differs else "identical",
        "differs" if normalized_differs else "identical",
    )


def missing_from(has_before, has_after):
    if not has_before and not has_after:
        return "both builds"
    return "the rebuilt build" if has_before else "the restored build"


def unresolved_scripts(restored, rebuilt):
    for package in sorted(restored.keys() | rebuilt.keys()):
        before, after = restored.get(package, set()), rebuilt.get(package, set())
        # An executable found in only one pass already produces a missing row.
        missing_before = None in before and not (after - before - {None})
        missing_after = None in after and not (before - after - {None})
        if missing_before or missing_after:
            yield package, missing_from(not missing_before, not missing_after)


def compare_category(category, restored, rebuilt, saved, target, compare, form, remapped, *,
                     tree=False, unresolved=()):
    counters = dict.fromkeys(("identical", "differs", "missing"), 0)
    normalized = counters.copy()
    present = Path.is_dir if tree else Path.is_file
    missing = "missing executable" if category == SCRIPTS else "missing"
    for package, absent in unresolved:
        print(f"{package}\t{missing} from {absent}")
        counters["missing"] += 1
        normalized["missing"] += 1
    for artifact in sorted(restored | rebuilt):
        before, after = saved / artifact, target / artifact
        has_before = artifact in restored and present(before)
        has_after = artifact in rebuilt and present(after)
        if not (has_before and has_after):
            raw = normal = "missing"
            print(f"{artifact}\t{missing} from {missing_from(has_before, has_after)}")
        else:
            raw, normal = compare(before, after)
            print(f"{artifact}\t{raw}\t{form.lower()}: {normal}")
        counters[raw] += 1
        normalized[normal] += 1

    for label, results in (("Bytes", counters), (form, normalized)):
        if category:
            label = f"{category} {label.lower()}"
        print(f"{label}: " + ", ".join(f"{count} {status}" for status, count in results.items()))
    return bool(normalized["differs"] or normalized["missing"] or (remapped and counters["differs"]))


def snapshot(artifacts, out_dirs, target, saved, run):
    # Smoke runs can write to the target tree, so keep independent copies in that case.
    save_file = shutil.copy2 if run else os.link
    for artifact in set().union(*artifacts.values()):
        source, copy = target / artifact, saved / artifact
        if source.is_file():
            copy.parent.mkdir(parents=True, exist_ok=True)
            save_file(source, copy)
    for out_dir in out_dirs:
        source = target / out_dir
        if source.is_dir():
            shutil.copytree(source, saved / out_dir, symlinks=True, copy_function=save_file)


def cargo_metadata(checkout, env):
    return json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version=1", "--no-deps"], cwd=checkout, env=env
    ))


def main():
    parser = argparse.ArgumentParser(
        prog="linked-soak.sh", description="Compare cached and locally built Cargo artifacts"
    )
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--targets", choices=("tests", "all"), default="tests",
                        help="test harnesses (default), or also bins, examples and build scripts")
    parser.add_argument("--run", action="store_true", help="run the restored harnesses before rebuilding")
    parser.add_argument("--run-bins", action="append", type=shlex.split, default=[], metavar="COMMAND",
                        help="run a named restored bin with optional arguments (all targets only)")
    parser.add_argument("--warm-checkout", type=Path,
                        help="also normalize paths from the checkout used to fill the cache")
    parser.add_argument("--objcopy", default="objcopy", help="objcopy command used for ELF normalization")
    args = parser.parse_args()
    all_targets = args.targets == "all"
    if args.run_bins and not all_targets:
        parser.error("--run-bins requires --targets all")
    if args.warm_checkout is not None and not all_targets:
        parser.error("--warm-checkout requires --targets all")
    if any(not command for command in args.run_bins):
        parser.error("--run-bins requires a binary name")
    checkout = args.checkout.resolve(strict=True)
    objcopy = shutil.which(args.objcopy)
    if objcopy is None:
        parser.error(f"could not find {args.objcopy}")

    env = os.environ.copy()
    env["RUSTC_WRAPPER"] = env.get("RUSTC_WRAPPER") or "sccache"
    env.setdefault("SCCACHE_BASEDIRS", str(checkout))
    for name in ("SCCACHE_RECACHE", "SCCACHE_NO_CACHE"):
        env.pop(name, None)

    metadata = cargo_metadata(checkout, env)
    target = Path(metadata["target_directory"]).resolve()
    build_dir = Path(metadata.get("build_directory", target)).resolve()
    if not build_dir.is_relative_to(target):
        parser.error(f"Cargo's build directory must be under the target directory ({target})")
    if target.exists() and any(target.iterdir()):
        parser.error(f"Cargo target directory must be empty for the first build ({target})")
    target.parent.mkdir(parents=True, exist_ok=True)
    remapped = env.get("SCCACHE_BASEDIRS_REMAP_PATHS", "").lower() in ("1", "on", "true")

    warm_checkout = warm_target = None
    if args.warm_checkout is not None:
        warm_checkout = args.warm_checkout.resolve(strict=True)
        warm_target = Path(cargo_metadata(warm_checkout, env)["target_directory"]).resolve()
    normalize = path_normalizer(checkout, target, warm_checkout, warm_target)

    with tempfile.TemporaryDirectory(prefix=".linked-soak-", dir=target.parent) as tmp:
        scratch = Path(tmp)
        saved = scratch / "restored"
        print("Building against the configured sccache server", flush=True)
        restored, restored_out, bins, restored_scripts = collect(checkout, target, env, all_targets)
        if not any(restored.values()) and not restored_out and not restored_scripts:
            if all_targets:
                raise ValueError("Cargo produced no linked artifacts or build-script outputs")
            raise ValueError("Cargo produced no test harnesses")
        snapshot(restored, restored_out, target, saved, args.run or args.run_bins)
        run_failed = False
        for name, *arguments in args.run_bins:
            matches = bins.get(name, set())
            if len(matches) != 1:
                raise ValueError(f"expected one restored binary named {name!r}, found {len(matches)}")
            _, executable = next(iter(matches))
            run_failed |= subprocess.run([target / executable, *arguments], cwd=checkout, env=env).returncode != 0
        if args.run:
            run_failed |= subprocess.run(
                ["cargo", "test", "--workspace", "--tests"], cwd=checkout, env=env
            ).returncode != 0
        # Preserve the target's pathname for both Cargo and the linked cache key.
        old_target = scratch / "old-target"
        if target.exists():
            target.rename(old_target)
            shutil.rmtree(old_target)

        print("Rebuilding without reading or writing the cache", flush=True)
        rebuilt, rebuilt_out, _, rebuilt_scripts = collect(
            checkout, target, env | {"SCCACHE_NO_CACHE": "1"}, all_targets,
        )
        compare_binary = partial(compare_executable, objcopy=objcopy, scratch=scratch)
        compare_text = partial(compare_data, normalize=normalize)
        differs = False
        for category, paths in restored.items():
            if category == OTHER and not (paths or rebuilt[category]):
                continue
            compare, form = (compare_text, "Normalized") if category == OUTPUT else (compare_binary, "Stripped")
            differs |= compare_category(
                category if all_targets else "", paths, rebuilt[category], saved, target, compare, form, remapped,
                unresolved=unresolved_scripts(restored_scripts, rebuilt_scripts) if category == SCRIPTS else (),
            )
        if all_targets:
            differs |= compare_category(
                "OUT_DIR trees", restored_out, rebuilt_out, saved, target,
                lambda before, after: compare_tree(before, after, after.relative_to(target), normalize),
                "Normalized", remapped, tree=True,
            )
        return int(run_failed or differs)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"linked-soak: {error}")
PY
