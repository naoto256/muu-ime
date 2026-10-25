#!/usr/bin/env python3
"""Verify the linked arm64 Rust code against the app metadata and requested HEAD.

This identifies the source commit, not uncommitted working-tree edits. Build
and install only from a clean committed tree when provenance matters (docs/src/internals/build-and-test.md「ビルド」).
"""

import argparse
import json
import plistlib
import re
import subprocess
from pathlib import Path


def verify(app: Path, expected: str) -> dict:
    if not re.fullmatch(r"[0-9a-f]{40}", expected):
        raise ValueError("expected source commit must be a full lowercase hash")
    with (app / "Contents/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    if info.get("MuuSourceCommit") != expected:
        raise ValueError("app MuuSourceCommit does not match the requested commit")
    executable = app / "Contents/MacOS" / info["CFBundleExecutable"]
    arches = subprocess.check_output(["lipo", "-archs", str(executable)], text=True).split()
    if arches != ["arm64"]:
        raise ValueError("the product executable must be arm64 only")
    # muu-ffi's build.rs embeds this NUL-terminated marker; every copy linked
    # into the executable must name the same commit.
    values = set(re.findall(rb"MUU_RUST_SOURCE_COMMIT=([^\x00]*)\x00", executable.read_bytes()))
    if values != {expected.encode("ascii")}:
        raise ValueError("Rust source identifier missing, unknown or mismatched (arm64)")
    return {"source_commit": expected, "architectures": {"arm64": expected}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("expected_commit")
    arguments = parser.parse_args()
    try:
        print(json.dumps(verify(arguments.app, arguments.expected_commit), sort_keys=True))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Rust source verification failed: {error}\n")
