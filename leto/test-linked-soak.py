#!/usr/bin/env python3
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


# Exercise Cargo's build-script message shapes without depending on registry access.
CARGO_FIXTURE = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

checkout = Path.cwd()
target = checkout / "target"
build = target / "debug" / "build"
command = sys.argv[1]


def emit(reason, **fields):
    print(json.dumps(dict(reason=reason, **fields)))


def executable(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"fixture executable")
    path.chmod(0o755)
    return str(path)


def compiled(package, filenames=(), binary=None, name="build-script-build"):
    emit("compiler-artifact", package_id=package, profile={"test": False},
         target={"kind": ["custom-build"], "name": name}, executable=binary, filenames=filenames)


def executed(package, directory):
    out_dir = directory / "out"
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "generated.rs").write_text("const GENERATED: u8 = 42;\n")
    (directory / "output").write_text("cargo:rustc-cfg=fixture\n")
    emit("build-script-executed", package_id=package, out_dir=str(out_dir))


if command == "metadata":
    print(json.dumps({"target_directory": str(target)}))
    sys.exit(0)

if command == "build":
    aws = "registry+https://github.com/rust-lang/crates.io-index#aws-lc-sys@0.47.0"
    compiled(aws, [executable(build / "aws-lc-sys-111" / "build-script-main")], name="build-script-main")
    print("fixture stdout outside Cargo JSON")
    executed(aws, build / "aws-lc-sys-222")
    compiled("compiled-only", binary=executable(build / "compiled-only-111" / "direct"))

    adjacent = build / "adjacent-only-111"
    executable(adjacent / "build-script-build")
    executed("adjacent-only", adjacent)

    executable(build / "sibling-only-111" / "build-script-custom")
    executed("sibling-only", build / "sibling-only-222")

    if os.environ.get("FIXTURE_MISSING"):
        compiled("missing-from-both")
        path = build / "missing-from-restore-111" / "build-script-build"
        if os.environ.get("SCCACHE_NO_CACHE"):
            executable(path)
        compiled("missing-from-restore", [str(path)])

emit("build-finished", success=True)
sys.stdout.flush()
phase = "rebuild" if os.environ.get("SCCACHE_NO_CACHE") else "restore"
with (checkout / "completed").open("a") as completed:
    print(phase, command, file=completed)
'''


class LinkedSoakTest(unittest.TestCase):
    def test_build_script_executables(self):
        soak = Path(__file__).resolve().with_name("linked-soak.sh")
        for missing in (False, True):
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                checkout = root / "checkout"
                checkout.mkdir()
                cargo = root / "cargo"
                cargo.write_text(CARGO_FIXTURE)
                cargo.chmod(0o755)
                env = os.environ | {"PATH": f"{root}{os.pathsep}{os.environ['PATH']}"}
                env.pop("FIXTURE_MISSING", None)
                if missing:
                    env["FIXTURE_MISSING"] = "1"
                result = subprocess.run(
                    [soak, checkout, "--targets", "all"], env=env, capture_output=True, text=True,
                )

                self.assertEqual(result.returncode, int(missing), result.stdout + result.stderr)
                self.assertEqual(
                    (checkout / "completed").read_text().splitlines(),
                    ["restore build", "restore test", "rebuild build", "rebuild test"],
                )
                self.assertIn("fixture stdout outside Cargo JSON", result.stderr)
                for path in ("aws-lc-sys-111/build-script-main", "compiled-only-111/direct",
                             "adjacent-only-111/build-script-build", "sibling-only-111/build-script-custom"):
                    self.assertIn(f"{path}\tidentical", result.stdout)
                self.assertIn(f"Build-script executables bytes: 4 identical, 0 differs, {2 * missing} missing",
                              result.stdout)
                self.assertIn("OUT_DIR trees normalized: 3 identical, 0 differs, 0 missing", result.stdout)
                if missing:
                    self.assertIn("missing-from-both\tmissing executable from both builds", result.stdout)
                    self.assertIn("missing-from-restore-111/build-script-build\tmissing executable "
                                  "from the restored build", result.stdout)


if __name__ == "__main__":
    unittest.main()
