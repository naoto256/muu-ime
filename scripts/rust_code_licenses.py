#!/usr/bin/env python3
"""Copy reviewed Rust license originals; checks never approve new obligations."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile


# The reviewed manifest (THIRD_PARTY_LICENSES/code_deps/manifest.json) is the
# authority. Which license of an OR expression is used, which extra notice
# files and source copyright lines must be kept: these are review decisions
# that cannot be inferred safely, so this script never makes them. It compares
# the actual Cargo runtime graph and registry archives with the manifest, and
# any new package, version, checksum, license expression, feature, original or
# copyright line fails and goes back to review instead of being regenerated.
# The same manifest lets the bundle check run with no Cargo at all.
#
# `--check` only reads and reports differences; `--write` is for an explicit
# update after review; `--check-bundle APP` compares a built app with the
# manifest alone.
CODE = Path("THIRD_PARTY_LICENSES/code_deps")
CONTENTS = "Resources/THIRD_PARTY_LICENSES/code_deps"
ROOT = Path(__file__).resolve().parent.parent


class LicenseError(ValueError):
    pass


def digest(data):
    return hashlib.sha256(data).hexdigest()


def relative(value):
    if not isinstance(value, str) or not value or "\\" in value or "\x00" in value:
        raise LicenseError("invalid relative license path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(p in ("", ".", "..") for p in value.split("/")):
        raise LicenseError(f"unsafe relative license path: {value!r}")
    return path


def checked_hash(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value):
        raise LicenseError("invalid license SHA-256")
    return value


# Deterministic output: sorted keys, fixed indentation, one trailing newline,
# and nothing machine-specific (no absolute path, time or environment). The
# check compares bytes, so the same inputs must always give the same tree,
# and a reviewed update shows only the real change in its diff.
def canonical(data):
    return (json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()


def identity(package):
    return package["name"], package["version"]


def manifest_at(repository):
    path = repository / CODE / "manifest.json"
    regular_tree(path.parent)
    try:
        manifest = json.loads(path.read_bytes())
        if manifest["schema_version"] != 1 or (
            manifest["root"], manifest["target"], manifest["profile"], manifest["default_features"]
        ) != ("muu-ffi", "aarch64-apple-darwin", "release", True):
            raise LicenseError("unsupported runtime license scope")
        if not manifest["crates"] or not manifest["workspace_crates"]:
            raise LicenseError("empty runtime license inventory")
        for packages in (manifest["crates"], manifest["workspace_crates"]):
            keys = [identity(p) for p in packages]
            if len(keys) != len(set(keys)):
                raise LicenseError("duplicate runtime license package")
        for policy in manifest["crates"]:
            paths = [str(relative(f["path"])) for f in policy["files"]]
            if len(paths) != len(set(paths)) or policy["license_file"] not in paths:
                raise LicenseError("missing or duplicate selected license original")
            choice(policy["license"], policy["selected_license"])
        return manifest
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise LicenseError(f"invalid runtime license manifest: {error}") from error


# The runtime set is what `cargo tree` reaches from muu-ffi for the release
# target over normal, non-proc-macro edges: exactly what is linked into the
# staticlib. Build, dev and host-only tools are outside it.
def select_runtime(metadata, tree, checksums=None):
    """Metadata supplies identities; the selected tree supplies target features.

    Workspace-wide metadata can unify dev features. Neither proc-macro host
    dependencies nor another workspace member may enlarge this runtime set.
    """
    candidates = {}
    for package in metadata["packages"]:
        candidates.setdefault(identity(package), []).append(package)
    selected = {}
    for line in tree.splitlines():
        try:
            display, features = line.split("\t")
            match = re.fullmatch(r"([A-Za-z0-9_-]+) v([^\s]+)(?: \(.+\))?", display)
            if not match:
                raise ValueError("unrecognized package display")
            key = match.group(1, 2)
            matches = candidates[key]
            if len(matches) != 1:
                raise ValueError("ambiguous package source")
            selected.setdefault(key, {"package": matches[0], "features": set()})["features"].update(
                filter(None, features.split(","))
            )
        except (ValueError, KeyError) as error:
            raise LicenseError(f"cannot identify selected Cargo package: {line!r}") from error
    if not any(name == "muu-ffi" for name, _ in selected):
        raise LicenseError("selected Cargo tree has no muu-ffi root")
    workspace, crates = [], []
    for entry in selected.values():
        package = entry["package"]
        if any("proc-macro" in target["kind"] for target in package["targets"]):
            raise LicenseError("host proc-macro entered the runtime tree")
        row = {k: package[k] for k in ("name", "version", "license")}
        if package["id"] in metadata["workspace_members"]:
            workspace.append(row)
            continue
        if package["source"] != "registry+https://github.com/rust-lang/crates.io-index":
            raise LicenseError(f"unreviewed runtime source: {package['name']}")
        source = Path(package["manifest_path"]).parent
        archive = source.parents[2] / "cache" / source.parent.name / f"{package['name']}-{package['version']}.crate"
        # Cargo --locked validates Cargo.lock against the registry index.
        # Compare the actual archive with the reviewed checksum separately;
        # unpacked sources need not have .cargo-checksum.json on this Cargo.
        checksum = (checksums[(*identity(package), package["source"])] if checksums is not None
                    else digest(archive.read_bytes()))
        row.update(source=package["source"], features=sorted(entry["features"]), directory=source,
                   archive=archive,
                   checksum=checksum)
        crates.append(row)
    return {"workspace_crates": sorted(workspace, key=identity), "crates": sorted(crates, key=identity)}


def runtime(repository):
    command = [sys.executable, str(repository / "scripts/rust_toolchain.py"), "cargo"]
    environment = os.environ | {"CARGO_NET_OFFLINE": "true"}
    metadata = json.loads(subprocess.check_output(command + [
        "metadata", "--format-version", "1", "--locked", "--offline", "--filter-platform", "aarch64-apple-darwin"
    ], cwd=repository, env=environment))
    tree = subprocess.check_output(command + [
        "tree", "--locked", "--offline", "-p", "muu-ffi", "--target", "aarch64-apple-darwin",
        "--edges", "normal,no-proc-macro", "--prefix", "none", "--no-dedupe", "--format", "{p}\t{f}"
    ], cwd=repository, env=environment, text=True)
    return select_runtime(metadata, tree)


def choice(expression, selected):
    # Only the reviewed simple OR forms are supported. A new AND, WITH or
    # unfamiliar expression needs review, not a home-grown SPDX inference.
    aliases = {"MIT/Apache-2.0": "MIT OR Apache-2.0", "Apache-2.0/MIT": "Apache-2.0 OR MIT"}
    options = aliases.get(expression, expression).split(" OR ")
    known = {"MIT", "Apache-2.0", "Zlib", "0BSD", "Unlicense", "BSL-1.0", "BSD-2-Clause"}
    if selected not in ("MIT", "Apache-2.0") or selected not in options or not set(options) <= known:
        raise LicenseError(f"unreviewed license choice: {expression!r} -> {selected!r}")


def copyright_candidates(files):
    """Find leading Rust copyright lines, not licenses or legal conclusions.

    This deliberately small guard makes an omitted reviewed header fail.
    Conditional native code and other license exceptions remain a review task.
    """
    result = set()
    for path, data in files.items():
        if not path.startswith("src/") or not path.endswith(".rs"):
            continue
        if re.search(r"(^|/)(?:test|tests|test.rs|tests.rs)(?:/|$)", path):
            continue
        for line in data.decode().splitlines()[:40]:
            if re.match(r"^\s*//[/!]?\s*copyright\b", line, re.I):
                result.add(line)
    return result


# License originals are copied byte for byte from the registry archive whose
# SHA-256 the manifest pins, including CRLF line endings (generic-array) and
# trailing blank lines (zerocopy). Normalizing them would change the upstream
# text and break the hash that proves the bundled file is that original.
def originals(package, policy):
    archive = package["archive"].read_bytes()
    if digest(archive) != checked_hash(policy["checksum"]):
        raise LicenseError(f"crate archive checksum differs: {policy['name']}")
    prefix = f"{policy['name']}-{policy['version']}/"
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        files = {}
        for member in tar.getmembers():
            if not member.name.startswith(prefix):
                raise LicenseError("crate member outside package")
            path = member.name[len(prefix):]
            if not member.isfile():
                if member.isdir():
                    continue
                raise LicenseError("non-regular crate member")
            relative(path)
            files[path] = tar.extractfile(member).read()
    copied = {}
    for item in policy["files"]:
        path = str(relative(item["path"]))
        data = files[path]
        if digest(data) != checked_hash(item["sha256"]):
            raise LicenseError(f"original license changed: {policy['name']}/{path}")
        if (package["directory"] / path).read_bytes() != data:
            raise LicenseError(f"unpacked license differs from archive: {path}")
        if path in copied:
            raise LicenseError("duplicate original license path")
        copied[path] = data
    if not copied:
        raise LicenseError("package has no license originals")
    if policy["license_file"] not in copied:
        raise LicenseError("missing selected license original")
    # These carriers are distinct from alternative root license options.
    extra = {p for p in files if re.fullmatch(r"(?:NOTICE|COPYRIGHT|AUTHORS)(?:\..+)?", PurePosixPath(p).name, re.I)
             or ("/" in p and "license" in PurePosixPath(p).name.lower()) or p == "LICENSE.md"}
    if not extra <= copied.keys():
        raise LicenseError(f"missing additional license originals: {policy['name']}: {sorted(extra - copied.keys())}")
    statements = []
    for item in policy["copyrights"]:
        path = str(relative(item["path"]))
        data = files[path]
        if digest(data) != checked_hash(item["sha256"]) or (package["directory"] / path).read_bytes() != data:
            raise LicenseError(f"copyright source changed: {path}")
        line = item["line"]
        if type(line) is not int or line < 1:
            raise LicenseError("invalid copyright line")
        text = data.decode().splitlines()[line - 1]
        if not re.match(r"^\s*//[/!]?\s*copyright\b", text, re.I):
            raise LicenseError("copyright excerpt no longer identifies a holder")
        statements.append((path, line, text))
    if {s[2] for s in statements} != copyright_candidates(files):
        raise LicenseError(f"unreviewed or omitted source copyright: {policy['name']}")
    return copied, statements


def copyright_text(sections):
    lines = ["Rust source copyright notices", "============================", "",
             "Original source notices. The selected license and its full text are listed in manifest.json.", ""]
    for policy, statements in sections:
        if not statements:
            continue
        lines += [f"{policy['name']} {policy['version']} ({policy['selected_license']})", ""]
        for path, line, text in sorted(statements):
            lines += [f"{path}:{line}", text, ""]
    return ("\n".join(lines).rstrip() + "\n").encode()


def plan(manifest, graph):
    if manifest["workspace_crates"] != graph["workspace_crates"]:
        raise LicenseError("workspace runtime license inventory differs")
    actual = {identity(p): p for p in graph["crates"]}
    if {identity(p) for p in manifest["crates"]} != actual.keys():
        raise LicenseError("Cargo runtime license package set differs")
    result, sections = {}, []
    for policy in sorted(manifest["crates"], key=identity):
        package = actual[identity(policy)]
        for field in ("source", "checksum", "license", "features"):
            if policy[field] != package[field]:
                raise LicenseError(f"runtime {field} differs: {policy['name']}")
        choice(policy["license"], policy["selected_license"])
        prefix = str(relative(f"{policy['name']}-{policy['version']}"))
        copied, statements = originals(package, policy)
        result.update({f"{prefix}/{path}": data for path, data in copied.items()})
        sections.append((policy, statements))
    notices = copyright_text(sections)
    normalized = manifest | {"source_notices_sha256": digest(notices)}
    result.update({"manifest.json": canonical(normalized), "SOURCE-NOTICES.txt": notices})
    return result


def regular_tree(directory):
    files, directories = set(), set()
    if directory.is_symlink():
        raise LicenseError(f"license tree contains a symlink: {directory}")
    if not directory.exists():
        return files, directories
    for base, children, names in os.walk(directory, followlinks=False):
        for name in children + names:
            path = Path(base) / name
            relative_path = path.relative_to(directory).as_posix()
            if path.is_symlink():
                raise LicenseError(f"license tree contains a symlink: {relative_path}")
            if path.is_dir():
                directories.add(relative_path)
            elif path.is_file():
                files.add(relative_path)
            else:
                raise LicenseError(f"license is not a regular file: {relative_path}")
    return files, directories


def parent_paths(files):
    return {p.as_posix() for name in files for p in PurePosixPath(name).parents if str(p) != "."}


# Without `write` this only compares, exactly: missing, extra and changed
# files and extra directories all fail. With `write` it writes the expected
# files and removes anything else, but only inside this license directory.
def synchronize(directory, expected, write=False):
    files, directories = regular_tree(directory)
    wanted_dirs = parent_paths(expected)
    changed = [name for name, data in expected.items() if name not in files or (directory / name).read_bytes() != data]
    if not write:
        if files != expected.keys() or directories != wanted_dirs or changed:
            raise LicenseError(f"generated license tree differs: missing={sorted(expected.keys() - files)}, "
                               f"extra={sorted(files - expected.keys())}, changed={sorted(changed)}")
        return
    for name, data in sorted(expected.items()):
        path = directory / relative(name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    for name in files - expected.keys():
        (directory / name).unlink()
    for name in sorted(directories - wanted_dirs, key=lambda p: (p.count("/"), p), reverse=True):
        (directory / name).rmdir()


# The release allowlist lists every license file and directory by name; the
# code-license subtree is replaced in place, and other rows are kept as they
# are. No wildcard row is ever produced.
def allowlist_bytes(data, files):
    rows = [("file", f"{CONTENTS}/{name}") for name in files]
    rows += [("directory", CONTENTS)] + [("directory", f"{CONTENTS}/{name}") for name in parent_paths(files)]
    generated = [f"{kind}\t{path}" for kind, path in sorted(rows, key=lambda row: (row[1], row[0]))]
    output, inserted = [], False
    for line in data.decode().splitlines():
        if line and not line.startswith("#"):
            kind, path = line.split("\t")
            if path == CONTENTS or path.startswith(CONTENTS + "/"):
                if not inserted:
                    output.extend(generated)
                    inserted = True
                continue
        output.append(line)
    if not inserted:
        raise LicenseError("release allowlist has no code license subtree")
    return ("\n".join(output) + "\n").encode()


def check_bundle(repository, app, manifest):
    # No Cargo, registry, network or toolchain is needed on this path. The
    # Cargo-to-manifest check runs separately before building the bundle.
    hashes = {"manifest.json": digest((repository / CODE / "manifest.json").read_bytes()),
              "SOURCE-NOTICES.txt": checked_hash(manifest["source_notices_sha256"])}
    for policy in manifest["crates"]:
        prefix = str(relative(f"{policy['name']}-{policy['version']}"))
        for item in policy["files"]:
            hashes[f"{prefix}/{relative(item['path'])}"] = checked_hash(item["sha256"])
    for directory in (repository / CODE, app / "Contents" / CONTENTS):
        files, directories = regular_tree(directory)
        if files != hashes.keys() or directories != parent_paths(hashes):
            raise LicenseError("code license file/directory set differs from manifest")
        for name, expected in hashes.items():
            if digest((directory / name).read_bytes()) != expected:
                raise LicenseError(f"code license bytes differ: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--check", action="store_true")
    modes.add_argument("--write", action="store_true")
    modes.add_argument("--check-bundle", type=Path, metavar="APP")
    parser.add_argument("--repository", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        manifest = manifest_at(args.repository)
        if args.check_bundle:
            check_bundle(args.repository, args.check_bundle, manifest)
        else:
            expected = plan(manifest, runtime(args.repository))
            allowlist = args.repository / "test-support/release-bundle-contents.tsv"
            data = allowlist_bytes(allowlist.read_bytes(), expected)
            synchronize(args.repository / CODE, expected, args.write)
            if args.write:
                allowlist.write_bytes(data)
            elif allowlist.read_bytes() != data:
                raise LicenseError("release allowlist code license subtree differs")
        print("Rust code licenses verified" if not args.write else "Rust code licenses generated")
    except (LicenseError, OSError, KeyError, IndexError, UnicodeError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"Rust code license check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
