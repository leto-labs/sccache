import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("shadow_report", ROOT / "report.py")
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


class ReportTest(unittest.TestCase):
    def test_checkout_ancestor_lookups_stay_pure(self):
        row = {"worktree": "/home/u/.t3/worktrees/p/lane"}
        lookups = {"class": "other", "reason": "read outside checkout and allowed system paths",
                   "other_reads": ["/", "/home", "/home/u/.t3/worktrees/p"], "checkout_reads": []}
        self.assertEqual(report.observed_class(row, lookups), "pure")
        self.assertEqual(report.observed_class(row, {**lookups, "checkout_reads": ["/x"]}), "files")
        foreign = {**lookups, "other_reads": ["/home/u/.aws/credentials"]}
        self.assertEqual(report.observed_class(row, foreign), "other")
        self.assertEqual(report.observed_class(row, {**lookups, "outside_writes": ["/var/x"]}), "other")
        listing = {**lookups, "ancestor_lookups": ["/home"], "other_reads": ["/home"]}
        self.assertEqual(report.observed_class(row, listing), "other")

    def test_cancelled_runs_are_not_nondeterminism(self):
        cancelled = report.mismatch_class(("p", "b", "t"), set(), (("signal", 2),), (("exit", 0),))
        self.assertEqual(cancelled, "cancelled run")
        flaky = report.mismatch_class(("p", "b", "t"), set(), (("exit", 101),), (("exit", 0),))
        self.assertEqual(flaky, "unclassified nondeterminism")

    def test_weekly_fixture(self):
        fixture = ROOT / "tests/fixtures/shadow.jsonl"
        rows = [json.loads(line) for line in fixture.read_text().splitlines()]
        day = 86_400 * report.NS
        benchmark = {
            "gate_budget_reference_s": 235,
            "runs": [
                {"mode": "plain", "wall_s": 98, "pair": 1},
                {"mode": "plain", "wall_s": 102, "pair": 2},
                {"mode": "default", "wall_s": 106, "pair": 1},
            ],
        }
        result = report.aggregate(rows, 10 * day, 17 * day, [("toy", "toy", "flake")], benchmark)
        self.assertEqual(result["totals"]["would_be"]["same_outcome"], {"count": 6, "seconds": 12.0})
        self.assertEqual(result["totals"]["would_be"]["stage1"], {"count": 2, "seconds": 4.0})
        self.assertEqual(result["totals"]["would_be"]["stage2_additional"], {"count": 1, "seconds": 2.0})
        self.assertEqual(result["rebuilds"], {"rebuilds": 2, "byte_identical": 1, "rate": 0.5})
        self.assertEqual(result["mismatches"][0]["test"], "flake")
        self.assertEqual(result["mismatches"][0]["classification"], "known flake")
        tests = {test["test"]: test for test in result["tests"]}
        self.assertIs(tests["pure"]["stable"], True)
        self.assertIs(tests["unstable"]["stable"], False)
        self.assertIsNone(tests["expired"]["stable"])
        self.assertEqual(result["benchmark"]["modes"]["default"]["added_wall_s"], 8)
        result["runner_errors"] = 0
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            report.write_report(output, "2026-W47", result)
            self.assertEqual((output / "latest.json").readlink(), Path("2026-W47.json"))
            self.assertIn("known flake", (output / "latest.md").read_text())


if __name__ == "__main__":
    unittest.main()
