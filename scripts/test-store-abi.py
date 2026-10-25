#!/usr/bin/env python3
"""Run real Swift stores and C ABI across fresh processes, on self-written data."""

import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import zipfile


BUNDLE = "dev.muu.test.StoreABI"


def native_dictionary(identifier, reading, surface):
    return ("#!muu-dictionary\t1\n" + f"#id\t{identifier}\n#name\t接続の検体\n#version\t1\n"
            "#author\t自作\n#license\tCC0\n#provenance\t自作のテスト\n"
            f"reading\tsurface\tpos\tpriority\n{reading}\t{surface}\tNOUN\thigh\n").encode()


def postal_zip(path, code, town):
    csv_text = io.StringIO()
    csv.writer(csv_text, quoting=csv.QUOTE_ALL, lineterminator="\r\n").writerow([
        "13101", "100", code, "トウキョウト", "シケンク", "マチ",
        "東京都", "試験区", town, "0", "0", "0", "0", "0", "0"])
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr(zipfile.ZipInfo("utf_ken_all.csv", date_time=(2026, 1, 1, 0, 0, 0)), csv_text.getvalue().encode())


def tree(root):
    return {p.relative_to(root).as_posix():
            ("file", hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_mtime_ns) if p.is_file()
            else ("directory", p.stat().st_mtime_ns) for p in root.rglob("*")}


