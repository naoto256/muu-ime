#!/usr/bin/env python3
"""Run compiled Rust tests in disjoint synthetic, public-asset and scale groups."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "test-support/rust-test-groups.json"
ASSET_VARIABLES = ("MUU_REAL_DICT_PATH", "MUU_LM_MODEL_DIR", "MUU_ZENZ_MODEL_DIR")
spec = importlib.util.spec_from_file_location("counted_rust_gate", ROOT / "scripts/run-named-rust-test.py")
counted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(counted)


def key(row):
    return row["package"], row["target"], row["name"]


# test-support/rust-test-groups.json lists every assets and scale test by
# package, target and exact name; fast is everything else. A row's
# `requires` must be non-empty exactly for assets, so the two lists cannot be
# swapped, and one identity may appear only once.
def groups(document):
    if set(document) != {"format_version", "assets", "scale"} or document["format_version"] != 1:
        raise ValueError("unknown Rust test group manifest")
    result = {}
    seen = set()
    for group in ("assets", "scale"):
        rows = document[group]
        if not isinstance(rows, list) or not rows:
            raise ValueError("empty test group: " + group)
        names = set()
        for row in rows:
            if not isinstance(row, dict) or not {"package", "target", "name", "requires"} <= row.keys():
                raise ValueError("incomplete test group row")
            if any(not isinstance(row[field], str) or not row[field] for field in ("package", "target", "name")):
                raise ValueError("empty test identity")
            requirements = row["requires"]
            if not isinstance(requirements, list) or set(requirements) - {"dictionary", "edge", "zenz", "public_seed"}:
                raise ValueError("unknown asset requirement")
            if (group == "assets") != bool(requirements):
                raise ValueError("asset requirements disagree with test group")
            identity = key(row)
            if identity in seen:
                raise ValueError("duplicate test identity: " + str(identity))
            seen.add(identity)
            names.add(identity)
        result[group] = names
    return result


def test_names(output):
    names = output.splitlines()
    if any(not line.endswith(": test") for line in names):
        raise ValueError("unexpected compiled test listing")
    result = [line[:-6] for line in names]
    if len(result) != len(set(result)):
        raise ValueError("duplicate compiled test name")
    return result


# Compiles the test executables (only the declared targets for scale) and
# reads each one's full and ignored test lists. Doc-tests are not here; the
# fast recipe runs them separately.
def inventory(release, scopes=None):
    cargo = [sys.executable, str(ROOT / "scripts/rust_toolchain.py"), "cargo"]
    metadata = json.loads(subprocess.check_output(cargo + ["metadata", "--no-deps", "--format-version=1"], cwd=ROOT))
    packages = {item["id"]: item["name"] for item in metadata["packages"]}
    targets = [["--workspace"]]
    if scopes is not None:
        targets = []
        for package, target in sorted(scopes):
            if target == "lib":
                arguments = ["--lib"]
            else:
                kind, name = target.split(":", 1)
                if kind not in ("test", "bin"):
                    raise ValueError("unknown test target kind")
                arguments = ["--" + kind, name]
            targets.append(["-p", package, *arguments])
    artifacts = []
    for target in targets:
        command = cargo + ["test", *target, "--no-run", "--message-format=json"]
        if release:
            command.append("--release")
        built = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
        print(built.stderr, end="", file=sys.stderr)
        if built.returncode:
            print(built.stdout, end="", file=sys.stderr)
            raise RuntimeError("Rust test compilation failed")
        artifacts.extend(built.stdout.splitlines())
    result = []
    for line in artifacts:
        item = json.loads(line)
        if item.get("reason") != "compiler-artifact" or not item.get("executable") or not item["profile"]["test"]:
            continue
        target = item["target"]
        scope = "lib" if "lib" in target["kind"] else target["kind"][0] + ":" + target["name"]
        executable = item["executable"]
        names = test_names(subprocess.check_output([executable, "--list", "--format=terse"], text=True))
        ignored = test_names(subprocess.check_output([executable, "--list", "--ignored", "--format=terse"], text=True))
        result.append({"package": packages[item["package_id"]], "target": scope,
                       "executable": executable, "tests": names, "ignored": ignored})
    if not result:
        raise ValueError("no compiled test executables")
    return sorted(result, key=lambda row: (row["package"], row["target"]))


# Before anything runs: every declared test must exist, and the ignored set
# must be exactly assets plus scale. A new #[ignore] without a manifest row,
# or a row whose test lost #[ignore], fails here rather than going unrun.
def validate_inventory(binaries, declared):
    all_tests, ignored = set(), set()
    for binary in binaries:
        prefix = binary["package"], binary["target"]
        found = {(*prefix, name) for name in binary["tests"]}
        if all_tests & found:
            raise ValueError("duplicate compiled test executable")
        all_tests.update(found)
        ignored.update((*prefix, name) for name in binary["ignored"])
    expected = declared["assets"] | declared["scale"]
    if not expected <= all_tests:
        raise ValueError("declared tests are missing: " + str(sorted(expected - all_tests)))
    if ignored != expected:
        raise ValueError("ignored test set changed: " + str(sorted(ignored ^ expected)))
    return all_tests


# Every group sets MUU_REQUIRE_REAL_ASSETS=1, so a test that cannot find an
# asset fails instead of skipping and passing, as plain `cargo test` would.
def environment(group, original):
    result = dict(original, MUU_REQUIRE_REAL_ASSETS="1", RUST_TEST_THREADS="1")
    if group != "assets":
        # An accidental asset read must fail, even if the invoking shell has
        # model paths set. It must not silently return and count as passed.
        for variable in ASSET_VARIABLES:
            result.pop(variable, None)
    return result


# Runs a group with exact names and checks the completed count, so a stale
# name cannot pass as zero tests. Scale runs one test per process.
def run(group, release=False):
    declared = groups(json.loads(MANIFEST.read_text()))
    scopes = {identity[:2] for identity in declared["scale"]} if group == "scale" else None
    binaries = inventory(release, scopes)
    in_scope = {name: {identity for identity in identities if scopes is None or identity[:2] in scopes}
                for name, identities in declared.items()}
    all_tests = validate_inventory(binaries, in_scope)
    selected = all_tests - declared["assets"] - declared["scale"] if group == "fast" else declared[group]
    if not selected:
        raise ValueError("no tests selected")
    active = environment(group, os.environ)
    for binary in binaries:
        names = sorted(name for package, scope, name in selected
                       if (package, scope) == (binary["package"], binary["target"]))
        if not names:
            continue
        batches = [[name] for name in names] if group == "scale" else [names]
        for batch in batches:
            command = [binary["executable"], "--exact", "--test-threads=1", *batch]
            if group != "fast":
                command.append("--ignored")
            status = counted.run_counted(command, len(batch), active)
            if status:
                return status
    print(json.dumps({"group": group, "scope": "scale targets" if scopes else "workspace",
                      "passed": len(selected), "registered": len(all_tests),
                      "not_requested": len(all_tests) - len(selected)}, sort_keys=True))
    return 0


class GroupTests(unittest.TestCase):
    def setUp(self):
        self.asset = {"package": "p", "target": "lib", "name": "asset", "requires": ["dictionary"]}
        self.scale = {**self.asset, "name": "scale", "requires": []}
        self.document = {"format_version": 1, "assets": [self.asset], "scale": [self.scale]}
        self.binary = {"package": "p", "target": "lib", "tests": ["unit", "asset", "scale"],
                       "ignored": ["asset", "scale"]}

    # Negatives: a declared test that was not compiled, a declared test that is
    # not #[ignore], an #[ignore] test with no manifest row, and the same
    # executable listed twice.
    def test_disjoint_groups_require_complete_compiled_and_ignored_sets(self):
        declared = groups(self.document)
        self.assertEqual(len(validate_inventory([self.binary], declared)), 3)
        for changes in ({"tests": ["unit", "scale"]}, {"ignored": ["scale"]},
                        {"tests": self.binary["tests"] + ["unknown"],
                         "ignored": self.binary["ignored"] + ["unknown"]}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                validate_inventory([{**self.binary, **changes}], declared)
        with self.assertRaises(ValueError):
            validate_inventory([self.binary, self.binary], declared)

    # Negatives: an empty group, a repeated row, an asset row filed under scale,
    # and an asset row with no requirement.
    def test_empty_duplicate_or_misclassified_manifest_is_rejected(self):
        for changes in ({"assets": []}, {"assets": [self.asset, self.asset]},
                        {"scale": [self.asset]}, {"assets": [{**self.asset, "requires": []}]}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                groups({**self.document, **changes})

    # The terse listing is parsed strictly: a duplicate name, a benchmark line
    # or any other text fails rather than being skipped.
    def test_listing_must_contain_only_unique_test_names(self):
        self.assertEqual(test_names("a: test\nb: test\n"), ["a", "b"])
        for text in ("a: test\na: test\n", "a: benchmark\n", "unparsed output\n"):
            with self.assertRaises(ValueError):
                test_names(text)

    # Asset paths set in the caller's shell reach only the assets group, and
    # the shell cannot turn the require-assets switch off.
    def test_synthetic_groups_cannot_inherit_real_asset_paths(self):
        original = {variable: "fixture-path" for variable in ASSET_VARIABLES}
        original["MUU_REQUIRE_REAL_ASSETS"] = "0"
        for group in ("fast", "scale"):
            selected = environment(group, original)
            self.assertFalse(set(ASSET_VARIABLES) & selected.keys())
            self.assertEqual(selected["MUU_REQUIRE_REAL_ASSETS"], "1")
        self.assertTrue(set(ASSET_VARIABLES) <= environment("assets", original).keys())

    # Scale compiles only the declared targets and runs each test alone, with
    # --exact, --ignored and an expected count of one.
    def test_scale_keeps_one_exact_completed_test_per_process(self):
        document = {**self.document, "scale": [self.scale, {**self.scale, "name": "scale-two"}]}
        binary = {**self.binary, "executable": "compiled-fixture",
                  "tests": self.binary["tests"] + ["scale-two"],
                  "ignored": self.binary["ignored"] + ["scale-two"]}
        with mock.patch(__name__ + ".MANIFEST") as manifest, \
                mock.patch(__name__ + ".inventory", return_value=[binary]) as compiled, \
                mock.patch.object(counted, "run_counted", return_value=0) as completed:
            manifest.read_text.return_value = json.dumps(document)
            self.assertEqual(run("scale", release=True), 0)
        compiled.assert_called_once_with(True, {("p", "lib")})
        self.assertEqual(len(completed.call_args_list), 2)
        for call, name in zip(completed.call_args_list, ("scale", "scale-two")):
            self.assertEqual(call.args[0], ["compiled-fixture", "--exact", "--test-threads=1", name, "--ignored"])
            self.assertEqual(call.args[1], 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("group", choices=("fast", "assets", "scale"), nargs="?")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.group or args.release:
            parser.error("--self-test takes no group or profile")
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(GroupTests))
        return 0 if result.wasSuccessful() else 1
    if not args.group:
        parser.error("a test group is required")
    try:
        return run(args.group, args.release)
    except (ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print("Rust test group: " + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
