#!/usr/bin/env python3
"""Offline contracts for audit policies, synthetic Cargo metadata and crate archives."""

import copy
import io
import json
from pathlib import Path
import re
import shutil
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import rust_code_licenses as licenses


class LicenseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.files = {
            "LICENSE-MIT": b"MIT License\nCopyright 2026 Fixture contributors\nPermission is hereby granted.\n",
            "src/unicode_tables/LICENSE-UNICODE": b"Unicode fixture copyright and permission notice\n",
            "COPYRIGHT": b"Fixture copyright holder notice\n",
            "src/lib.rs": b"// Copyright 2026 Fixture contributors.\npub fn fixture() {}\n",
        }
        self.package = {
            "name": "fixture", "version": "1.2.3", "license": "MIT OR Apache-2.0", "features": ["std"],
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "directory": self.source, "archive": self.root / "fixture.crate",
        }
        self.write_archive()
        self.policy = {k: self.package[k] for k in ("name", "version", "license", "features", "source", "checksum")}
        self.policy.update(selected_license="MIT", license_file="LICENSE-MIT", files=[
            {"path": name, "sha256": licenses.digest(data)}
            for name, data in sorted(self.files.items()) if not name.endswith(".rs")
        ], copyrights=[{"path": "src/lib.rs", "line": 1, "sha256": licenses.digest(self.files["src/lib.rs"])}])
        self.workspace = [{"name": "muu-ffi", "version": "0.1.0", "license": "MIT"}]
        self.manifest = {"schema_version": 1, "root": "muu-ffi", "target": "aarch64-apple-darwin",
                         "profile": "release", "default_features": True, "workspace_crates": self.workspace,
                         "crates": [self.policy], "source_notices_sha256": ""}
        self.graph = {"workspace_crates": self.workspace, "crates": [self.package]}

    def write_archive(self):
        for name, data in self.files.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        with tarfile.open(self.package["archive"], "w:gz") as tar:
            for name, data in sorted(self.files.items()):
                member = tarfile.TarInfo("fixture-1.2.3/" + name)
                member.size = len(data)
                tar.addfile(member, io.BytesIO(data))
        self.package["checksum"] = licenses.digest(self.package["archive"].read_bytes())

    def generated(self):
        return licenses.plan(self.manifest, self.graph)

    def install_fixture(self):
        expected = self.generated()
        licenses.synchronize(self.root / licenses.CODE, expected, write=True)
        return json.loads(expected["manifest.json"])

    # The original is copied unchanged, the source copyright line is kept with
    # its path and line, and a second run gives identical bytes with no
    # machine-specific path in them.
    def test_original_bytes_copyright_and_determinism(self):
        first = self.generated()
        self.assertEqual(first["fixture-1.2.3/LICENSE-MIT"], self.files["LICENSE-MIT"])
        self.assertIn(b"// Copyright 2026 Fixture contributors.", first["SOURCE-NOTICES.txt"])
        self.assertIn(b"src/lib.rs:1", first["SOURCE-NOTICES.txt"])
        self.manifest = json.loads(first["manifest.json"])
        self.assertEqual(first, self.generated())
        self.assertNotIn(str(self.root).encode(), b"".join(first.values()))

    # Any change in a runtime package's version, license, source, checksum or
    # features fails instead of being accepted.
    def test_changed_runtime_identity_license_checksum_and_features(self):
        for field, value in [("version", "9.0.0"), ("license", "Apache-2.0"),
                             ("source", "git+unreviewed"), ("checksum", "0" * 64), ("features", [])]:
            with self.subTest(field=field):
                graph = copy.deepcopy(self.graph)
                graph["crates"][0][field] = value
                with self.assertRaises(licenses.LicenseError):
                    licenses.plan(self.manifest, graph)

    # The manifest's package set must equal Cargo's runtime set, in both
    # directions.
    def test_missing_or_added_normal_package(self):
        for crates in [[], [self.package, self.package | {"name": "new-crate"}]]:
            with self.assertRaisesRegex(licenses.LicenseError, "package set differs"):
                licenses.plan(self.manifest, self.graph | {"crates": crates})

    # Deleting a package from the manifest cannot hide it: Cargo still has it.
    def test_removed_manifest_entry_cannot_approve_an_omission(self):
        manifest = self.manifest | {"crates": []}
        with self.assertRaisesRegex(licenses.LicenseError, "package set differs"):
            licenses.plan(manifest, self.graph)

    # The archive must match its reviewed checksum, and the unpacked source
    # must match the archive.
    def test_changed_archive_and_unpacked_bytes_rejected(self):
        archive = self.package["archive"]
        archive.write_bytes(archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(licenses.LicenseError, "archive checksum"):
            self.generated()
        self.write_archive()
        (self.source / "LICENSE-MIT").write_bytes(b"changed unpacked license")
        with self.assertRaisesRegex(licenses.LicenseError, "unpacked license differs"):
            self.generated()

    # Extra carriers such as the Unicode table license and COPYRIGHT must be
    # listed; leaving one out fails.
    def test_unicode_and_copyright_carriers_cannot_be_omitted(self):
        for removed in ["src/unicode_tables/LICENSE-UNICODE", "COPYRIGHT"]:
            with self.subTest(removed=removed):
                policy = self.policy | {"files": [f for f in self.policy["files"] if f["path"] != removed]}
                with self.assertRaisesRegex(licenses.LicenseError, "missing additional license originals"):
                    licenses.plan(self.manifest | {"crates": [policy]}, self.graph)

    # The selected license's own text is required even when other license
    # files are present.
    def test_selected_license_body_is_required_even_with_other_carriers(self):
        self.policy["files"] = [f for f in self.policy["files"] if f["path"] != "LICENSE-MIT"]
        with self.assertRaisesRegex(licenses.LicenseError, "missing selected license original"):
            self.generated()

    # A new NOTICE in an updated archive fails even after the checksum is
    # updated, until the manifest lists it.
    def test_new_notice_requires_review_even_after_checksum_update(self):
        self.files["NOTICE"] = b"New upstream attribution\n"
        self.write_archive()
        self.policy["checksum"] = self.package["checksum"]
        with self.assertRaisesRegex(licenses.LicenseError, "missing additional license originals"):
            self.generated()

    # A source copyright line the manifest does not list fails rather than
    # being added by the generator.
    def test_missing_source_copyright_is_not_silently_generated(self):
        self.policy["copyrights"] = []
        with self.assertRaisesRegex(licenses.LicenseError, "omitted source copyright"):
            self.generated()

    # A changed copyright holder in the source fails until reviewed.
    def test_changed_source_holder_requires_review(self):
        self.files["src/lib.rs"] = b"// Copyright 2026 Another fixture holder.\npub fn fixture() {}\n"
        self.write_archive()
        self.policy["checksum"] = self.package["checksum"]
        with self.assertRaisesRegex(licenses.LicenseError, "copyright source changed"):
            self.generated()

    # Only reviewed simple OR expressions are accepted, and the choice must be
    # MIT or Apache-2.0 from the expression. AND, an unknown license or a
    # choice not offered all fail.
    def test_unreviewed_expression_and_invalid_choice_rejected(self):
        for expression, selected in [("MIT AND Unicode-3.0", "MIT"), ("GPL-3.0", "MIT"),
                                     ("Apache-2.0", "MIT"), ("MIT", "Unlicense")]:
            with self.assertRaises(licenses.LicenseError):
                licenses.choice(expression, selected)
        licenses.choice("MIT/Apache-2.0", "MIT")
        licenses.choice("Apache-2.0 OR BSL-1.0", "Apache-2.0")

    # `--check` changes nothing even when it fails. `--write` restores the
    # expected tree and removes the stray entry, and then a check passes.
    def test_check_is_read_only_and_write_prunes_only_the_license_tree(self):
        expected = self.generated()
        directory = self.root / "output"
        licenses.synchronize(directory, expected, write=True)
        before = {p: p.read_bytes() for p in directory.rglob("*") if p.is_file()}
        (directory / "extra").mkdir()
        with self.assertRaises(licenses.LicenseError):
            licenses.synchronize(directory, expected)
        self.assertEqual(before, {p: p.read_bytes() for p in directory.rglob("*") if p.is_file()})
        licenses.synchronize(directory, expected, write=True)
        licenses.synchronize(directory, expected)
        self.assertFalse((directory / "extra").exists())
        self.assertEqual(before, {p: p.read_bytes() for p in directory.rglob("*") if p.is_file()})

    # Unsafe relative paths, and a symlink inside the license tree, are
    # refused.
    def test_paths_and_symlinks_rejected(self):
        for path in ["../outside", "/absolute", "a/../b", "a//b", "./a", "a\\b", ""]:
            with self.assertRaises(licenses.LicenseError):
                licenses.relative(path)
        directory = self.root / "linked"
        directory.mkdir()
        (directory / "outside").symlink_to(self.source, target_is_directory=True)
        with self.assertRaisesRegex(licenses.LicenseError, "symlink"):
            licenses.synchronize(directory, self.generated(), write=True)

    # The allowlist gets one row per file and directory, no wildcard, and
    # keeps every unrelated row; regenerating it is a no-op.
    def test_exact_allowlist_preserves_other_contents_and_lists_directories(self):
        original = b"# fixture\nfile\tInfo.plist\ndirectory\t" + licenses.CONTENTS.encode() + b"\nfile\tResources/other.txt\n"
        expected = self.generated()
        output = licenses.allowlist_bytes(original, expected)
        self.assertEqual(output, licenses.allowlist_bytes(output, expected))
        self.assertIn(b"file\tInfo.plist\n", output)
        self.assertIn(b"file\tResources/other.txt\n", output)
        self.assertIn(("directory\t" + licenses.CONTENTS + "/fixture-1.2.3/src/unicode_tables\n").encode(), output)
        self.assertNotIn(b"*", output)
        self.assertEqual(len(expected), sum(line.startswith("file\t" + licenses.CONTENTS + "/") for line in output.decode().splitlines()))

    # The bundle check runs without Cargo. Deleting the same file from both
    # the source tree and the app still fails, because the manifest lists it.
    def test_bundle_does_not_need_cargo_and_detects_both_side_deletion(self):
        manifest = self.install_fixture()
        app = self.root / "MuuIME.app"
        bundle = app / "Contents" / licenses.CONTENTS
        shutil.copytree(self.root / licenses.CODE, bundle)
        with patch.object(licenses.subprocess, "check_output", side_effect=AssertionError("Cargo called")):
            licenses.check_bundle(self.root, app, manifest)
        relative = "fixture-1.2.3/LICENSE-MIT"
        for tree in [self.root / licenses.CODE, bundle]:
            (tree / relative).unlink()
        with self.assertRaisesRegex(licenses.LicenseError, "set differs"):
            licenses.check_bundle(self.root, app, manifest)

    # In the app, changed bytes, an extra directory and an extra file each fail.
    def test_bundle_rejects_changed_bytes_extra_directory_and_file(self):
        manifest = self.install_fixture()
        app = self.root / "MuuIME.app"
        bundle = app / "Contents" / licenses.CONTENTS
        shutil.copytree(self.root / licenses.CODE, bundle)
        target = bundle / "fixture-1.2.3/LICENSE-MIT"
        data = target.read_bytes()
        target.write_bytes(b"different")
        with self.assertRaisesRegex(licenses.LicenseError, "bytes differ"):
            licenses.check_bundle(self.root, app, manifest)
        target.write_bytes(data)
        for name, directory in [("extra-dir", True), ("extra.txt", False)]:
            path = bundle / name
            if directory:
                path.mkdir()
            else:
                path.write_text("extra")
            with self.assertRaisesRegex(licenses.LicenseError, "set differs"):
                licenses.check_bundle(self.root, app, manifest)
            path.rmdir() if directory else path.unlink()

    # An empty inventory, another target or another root fails.
    def test_manifest_rejects_empty_inventory_and_unsupported_scope(self):
        self.install_fixture()
        path = self.root / licenses.CODE / "manifest.json"
        for key, value in [("crates", []), ("target", "x86_64-apple-darwin"), ("root", "muu-dict-cli")]:
            path.write_bytes(licenses.canonical(self.manifest | {key: value}))
            with self.assertRaises(licenses.LicenseError):
                licenses.manifest_at(self.root)

    def test_selected_tree_excludes_tools_and_uses_target_features(self):
        # Metadata includes a dev feature and non-runtime packages. Only the
        # normal,no-proc-macro tree passed by Cargo selects runtime packages.
        def package(name, source, kinds):
            return {"name": name, "version": "0.1.0", "license": "MIT", "id": "opaque:" + name,
                    "source": source, "manifest_path": "/registry/src/index/" + name + "/Cargo.toml",
                    "features": {"dev-feature": []}, "targets": [{"kind": kinds}]}
        registry = self.package["source"]
        root = package("muu-ffi", None, ["staticlib"])
        normal = package("normal", registry, ["lib"])
        macro = package("macro", registry, ["proc-macro"])
        metadata = {"packages": [root, normal, macro] + [package(n, registry, ["lib"]) for n in ["dev", "build", "windows"]],
                    "workspace_members": [root["id"]]}
        lock = {("normal", "0.1.0", registry): "a" * 64}
        graph = licenses.select_runtime(metadata, "muu-ffi v0.1.0 (/workspace)\t\nnormal v0.1.0\tstd\n", lock)
        self.assertEqual(["normal"], [p["name"] for p in graph["crates"]])
        self.assertEqual(["std"], graph["crates"][0]["features"])
        with self.assertRaisesRegex(licenses.LicenseError, "proc-macro"):
            licenses.select_runtime(metadata, "muu-ffi v0.1.0\t\nmacro v0.1.0\t\n", lock)
        with self.assertRaisesRegex(licenses.LicenseError, "no muu-ffi root"):
            licenses.select_runtime(metadata, "", lock)

    # A registry source with no .cargo-checksum.json still gets its checksum,
    # computed from the cached .crate archive, which must equal the reviewed one.
    def test_registry_archive_without_optional_unpacked_checksum_record(self):
        directory = self.root / "registry/src/index/fixture-1.2.3"
        directory.mkdir(parents=True)
        archive = self.root / "registry/cache/index/fixture-1.2.3.crate"
        archive.parent.mkdir(parents=True)
        shutil.copyfile(self.package["archive"], archive)
        package = {"name": "fixture", "version": "1.2.3", "license": "MIT OR Apache-2.0",
                   "id": "opaque:fixture", "source": self.package["source"], "targets": [{"kind": ["lib"]}],
                   "manifest_path": str(directory / "Cargo.toml")}
        root = {"name": "muu-ffi", "version": "0.1.0", "license": "MIT", "id": "opaque:root",
                "source": None, "targets": [{"kind": ["staticlib"]}]}
        metadata = {"packages": [package, root], "workspace_members": [root["id"]]}
        graph = licenses.select_runtime(metadata, "muu-ffi v0.1.0\t\nfixture v1.2.3\tstd\n")
        self.assertFalse((directory / ".cargo-checksum.json").exists())
        self.assertEqual(self.policy["checksum"], graph["crates"][0]["checksum"])

    def test_advisory_ignores_match_across_audit_tools(self):
        root = Path(__file__).resolve().parents[1]
        deny_ignores = tomllib.loads((root / "deny.toml").read_text())["advisories"]["ignore"]
        audit_text = (root / ".cargo/audit.toml").read_text()
        audit_ignores = tomllib.loads(audit_text)["advisories"]["ignore"]
        self.assertCountEqual([entry["id"] for entry in deny_ignores], audit_ignores)
        for entry in deny_ignores:
            reason = entry["reason"]
            self.assertIsInstance(reason, str)
            self.assertTrue(reason.strip(), "ignored advisories need a reason")
            # cargo-audit accepts IDs, so keep each matching reason on its ID's line.
            self.assertRegex(audit_text, rf'(?m)^\s*"{re.escape(entry["id"])}"\s*,?\s*#\s*{re.escape(reason)}\s*$')

    # Cargo runs locked, offline and for the release target only.
    def test_cargo_calls_are_locked_offline_and_target_scoped(self):
        with patch.object(licenses.subprocess, "check_output", side_effect=[b"{}", ""]) as run:
            with patch.object(licenses, "select_runtime", return_value={}) as select:
                licenses.runtime(self.root)
        self.assertEqual(2, run.call_count)
        for call in run.call_args_list:
            arguments = call.args[0]
            self.assertIn("--locked", arguments)
            self.assertIn("--offline", arguments)
            self.assertIn("aarch64-apple-darwin", arguments)
            self.assertEqual("true", call.kwargs["env"]["CARGO_NET_OFFLINE"])
        self.assertIn("normal,no-proc-macro", run.call_args_list[1].args[0])
        select.assert_called_once_with({}, "")


if __name__ == "__main__":
    unittest.main()
