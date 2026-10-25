#!/usr/bin/env python3
"""Compare public fixture effects; normalize only nonce and two v5 durations."""

import argparse
import copy
import json
import tempfile
import unittest
from pathlib import Path

from input_path_cases import case_names


def read(path):
    result = []
    for line in path.read_text(encoding="utf-8").splitlines():
        row = json.loads(line)
        if row["operation"] == "commit_stream":
            record = row["facts"]["json"]
            duration_keys = ("conversion_to_commit_us", "conversion_to_result_us")
            timing = record.get("timing")
            # Non-deterministic values may differ, but missing fields are
            # broken observations. Validate before replacing any values.
            if (
                "session_nonce" not in record
                or not isinstance(timing, dict)
                or any(key not in timing for key in duration_keys)
            ):
                raise ValueError(f"missing normalized trace fields: {path}")
            record["session_nonce"] = "<session>"
            # Keep source, sequence, scores, and every other fact comparable.
            for key in duration_keys:
                timing[key] = "<duration>"
        result.append(row)
    return result


def encode(value):
    # Preserve the encoded numeric representation: 1 differs from 1.0.
    return json.dumps(
        value, sort_keys=True, ensure_ascii=False, separators=(",", ":")
    ).encode()


def compare(before_dir, after_dir):
    expected = {name + ".jsonl" for name in case_names(trace=True)}
    for label, directory in (("before", before_dir), ("after", after_dir)):
        actual = {path.name for path in directory.glob("*.jsonl")}
        if actual != expected:
            raise ValueError(
                f"{label} trace cases differ: missing={sorted(expected - actual)}, "
                f"extra={sorted(actual - expected)}"
            )
    differences = []
    count = 0
    for name in sorted(expected):
        before, after = read(before_dir / name), read(after_dir / name)
        # Two empty files would otherwise compare equal with no observations.
        for label, rows in (("before", before), ("after", after)):
            if not rows:
                raise ValueError(f"{label} trace case is empty: {name}")
        count += len(before)
        for index in range(max(len(before), len(after))):
            left = before[index] if index < len(before) else None
            right = after[index] if index < len(after) else None
            if encode(left) != encode(right):
                differences.append(
                    {"case": name, "observation": index, "before": left, "after": right}
                )
    return {"cases": len(expected), "observations": count, "differences": differences}


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="muu-trace-test-")
        self.addCleanup(self.scratch.cleanup)
        self.before = Path(self.scratch.name) / "before"
        self.after = Path(self.scratch.name) / "after"
        self.before.mkdir()
        self.after.mkdir()
        self.rows = [
            {"operation": "marked_text", "facts": {"attributes": ["selected"], "cursor": 3}},
            {"operation": "ffi_request", "facts": {"request": "focus_next"}},
            {"operation": "learning", "facts": {"version": 1}},
            {
                "operation": "commit_stream",
                "facts": {"json": {
                    "session_nonce": "before",
                    "sequence": 1,
                    "score": 1,
                    "timing": {"conversion_to_commit_us": 10, "conversion_to_result_us": 20},
                }},
            },
        ]
        for directory in (self.before, self.after):
            for name in case_names(trace=True):
                self.write(directory / (name + ".jsonl"), self.rows)
        self.changed = self.after / "controller.jsonl"

    @staticmethod
    def write(path, rows):
        path.write_text(
            "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows),
            encoding="utf-8",
        )

    # Positive control: pins the current 23/19 split, so a case silently
    # dropped from CASES fails here, not only in a later trace comparison.
    def test_complete_current_case_set_matches(self):
        self.assertEqual(len(case_names()), 23)
        self.assertEqual(len(case_names(trace=True)), 19)
        self.assertEqual(
            compare(self.before, self.after),
            {"cases": 19, "observations": 19 * len(self.rows), "differences": []},
        )

    # The two asset groups split the cases with no overlap and no gap, for the
    # full set and the traced set alike, so running "fast" then "assets"
    # covers every case. The counts pin today's split; an unknown group fails.
    def test_asset_groups_partition_all_cases_without_losing_traces(self):
        for trace, fast_count, asset_count in ((False, 8, 15), (True, 5, 14)):
            fast = set(case_names(trace=trace, group="fast"))
            assets = set(case_names(trace=trace, group="assets"))
            self.assertEqual(len(fast), fast_count)
            self.assertEqual(len(assets), asset_count)
            self.assertFalse(fast & assets)
            self.assertEqual(fast | assets, set(case_names(trace=trace)))
        with self.assertRaises(ValueError):
            case_names(group="unknown")

    # Contract: the exact expected file set. The "both sides" rows catch the
    # old failure: equal but incomplete directories used to compare clean.
    def test_missing_or_extra_cases_are_rejected_even_on_both_sides(self):
        for sides in ((self.before,), (self.after,), (self.before, self.after)):
            for directory in sides:
                (directory / "controller.jsonl").unlink()
            with self.assertRaisesRegex(ValueError, "missing="):
                compare(self.before, self.after)
            for directory in sides:
                self.write(directory / "controller.jsonl", self.rows)
                self.write(directory / "unexpected.jsonl", self.rows)
            with self.assertRaisesRegex(ValueError, "extra="):
                compare(self.before, self.after)
            for directory in sides:
                (directory / "unexpected.jsonl").unlink()

    # Fail closed: an unreadable trace raises instead of counting as empty.
    def test_invalid_json_is_rejected(self):
        self.changed.write_text("{broken}\n", encoding="utf-8")
        with self.assertRaises(json.JSONDecodeError):
            compare(self.before, self.after)

    # Negative: one case or every case written empty, on either or both
    # sides. Both-sides-empty is the input that used to compare clean.
    def test_empty_cases_are_rejected_on_either_or_both_sides(self):
        for names in (("controller",), case_names(trace=True)):
            for sides in ((self.before,), (self.after,), (self.before, self.after)):
                with self.subTest(names=names, sides=sides):
                    for directory in sides:
                        for name in names:
                            self.write(directory / (name + ".jsonl"), [])
                    with self.assertRaisesRegex(ValueError, "trace case is empty"):
                        compare(self.before, self.after)
                    for directory in sides:
                        for name in names:
                            self.write(directory / (name + ".jsonl"), self.rows)

    # Negative: a record lacking the nonce, timing or either duration. The
    # normalization must not hide a missing field by replacing it, and must
    # fail with a clear error rather than a KeyError.
    def test_normalized_fields_must_exist_on_either_or_both_sides(self):
        fields = (
            ("session_nonce",), ("timing",),
            ("timing", "conversion_to_commit_us"),
            ("timing", "conversion_to_result_us"),
        )
        for fields_to_remove in fields:
            for sides in ((self.before,), (self.after,), (self.before, self.after)):
                rows = copy.deepcopy(self.rows)
                target = rows[3]["facts"]["json"]
                for field in fields_to_remove[:-1]:
                    target = target[field]
                del target[fields_to_remove[-1]]
                with self.subTest(fields=fields_to_remove, sides=sides):
                    for directory in sides:
                        self.write(directory / "controller.jsonl", rows)
                    with self.assertRaisesRegex(ValueError, "missing normalized trace fields"):
                        compare(self.before, self.after)
                    for directory in sides:
                        self.write(directory / "controller.jsonl", self.rows)

    # A dropped or added row is one difference, never ignored as a tail.
    def test_extra_or_missing_observations_are_differences(self):
        for rows in (self.rows[:-1], self.rows + self.rows[:1]):
            self.write(self.changed, rows)
            self.assertEqual(len(compare(self.before, self.after)["differences"]), 1)

    # Each mutated fact (marked attributes, cursor, FFI request, learning
    # version, record sequence, score 1 -> 1.0) must surface as exactly one
    # difference at its own row. The last proves numeric spelling counts.
    def test_public_effects_requests_learning_and_record_facts_are_compared(self):
        changes = (
            (0, "attributes", ["unselected"]),
            (0, "cursor", 4),
            (1, "request", "focus_previous"),
            (2, "version", 2),
            (3, "sequence", 2),
            (3, "score", 1.0),
        )
        for index, field, value in changes:
            rows = copy.deepcopy(self.rows)
            facts = rows[index]["facts"]
            (facts["json"] if index == 3 else facts)[field] = value
            self.write(self.changed, rows)
            differences = compare(self.before, self.after)["differences"]
            self.assertEqual(len(differences), 1, (index, field))
            self.assertEqual(differences[0]["observation"], index)

    # The normalization's only allowance: nonce and the two durations may
    # differ. Every other field stays compared by the test above.
    def test_only_nonce_and_measured_durations_are_normalized(self):
        rows = copy.deepcopy(self.rows)
        record = rows[3]["facts"]["json"]
        record["session_nonce"] = "after"
        record["timing"] = {"conversion_to_commit_us": 100, "conversion_to_result_us": 200}
        self.write(self.changed, rows)
        self.assertEqual(compare(self.before, self.after)["differences"], [])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("before", type=Path, nargs="?")
    parser.add_argument("after", type=Path, nargs="?")
    args = parser.parse_args()
    if args.self_test:
        if args.before is not None or args.after is not None:
            parser.error("--self-test takes no directories")
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ComparisonTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if args.before is None or args.after is None:
        parser.error("both before and after directories are required")
    result = compare(args.before, args.after)
    print(json.dumps(result, ensure_ascii=False))
    return 1 if result["differences"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
