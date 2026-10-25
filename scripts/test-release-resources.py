#!/usr/bin/env python3
"""Exercise release resource failures with small, self-authored bundles."""

import hashlib
import importlib.util
import json
from pathlib import Path
import plistlib
import shutil
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("release_resources", Path(__file__).with_name("verify-release-resources.py"))
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


# A small self-authored bundle and repository that pass as built; each test
# breaks one thing and expects the verifier's reason for it.
class Bundle:
    def __init__(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="muu-release-resources-")
        self.root = Path(self.scratch.name).resolve()
        self.repository = self.root / "repository"
        self.app = self.root / "MuuIME.app"
        self.resources = self.app / "Contents/Resources"
        self.seed = self.repository / "apple/MuuIME/PostalCodeSeed"
        for path in (self.resources, self.seed, self.repository / "dict-src", self.app / "Contents/MacOS"):
            path.mkdir(parents=True)
        # Use the same data authority as the real bundle check. Unrelated
        # resource bytes are placeholders; their content verifiers run elsewhere.
        self.authority = self.repository / "test-support/release-bundle-contents.tsv"
        self.authority.parent.mkdir()
        self.authority.write_bytes((Path(__file__).resolve().parents[1] / "test-support/release-bundle-contents.tsv").read_bytes())
        for kind, name in sorted(verifier.allowed_contents(self.repository)):
            path = self.app / "Contents" / name
            if kind == "directory":
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"synthetic packing member")
        self.dictionary = b"synthetic dictionary authority"
        (self.resources / "mozc-oss.muudict").write_bytes(self.dictionary)
        self.pin = self.repository / "dict-src/mozc-oss.sha256"
        self.pin.write_text(hashlib.sha256(self.dictionary).hexdigest() + "  fixture\n")
        self.tsv = b"# muu-postal-code\t1\n1000001\tself-authored fixture address\n"
        self.manifest = {"schema_version": 1, "entry_count": 1,
                         "snapshot_sha256": hashlib.sha256(self.tsv).hexdigest()}
        for directory in (self.seed, self.resources):
            (directory / "postal-code-seed.tsv").write_bytes(self.tsv)
            (directory / "postal-code-seed-manifest.json").write_text(json.dumps(self.manifest))
        self.info = self.app / "Contents/Info.plist"
        self.write_info()
        self.settings = self.root / "build-settings.json"
        self.write_settings()
        (self.app / "Contents/MacOS/MuuIME").write_bytes(b"synthetic executable; not launched")
        (self.app / "Contents/MacOS/MuuIME").chmod(0o755)

    def write_info(self, **changes):
        self.info.write_bytes(plistlib.dumps({"MuuBuildVariant": "standard", "LSMinimumSystemVersion": "15.0",
                                             "CFBundleExecutable": "MuuIME", **changes}))

    def write_settings(self, **changes):
        self.settings.write_text(json.dumps([{"target": "MuuIME", "buildSettings": {
            "CONFIGURATION": "Release", "ARCHS": "arm64", "MACOSX_DEPLOYMENT_TARGET": "15.0",
            "MUU_BUILD_VARIANT": "standard",
            **changes,
        }}]))

    def verify(self, expected_variant="standard"):
        return verifier.verify(self.app, self.repository, self.settings, expected_variant)

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.scratch.cleanup()