class StoreABITests(unittest.TestCase):
    consumer: Path
    dictionary: Path
    scratch_parent: Path

    def setUp(self):
        # Foundation standardizes /private/var back to /var; the latter is a
        # symlink and is intentionally refused by the production FileSafety.
        # Use an explicitly supplied workspace directory, not the system TMP.
        self.temporary = tempfile.TemporaryDirectory(prefix="muu-store-abi.", dir=self.scratch_parent)
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve()
        self.root = self.base / "storage"
        self.root.mkdir()
        self.fixtures = self.base / "fixtures"
        self.fixtures.mkdir()
        self.user = self.root / BUNDLE / "user_dict/user-dictionary.tsv"
        self.imported = self.root / BUNDLE / "imported_dict/v2/current"
        self.postal = self.root / BUNDLE / "postal_code/v1/current"
        (self.fixtures / "one.muudict").write_bytes(native_dictionary("org.example.abi", "あぶ", "自作取り込み語"))
        (self.fixtures / "next.muudict").write_bytes(native_dictionary("org.example.next", "あべ", "中断取り込み語"))
        seed = "# muu-postal-code\t1\n1000001\t東京都試験区旧町\n".encode()
        (self.fixtures / "seed.tsv").write_bytes(seed)
        manifest = {"schema_version": 1, "entry_count": 1,
                    "snapshot_sha256": hashlib.sha256(seed).hexdigest(),
                    "zip_sha256": "1" * 64, "csv_sha256": "2" * 64,
                    "source_url": "https://www.post.japanpost.jp/service/search/zipcode/download/utf/zip/utf_ken_all.zip",
                    "source_page_url": "https://www.post.japanpost.jp/service/search/zipcode/download/utf-zip.html",
                    "source_last_modified": "Thu, 01 Jan 2026 00:00:00 GMT", "retrieved_at": "2026-01-01T00:00:00Z"}
        (self.fixtures / "seed.json").write_text(json.dumps(manifest, ensure_ascii=False))
        postal_zip(self.fixtures / "update.zip", "1000002", "新町")
        postal_zip(self.fixtures / "next.zip", "1000003", "次町")

    # One fresh child process per step, so the next step sees only what the
    # previous one left on disk. A hard-exit step must exit 86 and print the
    # phase it stopped at; any other step must exit 0 and print its pass line.
    def run_child(self, mode, phase=None):
        child = subprocess.run([str(self.consumer), mode, str(self.root), str(self.dictionary), str(self.fixtures)],
                               capture_output=True, text=True, timeout=90)
        self.assertEqual(child.returncode, 86 if phase else 0, f"{mode}: {child.stdout}\n{child.stderr}")
        expected = f"hard-exit: {phase}" if phase else f"store ABI: {mode} passed"
        self.assertIn(expected, child.stdout.splitlines())

    def test_default_off_factory_does_not_create_storage(self):
        self.run_child("factory")
        self.assertEqual(tree(self.root), {})

    def test_first_load_is_background_and_gated_then_seed_survives_restart(self):
        self.run_child("first-load")
        self.assertTrue(self.postal.is_file())
        before = tree(self.root)
        self.run_child("read-seed")
        self.assertEqual(tree(self.root), before, "a successful read must not save or collect")

    def test_three_real_stores_persist_and_convert_in_the_next_process(self):
        self.run_child("write")
        for path in (self.user, self.imported, self.postal):
            self.assertTrue(path.is_file())
        before = tree(self.root)
        self.run_child("read")
        self.assertEqual(tree(self.root), before)

    # Stops the child after the new user file was renamed into place and
    # before Core's publish. The user store writes first, so the next process
    # must read the complete new file and convert with it. Reading it must
    # change nothing on disk.
    def test_hard_exit_after_user_rename_loads_the_complete_new_file(self):
        self.run_child("write")
        old = self.user.read_bytes()
        self.run_child("interrupt-user", "user_after_rename_before_publish")
        self.assertNotEqual(self.user.read_bytes(), old)
        before = tree(self.root)
        self.run_child("read-user-interrupted")
        self.assertEqual(tree(self.root), before)

    # Stops the child after Core acknowledged the new generation and before the
    # `current` pointer was rewritten. The hash-named blob and generation are
    # already staged, but `current` still names the old generation, so the
    # next process must use the complete old dictionary, not the new one. A
    # read must not collect the staged files either.
    def test_hard_exit_after_imported_ack_keeps_the_old_pointer(self):
        self.run_child("write")
        old = self.imported.read_bytes()
        files_before = set(tree(self.root))
        self.run_child("interrupt-import", "imported_after_ack_before_current")
        self.assertEqual(self.imported.read_bytes(), old)
        self.assertGreater(len(set(tree(self.root)) - files_before), 0, "pre-ACK blob/generation files must have been staged")
        before = tree(self.root)
        self.run_child("read")
        self.assertEqual(tree(self.root), before, "restart must not collect recovery/orphan files just by reading")

    # Stops the child after Core acknowledged the new snapshot and before any
    # generation or `current` was written. Postal writes nothing before the
    # ACK, so the disk must be unchanged, and the next process must use the
    # complete old snapshot (新町, not 次町).
    def test_hard_exit_after_postal_ack_keeps_the_old_generation(self):
        self.run_child("write")
        before = tree(self.root)
        self.run_child("interrupt-postal", "postal_after_ack_before_generation")
        self.assertEqual(tree(self.root), before)
        self.run_child("read")
        self.assertEqual(tree(self.root), before)

    def test_corrupt_files_are_read_only_and_never_automatically_rewritten(self):
        for path in (self.user, self.imported, self.postal):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"self-written malformed state\n")
        before = tree(self.root)
        self.run_child("readonly")
        self.assertEqual(tree(self.root), before)

    def test_failed_load_remains_gated_but_postal_explicit_update_recovers(self):
        self.run_child("failed")
        self.assertTrue(self.postal.is_file())

    def test_read_only_postal_recovers_only_after_an_explicit_update(self):
        self.postal.parent.mkdir(parents=True)
        self.postal.write_bytes(b"invalid current\n")
        self.run_child("recover-postal")
        self.assertNotEqual(self.postal.read_bytes(), b"invalid current\n")

    # A v1 catalog migrates only on an explicit edit, and keeps its content.
    # Each step is a new process:
    # 1. legacy-load: read the catalog; the imported tree is untouched and the
    #    word converts.
    # 2. legacy-disable: the first edit writes the current pointer.
    # 3. legacy-read-disabled: still disabled, no candidate, disk unchanged.
    # 4. legacy-enable: re-enable from the migrated blob.
    # 5. legacy-read-enabled: still enabled, converts, disk unchanged.
    # Metadata, count, import time and source SHA are checked at every step.
    # Only steps 4 and 5 prove that the surface survived migration; a
    # disabled list alone cannot tell.
    def test_legacy_imported_catalog_migrates_on_explicit_edit_only(self):
        legacy = self.root / BUNDLE / "imported_dict/catalog-v1.json"
        legacy.parent.mkdir(parents=True)
        source = (self.fixtures / "one.muudict").read_bytes()
        catalog = {"schema_version": 1, "dictionaries": [{
            "metadata": {"id": "org.example.abi", "name": "接続の検体", "version": "1",
                         "author": "自作", "license": "CC0", "provenance": "自作のテスト"},
            "enabled": True, "imported_at_unix_ms": 123, "source_sha256": hashlib.sha256(source).hexdigest(), "order": 0,
            "entries": [{"reading": "あぶ", "surface": "自作取り込み語", "partOfSpeech": "NOUN", "priority": "high"}]}]}
        legacy.write_text(json.dumps(catalog, ensure_ascii=False))
        original_imported = tree(legacy.parent)
        self.run_child("legacy-load")
        self.assertFalse(self.imported.exists())
        self.assertEqual(tree(legacy.parent), original_imported)
        self.assertEqual(legacy.read_text(), json.dumps(catalog, ensure_ascii=False))
        self.run_child("legacy-disable")
        self.assertTrue(self.imported.is_file())
        before = tree(self.root)
        self.run_child("legacy-read-disabled")
        self.assertEqual(tree(self.root), before)
        # Reload the disabled blob in a new Core, enable it, and commit its
        # original surface. A disabled list alone cannot detect lost content.
        self.run_child("legacy-enable")
        enabled = tree(self.root)
        self.run_child("legacy-read-enabled")
        self.assertEqual(tree(self.root), enabled)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--consumer", type=Path, required=True)
    parser.add_argument("--dictionary", type=Path, required=True)
    parser.add_argument("--scratch-parent", type=Path, required=True)
    args = parser.parse_args()
    if not args.consumer.is_file() or not args.dictionary.is_file():
        parser.error("the compiled consumer and public dictionary are required")
    StoreABITests.consumer = args.consumer.resolve()
    StoreABITests.dictionary = args.dictionary.resolve()
    StoreABITests.scratch_parent = args.scratch_parent.resolve()
    if not StoreABITests.scratch_parent.is_dir():
        parser.error("an existing private workspace scratch parent is required")
    unittest.main(argv=[__file__], verbosity=2)
