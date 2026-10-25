#!/usr/bin/env python3
"""Check release data against repository authorities, without opening an IME."""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import plistlib
import re
import shlex
import stat
import sys


# A required file replaced by a link or a directory is rejected without being
# followed, so the check never reads data from outside the bundle.
def regular(path):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"required regular file missing or linked: {path.name}")
    return path


# Bounds what a substituted oversized file can make the verifier read.
def read_small(path, maximum):
    regular(path)
    if path.stat().st_size > maximum:
        raise ValueError(f"resource exceeds its inspection limit: {path.name}")
    return path.read_bytes()


# Streams large resources: SHA-256 and the newline count, in one pass.
def fingerprint(path):
    regular(path)
    digest = hashlib.sha256()
    newlines = 0
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
            newlines += block.count(b"\n")
    return digest.hexdigest(), newlines


# Rejects a Debug build, an x86_64 or universal build, or an older deployment
# target, and Swift, C or Objective-C code compiled with test or diagnostic
# conditions. The input comes from `just _xcode-release settings`, the same
# invocation and overrides that build the app, so the two cannot drift. It is
# still a query of those settings, not a record taken from the built binary.
# The caller names the variant; it is never inferred from the build. Standard
# refuses MUU_DOGFOOD in every condition setting checked below, Swift, C and
# ObjC alike, so no part of a distribution build is compiled as dogfood.
# Dogfood must carry it as a Swift condition, where the recorder's capability
# is compiled, and still refuses the test and diagnostic flags.
def verify_build_settings(path, expected_variant="standard"):
    if expected_variant not in ("standard", "dogfood"):
        raise ValueError("unknown build variant")
    records = json.loads(read_small(path, 4 * 1024 * 1024))
    if not isinstance(records, list):
        raise ValueError("build settings must be an array")
    targets = [row for row in records if isinstance(row, dict) and row.get("target") == "MuuIME"]
    if len(targets) != 1 or not isinstance(targets[0].get("buildSettings"), dict):
        raise ValueError("exactly one MuuIME build settings record is required")
    settings = targets[0]["buildSettings"]
    if (settings.get("CONFIGURATION") != "Release" or settings.get("ARCHS") != "arm64"
            or settings.get("MACOSX_DEPLOYMENT_TARGET") != "15.0"):
        raise ValueError("release build settings differ")
    if settings.get("MUU_BUILD_VARIANT") != expected_variant:
        raise ValueError("build settings variant differs")
    swift_defines = set()
    # Check effective compiler settings, not only symbol naming conventions:
    # a test-only overload need not have ForTesting in its name.
    # GCC_PREPROCESSOR_DEFINITIONS controls C and ObjC too: a Swift-only check
    # would miss ClientGeometryBridge's #if defined(MUU_TEST) entry points.
    # Any value counts, MUU_TEST=0 included, because the C side tests
    # `defined`. The project has no per-file COMPILER_FLAGS or xcconfig, so
    # these target-level keys are the whole set.
    for key in ("SWIFT_ACTIVE_COMPILATION_CONDITIONS", "OTHER_SWIFT_FLAGS",
                "GCC_PREPROCESSOR_DEFINITIONS", "GCC_PREPROCESSOR_DEFINITIONS_NOT_USED_IN_PRECOMPS",
                "OTHER_CFLAGS", "OTHER_CPLUSPLUSFLAGS"):
        value = settings.get(key)
        tokens = shlex.split(value) if isinstance(value, str) else value
        if tokens is None:
            continue
        if not isinstance(tokens, list) or any(not isinstance(token, str) for token in tokens):
            raise ValueError("invalid compiler flags")
        # MUU_INPUT_PATH_DIAGNOSTICS is the retired diagnostic flag. Nothing
        # sets it any more; refusing it keeps it from coming back into a
        # Release build.
        defines = {token.removeprefix("-D").partition("=")[0] for token in tokens}
        if key in ("SWIFT_ACTIVE_COMPILATION_CONDITIONS", "OTHER_SWIFT_FLAGS"):
            swift_defines.update(defines)
        forbidden = {"MUU_TEST", "MUU_INPUT_PATH_DIAGNOSTICS"}
        if expected_variant == "standard":
            forbidden.add("MUU_DOGFOOD")
        if defines & forbidden:
            raise ValueError("release build contains test or diagnostic compilation flags")
    if expected_variant == "dogfood" and "MUU_DOGFOOD" not in swift_defines:
        raise ValueError("dogfood build lacks its Swift compilation condition")