class ReleaseResourcesTests(unittest.TestCase):
    # The checked-in plist itself keeps both termination shortcuts off, so the
    # delegate's final flush runs at restart and logout.
    def test_source_uses_delegate_driven_termination(self):
        source = Path(__file__).resolve().parents[1] / "apple/MuuIME/Info.plist"
        info = plistlib.loads(source.read_bytes())
        for key in ("NSSupportsSuddenTermination", "NSSupportsAutomaticTermination"):
            self.assertIs(info.get(key, False), False, key)

    # Either shortcut set to true fails the release check, for standard and
    # dogfood alike: dogfood also keeps learning, so it needs the same flush.
    def test_termination_shortcuts_are_rejected_in_both_variants(self):
        for variant in ("standard", "dogfood"):
            for key in ("NSSupportsSuddenTermination", "NSSupportsAutomaticTermination"):
                with self.subTest(variant=variant, key=key), Bundle() as bundle:
                    bundle.write_info(MuuBuildVariant=variant, **{key: True})
                    if variant == "dogfood":
                        bundle.write_settings(SWIFT_ACTIVE_COMPILATION_CONDITIONS="MUU_DOGFOOD", MUU_BUILD_VARIANT=variant)
                    with self.assertRaisesRegex(ValueError, "termination"):
                        bundle.verify(variant)

    # Dogfood passes only when asked for and when plist, variant setting and
    # Swift condition all agree. The negatives: the distribution verifier given
    # a dogfood bundle, a missing condition, MUU_TEST alongside it, a standard
    # plist, and an unknown variant name.
    def test_dogfood_requires_explicit_matching_metadata_and_compiled_capability(self):
        with Bundle() as bundle:
            bundle.write_info(MuuBuildVariant="dogfood")
            bundle.write_settings(MUU_BUILD_VARIANT="dogfood", SWIFT_ACTIVE_COMPILATION_CONDITIONS="MUU_DOGFOOD")
            bundle.verify("dogfood")
            with self.assertRaises(ValueError):
                bundle.verify()  # The distribution verifier never infers dogfood.
            for changes in ({}, {"SWIFT_ACTIVE_COMPILATION_CONDITIONS": ""},
                            {"SWIFT_ACTIVE_COMPILATION_CONDITIONS": "MUU_DOGFOOD MUU_TEST"}):
                bundle.write_settings(MUU_BUILD_VARIANT="dogfood", **changes)
                with self.assertRaises(ValueError):
                    bundle.verify("dogfood")
            bundle.write_settings(MUU_BUILD_VARIANT="dogfood", SWIFT_ACTIVE_COMPILATION_CONDITIONS="MUU_DOGFOOD")
            bundle.write_info(MuuBuildVariant="standard")
            with self.assertRaisesRegex(ValueError, "dogfood variant"):
                bundle.verify("dogfood")
            with self.assertRaisesRegex(ValueError, "unknown build variant"):
                bundle.verify("unapproved")

    # Members the authority does not list: a file, an empty directory, and a
    # file next to Info.plist, outside Resources.
    def test_unexpected_file_directory_and_contents_sibling_are_rejected(self):
        for kind, name in (("file", "Resources/unexpected.txt"),
                           ("directory", "Resources/unexpected-directory"),
                           ("file", "unexpected-next-to-Info.plist")):
            with self.subTest(kind=kind, name=name), Bundle() as bundle:
                path = bundle.app / "Contents" / name
                if kind == "directory":
                    path.mkdir()
                else:
                    path.write_bytes(b"unreviewed content")
                with self.assertRaisesRegex(ValueError, "bundle contents differ.*extra="):
                    bundle.verify()

    # Each of the authority's entries removed in turn. Removing Resources
    # fails at its lstat before the set comparison, so FileNotFoundError also
    # counts.
    def test_every_allowed_file_and_directory_is_required(self):
        with Bundle() as baseline:
            entries = verifier.allowed_contents(baseline.repository)
        for kind, name in sorted(entries):
            with self.subTest(kind=kind, name=name), Bundle() as bundle:
                path = bundle.app / "Contents" / name
                if kind == "directory":
                    shutil.rmtree(path)
                else:
                    path.unlink()
                with self.assertRaises((ValueError, FileNotFoundError)):
                    bundle.verify()

    # Authorities that must be refused rather than read loosely: empty, a
    # duplicate, an unknown kind, absolute, `..` or `./` paths, a file whose
    # parent directory is not listed, and a line without two columns.
    def test_invalid_contents_authority_is_rejected(self):
        for text in ("", "file\tInfo.plist\nfile\tInfo.plist\n", "link\tInfo.plist\n",
                     "file\t/Info.plist\n", "file\t../Info.plist\n", "file\t./Info.plist\n",
                     "file\tMacOS/MuuIME\n", "not-two-columns\n"):
            with self.subTest(text=text), Bundle() as bundle:
                bundle.authority.write_text(text)
                with self.assertRaises(ValueError):
                    bundle.verify()

    # An executable without its exec bit, or a plist naming another executable:
    # either app would fail to launch.
    def test_executable_is_launchable_and_matches_bundle_info(self):
        with Bundle() as bundle:
            (bundle.app / "Contents/MacOS/MuuIME").chmod(0o600)
            with self.assertRaisesRegex(ValueError, "owner-executable"):
                bundle.verify()
        with Bundle() as bundle:
            bundle.write_info(CFBundleExecutable="Other")
            with self.assertRaisesRegex(ValueError, "executable must be MuuIME"):
                bundle.verify()

    # Test or diagnostic code compiled into Swift, C or Objective-C, whatever
    # its names: each settings key, -D written joined, split or as a JSON
    # list, and MUU_TEST=0, which `#if defined` still treats as on. The two
    # GCC_PREPROCESSOR_DEFINITIONS cases came from the review probe that the
    # Swift-only check accepted. MUU_INPUT_PATH_DIAGNOSTICS is the retired
    # diagnostic flag, still refused.
    def test_effective_release_flags_reject_test_only_entries_regardless_of_their_name(self):
        for key, value in (("SWIFT_ACTIVE_COMPILATION_CONDITIONS", "MUU_TEST"),
                           ("SWIFT_ACTIVE_COMPILATION_CONDITIONS", "MUU_DOGFOOD"),
                           ("GCC_PREPROCESSOR_DEFINITIONS", "MUU_DOGFOOD=1"),
                           ("SWIFT_ACTIVE_COMPILATION_CONDITIONS", "MUU_INPUT_PATH_DIAGNOSTICS"),
                           ("OTHER_SWIFT_FLAGS", "-DMUU_TEST"),
                           ("OTHER_SWIFT_FLAGS", "-D MUU_TEST"),
                           ("OTHER_SWIFT_FLAGS", ["-D", "MUU_TEST"]),
                           ("GCC_PREPROCESSOR_DEFINITIONS", "MUU_TEST=1"),
                           ("GCC_PREPROCESSOR_DEFINITIONS", "$(inherited) MUU_INPUT_PATH_DIAGNOSTICS=1"),
                           ("GCC_PREPROCESSOR_DEFINITIONS", ["MUU_TEST=0"]),
                           ("GCC_PREPROCESSOR_DEFINITIONS_NOT_USED_IN_PRECOMPS", ["MUU_TEST=1"]),
                           ("OTHER_CFLAGS", "-DMUU_TEST=1"),
                           ("OTHER_CPLUSPLUSFLAGS", ["-D", "MUU_INPUT_PATH_DIAGNOSTICS"])):
            with self.subTest(key=key, value=value), Bundle() as bundle:
                bundle.write_settings(**{key: value})
                with self.assertRaisesRegex(ValueError, "test or diagnostic compilation flags"):
                    bundle.verify()

    # A Debug, universal or older-target build, and settings output that
    # does not name exactly one MuuIME target.
    def test_release_settings_must_describe_one_arm64_macos15_release_target(self):
        for key, value in (("CONFIGURATION", "Debug"), ("ARCHS", "arm64 x86_64"),
                           ("MACOSX_DEPLOYMENT_TARGET", "14.0")):
            with self.subTest(key=key), Bundle() as bundle:
                bundle.write_settings(**{key: value})
                with self.assertRaisesRegex(ValueError, "release build settings differ"):
                    bundle.verify()
        for records in ({}, [], [{"target": "MuuIME"}],
                        [{"target": "MuuIME", "buildSettings": {}}] * 2):
            with self.subTest(records=records), Bundle() as bundle:
                bundle.settings.write_text(json.dumps(records))
                with self.assertRaises(ValueError):
                    bundle.verify()

    # Valid plist or JSON of the wrong shape must be rejected, not crash.
    def test_non_object_plist_and_manifest_are_rejected(self):
        with Bundle() as bundle:
            bundle.info.write_bytes(plistlib.dumps([]))
            with self.assertRaisesRegex(ValueError, "Info.plist must be a dictionary"):
                bundle.verify()
        with Bundle() as bundle:
            (bundle.seed / "postal-code-seed-manifest.json").write_text("[]")
            with self.assertRaisesRegex(ValueError, "manifest must be an object"):
                bundle.verify()

    # The positive: a correct bundle passes and the check writes nothing.
    def test_canonical_pair_and_dictionary_are_accepted_without_writing(self):
        with Bundle() as bundle:
            before = {str(p.relative_to(bundle.root)): p.read_bytes() for p in bundle.root.rglob("*") if p.is_file()}
            self.assertEqual(bundle.verify(), {"dictionary_artifact_sha256": hashlib.sha256(bundle.dictionary).hexdigest(),
                                              "postal_entry_count": 1, "postal_snapshot_sha256": bundle.manifest["snapshot_sha256"]})
            self.assertEqual({str(p.relative_to(bundle.root)): p.read_bytes() for p in bundle.root.rglob("*") if p.is_file()}, before)

    # A bundle missing the dictionary, the seed snapshot or its manifest.
    def test_every_required_data_member_is_required(self):
        for name in ("mozc-oss.muudict", "postal-code-seed.tsv", "postal-code-seed-manifest.json"):
            with self.subTest(name=name), Bundle() as bundle:
                (bundle.resources / name).unlink()
                with self.assertRaisesRegex(ValueError, "bundle contents differ"):
                    bundle.verify()

    # A different dictionary in place of the pinned artifact.
    def test_another_dictionary_is_rejected(self):
        with Bundle() as bundle:
            (bundle.resources / "mozc-oss.muudict").write_bytes(b"different dictionary")
            with self.assertRaisesRegex(ValueError, "dictionary artifact fingerprint"):
                bundle.verify()

    # A bundled seed that no longer matches the source: changed snapshot
    # bytes, or a manifest with another count.
    def test_a_stale_or_swapped_seed_pair_is_rejected(self):
        for member in ("snapshot", "manifest"):
            with self.subTest(member=member), Bundle() as bundle:
                if member == "snapshot":
                    (bundle.resources / "postal-code-seed.tsv").write_bytes(bundle.tsv.replace(b"1000001", b"1000002"))
                else:
                    manifest = {**bundle.manifest, "entry_count": 2}
                    (bundle.resources / "postal-code-seed-manifest.json").write_text(json.dumps(manifest))
                with self.assertRaisesRegex(ValueError, "differs from the canonical source"):
                    bundle.verify()

    # The authorities themselves are broken: the source manifest disagrees
    # with its TSV (count or hash), or the dictionary pin is not a SHA-256.
    def test_the_source_pair_and_pin_must_be_self_consistent(self):
        for failure in ("count", "hash", "pin"):
            with self.subTest(failure=failure), Bundle() as bundle:
                if failure == "pin":
                    bundle.pin.write_text("invalid\n")
                    reason = "dictionary authority is malformed"
                else:
                    manifest = {**bundle.manifest, "entry_count": 2} if failure == "count" else {**bundle.manifest, "snapshot_sha256": "0" * 64}
                    (bundle.seed / "postal-code-seed-manifest.json").write_text(json.dumps(manifest))
                    reason = "source seed hash or count is inconsistent"
                with self.assertRaisesRegex(ValueError, reason):
                    bundle.verify()

    # The retired diagnostic variant (or any non-standard one), or a bundle
    # that claims an older macOS.
    def test_nonstandard_or_older_bundles_are_rejected(self):
        for changes, reason in (({"MuuBuildVariant": "input-path-diagnostic"}, "standard variant"),
                                ({"LSMinimumSystemVersion": "14.0"}, "minimum macOS")):
            with self.subTest(changes=changes), Bundle() as bundle:
                bundle.write_info(**changes)
                with self.assertRaisesRegex(ValueError, reason):
                    bundle.verify()

    # A required file, or an extra entry, replaced by a link. The target
    # does not exist, so a verifier that followed it would fail differently.
    def test_linked_resources_are_rejected_without_following_the_target(self):
        for name in ("mozc-oss.muudict", "postal-code-seed.tsv", "postal-code-seed-manifest.json", "extra-link"):
            with self.subTest(name=name), Bundle() as bundle:
                path = bundle.resources / name
                path.unlink(missing_ok=True)
                path.symlink_to(bundle.root / "unopened-missing-target")
                with self.assertRaisesRegex(ValueError, "bundle contains a symlink"):
                    bundle.verify()

    # A fetched model file or Models directory copied into Resources.
    def test_model_weights_are_not_part_of_the_bundle(self):
        for name in ("model.safetensors", "model.gguf", "Models"):
            with self.subTest(name=name), Bundle() as bundle:
                (bundle.resources / name).write_bytes(b"not a bundled model")
                with self.assertRaisesRegex(ValueError, "bundle contents differ"):
                    bundle.verify()


if __name__ == "__main__":
    unittest.main()
