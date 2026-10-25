#!/usr/bin/env python3
"""Run a named Cargo test gate and reject a successful empty filter."""

import contextlib
import io
import os
import re
import subprocess
import sys
import unittest
from unittest import mock

from rust_toolchain import tool_command


def run_counted(command, expected, environment=None):
    # Test stdout could otherwise forge a libtest summary. Keep successful
    # tests captured, including when libtest is configured through the env.
    uncaptured_options = {"--nocapture", "--no-capture", "--show-output"}
    active_environment = os.environ if environment is None else environment
    if (
        any(argument.split("=", 1)[0] in uncaptured_options for argument in command)
        # libtest disables capture for any value other than "0", empty included.
        or active_environment.get("RUST_TEST_NOCAPTURE", "0") != "0"
    ):
        print("named Rust gate: test output must remain captured", file=sys.stderr)
        return 2
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env=environment,
    )
    passed = 0
    assert process.stdout is not None
    for line in process.stdout:
        print(line, end="", flush=True)
        # Passing tests' output stays captured, so it cannot add summaries.
        # libtest still prints a failing test's output, but that run exits
        # nonzero, and the status check below returns before any counting.
        result = re.match(r"test result: ok\. (\d+) passed;", line)
        if result:
            passed += int(result[1])
    status = process.wait()
    process.stdout.close()
    if status:
        return status
    if passed != expected:
        print(f"named Rust gate: expected {expected} passed, observed {passed}", file=sys.stderr)
        return 1
    return 0


class CountedGateTests(unittest.TestCase):
    def fake_cargo(self, output, expected, status=0):
        command = [
            sys.executable, "-c",
            "import sys; sys.stdout.write(sys.argv[1]); sys.exit(int(sys.argv[2]))",
            output, str(status),
        ]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return run_counted(command, expected)

    # Contract: success means exactly N completed tests. Too few (a renamed
    # test) and too many (a filter matching extra tests) both fail.
    def test_success_requires_the_exact_completed_count(self):
        output = "running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n"
        self.assertEqual(self.fake_cargo(output, 2), 0)
        self.assertNotEqual(self.fake_cargo(output, 1), 0)
        self.assertNotEqual(self.fake_cargo(output, 3), 0)

    # Negatives for the gap this wrapper exists to close: no output, a
    # 0-passed filter, --list discovery output, and a run that never finished.
    def test_empty_filter_and_discovery_only_cannot_pass(self):
        outputs = (
            "",
            "test result: ok. 0 passed; 0 failed; 0 ignored; 507 filtered out;\n",
            "some::test: test\n1 test, 0 benchmarks\n",
            "running 1 test\n",
        )
        for output in outputs:
            self.assertNotEqual(self.fake_cargo(output, 1), 0, output)

    # Cargo's own status wins and is passed through unchanged.
    def test_nonzero_exit_cannot_be_hidden_by_a_success_summary(self):
        output = "test result: ok. 1 passed; 0 failed;\n"
        self.assertEqual(self.fake_cargo(output, 1, status=7), 7)

    # Unit, integration and doctest binaries each print a summary.
    def test_multiple_binaries_and_doctests_are_summed(self):
        output = (
            "test result: ok. 1 passed; 0 failed;\n"
            "test result: ok. 2 passed; 0 failed;\n"
            "test result: ok. 0 passed; 0 failed;\n"
        )
        self.assertEqual(self.fake_cargo(output, 3), 0)
        self.assertNotEqual(self.fake_cargo(output, 4), 0)

    # Even with exit 0, a FAILED summary adds nothing to the count.
    def test_failed_summary_does_not_count_as_success(self):
        output = "test result: FAILED. 1 passed; 1 failed;\n"
        self.assertNotEqual(self.fake_cargo(output, 1), 0)

    # An expected count of 0 would reopen the empty-filter hole by design.
    def test_invalid_expected_counts_are_rejected_before_running_cargo(self):
        for arguments in ([], ["0", "unused"], ["-1", "unused"], ["one", "unused"]):
            result = subprocess.run(
                [sys.executable, __file__, *arguments], capture_output=True, text=True
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("usage:", result.stderr)

    # Contract: a gate whose test output could forge summaries never starts.
    # Each spelling (and `=value`) and each enabling env value is a negative;
    # the last case is the positive control that "0" still runs normally.
    def test_uncaptured_output_is_rejected_before_launch(self):
        for option in ("--nocapture", "--no-capture", "--show-output", "--nocapture=true"):
            with self.subTest(option=option), mock.patch("subprocess.Popen") as launch:
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(run_counted(["cargo", "test", "--", option], 1, {}), 2)
                launch.assert_not_called()
        for value in ("", "1", "true"):
            with self.subTest(environment=value), mock.patch("subprocess.Popen") as launch:
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(run_counted(["cargo", "test"], 1, {"RUST_TEST_NOCAPTURE": value}), 2)
                launch.assert_not_called()
        output = "test result: ok. 1 passed; 0 failed;\n"
        with mock.patch.dict(os.environ, {"RUST_TEST_NOCAPTURE": "0"}):
            self.assertEqual(self.fake_cargo(output, 1), 0)


def main():
    if sys.argv[1:] == ["--self-test"]:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(CountedGateTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if len(sys.argv) < 3 or not sys.argv[1].isdigit() or int(sys.argv[1]) < 1:
        sys.exit("usage: run-named-rust-test.py <expected-passed> <cargo-test-args...>")
    # Count completed tests, not discovery output or a successful empty filter.
    command, environment = tool_command("cargo", ["test", *sys.argv[2:]])
    return run_counted(command, int(sys.argv[1]), environment)


if __name__ == "__main__":
    sys.exit(main())
