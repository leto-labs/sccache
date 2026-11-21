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
    def test_weekly_fixture(self):
        fixture = ROOT / "tests/fixtures/shadow.jsonl"
        rows = [json.loads(line) for line in fixture.read_text().splitlines()]
        day = 86_400 * report.NS
        benchmark = {
            "gate_budget_reference_s": 235,
            "runs": [
                {"mode": "plain", "wall_s": 98},
                {"mode": "plain", "wall_s": 102},
                {"mode": "default", "wall_s": 106},
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
        self.assertEqual(result["benchmark"]["modes"]["default"]["added_wall_s"], 6)
        result["runner_errors"] = 0
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            report.write_report(output, "2026-W47", result)
            self.assertEqual((output / "latest.json").readlink(), Path("2026-W47.json"))
            self.assertIn("known flake", (output / "latest.md").read_text())


if __name__ == "__main__":
    unittest.main()