# test-support/release-bundle-contents.tsv is the one authority for what the
# signed standard Release app may contain under Contents/. No test or recipe
# generates or rewrites it; it changes only by a reviewed edit.
#
# Format: one `kind<TAB>path` per line, kind `file` or `directory`, path
# relative to Contents/. Blank lines and lines starting with `#` are
# ignored. No wildcards, and every parent directory has its own line.
# Malformed lines are refused rather than skipped: an absolute path, `..`,
# a non-normal spelling such as `./x` or `a/`, a duplicate, an unknown kind,
# or a missing parent would otherwise give an entry that silently never
# matches.
#
# To add or remove a bundle member:
# 1. Change the Xcode project or build step that produces it.
# 2. Run `just release-bundle-check`. It fails and names each new path under
#    `extra=` and each vanished one under `missing=`.
# 3. Edit the TSV by hand: one line per new file and per new directory,
#    parents included; delete the lines of removed members. Check each path
#    belongs in the release rather than copying the list over.
# 4. A new third-party member also needs its license files under
#    THIRD_PARTY_LICENSES (checked by verify-bundled-third-party-licenses.sh),
#    and those files need lines here too.
# 5. Run `just release-resources-test` and `just release-bundle-check` again.
def allowed_contents(repository):
    entries = set()
    data = read_small(repository / "test-support/release-bundle-contents.tsv", 64 * 1024)
    for line in data.decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 2:
            raise ValueError("invalid release contents authority")
        kind, name = fields
        path = PurePosixPath(name)
        if (kind not in ("file", "directory") or not name or path.is_absolute()
                or ".." in path.parts or name != str(path) or name == "."
                or (kind, name) in entries):
            raise ValueError("invalid release contents authority")
        entries.add((kind, name))
    if not entries:
        raise ValueError("empty release contents authority")
    for _, name in entries:
        for parent in PurePosixPath(name).parents:
            if str(parent) != "." and ("directory", str(parent)) not in entries:
                raise ValueError("release contents authority is missing a parent directory")
    return entries


# Compares the (kind, path) set under Contents/ with the authority. This
# rejects an extra file (a debug resource or a fetched model), an empty or
# extra directory, a missing member, and a file swapped for a directory or
# back. A symlink or special file fails outright. The error names both
# differences, so one run shows the whole change. `_CodeSignature` is listed,
# so this runs on the signed app.
def verify_contents(contents, repository):
    expected = allowed_contents(repository)
    actual = set()
    for path in contents.rglob("*"):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            raise ValueError("release bundle contains a symlink")
        if stat.S_ISDIR(mode):
            kind = "directory"
        elif stat.S_ISREG(mode):
            kind = "file"
        else:
            raise ValueError("release bundle contains a special file")
        actual.add((kind, path.relative_to(contents).as_posix()))
    if actual != expected:
        raise ValueError(f"release bundle contents differ: missing={sorted(expected - actual)}, extra={sorted(actual - expected)}")


def verify(app, repository, build_settings, expected_variant="standard"):
    verify_build_settings(build_settings, expected_variant)
    contents = app / "Contents"
    resources = contents / "Resources"
    # A bundle whose Contents or Resources is a link to some other tree.
    for directory in (app, contents, resources):
        if not stat.S_ISDIR(directory.lstat().st_mode):
            raise ValueError("release bundle directories must not be links")
    # Check the entire tree before reading members, including MacOS, localized
    # resources and signature products. No extra file or empty directory passes.
    verify_contents(contents, repository)
    plist = plistlib.loads(read_small(contents / "Info.plist", 1024 * 1024))
    if not isinstance(plist, dict):
        raise ValueError("release Info.plist must be a dictionary")
    # Normal AppKit termination must ask the delegate to flush pending saves.
    # Keep system-managed termination shortcuts disabled in both variants.
    # Sudden termination can end the process without applicationShouldTerminate,
    # skipping the final learning save.
    for key in ("NSSupportsSuddenTermination", "NSSupportsAutomaticTermination"):
        if plist.get(key, False) is not False:
            raise ValueError("release termination must use the application delegate: " + key)
    # The wrong variant (input-path-diagnostic), a lower minimum macOS, or
    # an executable name that would not launch.
    if plist.get("MuuBuildVariant") != expected_variant:
        raise ValueError("release bundle must use the " + expected_variant + " variant")
    if plist.get("LSMinimumSystemVersion") != "15.0":
        raise ValueError("release minimum macOS must be 15.0")
    if plist.get("CFBundleExecutable") != "MuuIME":
        raise ValueError("release bundle executable must be MuuIME")
    # A copy that lost the executable bit would not launch.
    executable = regular(contents / "MacOS/MuuIME")
    if not executable.stat().st_mode & stat.S_IXUSR:
        raise ValueError("release executable must be owner-executable")

    # Another dictionary: a stale build, or one built from different sources.
    # The pin covers the whole artifact (header, payload and provenance), so
    # a byte match needs no second dictionary parser.
    pin = read_small(repository / "dict-src/mozc-oss.sha256", 4096).decode().split()
    if not pin or not re.fullmatch("[0-9a-f]{64}", pin[0]):
        raise ValueError("dictionary authority is malformed")
    actual, _ = fingerprint(resources / "mozc-oss.muudict")
    if actual != pin[0]:
        raise ValueError("dictionary artifact fingerprint differs from the pin")

    # The repository's own seed pair must agree first: a TSV updated without
    # its manifest, or the reverse, is caught before the bundle is compared.
    seed = repository / "apple/MuuIME/PostalCodeSeed"
    manifest_name = "postal-code-seed-manifest.json"
    reference_bytes = read_small(seed / manifest_name, 64 * 1024)
    manifest = json.loads(reference_bytes)
    if not isinstance(manifest, dict):
        raise ValueError("source seed manifest must be an object")
    count = manifest.get("entry_count")
    if manifest.get("schema_version") != 1 or type(count) is not int or count <= 0:
        raise ValueError("source seed count or schema is invalid")
    # This is byte packing, not a second TSV codec. Rust's seed test owns field
    # interpretation. V1's one header line is excluded from the record count.
    expected_hash, expected_lines = fingerprint(seed / "postal-code-seed.tsv")
    if expected_hash != manifest.get("snapshot_sha256") or expected_lines - 1 != count:
        raise ValueError("source seed hash or count is inconsistent")
    # A stale or swapped seed in the bundle: the manifest must match the
    # source byte for byte, and the snapshot its hash and record count.
    if read_small(resources / manifest_name, 64 * 1024) != reference_bytes:
        raise ValueError("seed manifest differs from the canonical source")
    actual_hash, actual_lines = fingerprint(resources / "postal-code-seed.tsv")
    if actual_hash != expected_hash or actual_lines - 1 != count:
        raise ValueError("seed snapshot hash or count differs from the canonical source")
    return {"dictionary_artifact_sha256": actual, "postal_entry_count": count,
            "postal_snapshot_sha256": actual_hash}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", required=True, type=Path)
    parser.add_argument("--repository", required=True, type=Path)
    parser.add_argument("--build-settings", required=True, type=Path)
    parser.add_argument("--expected-variant", choices=("standard", "dogfood"), default="standard")
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args.app, args.repository, args.build_settings, args.expected_variant), sort_keys=True))
    except (OSError, ValueError, TypeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
