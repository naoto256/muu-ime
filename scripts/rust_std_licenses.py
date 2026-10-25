#!/usr/bin/env python3
"""Check the reviewed arm64 sysroot notices without fetching or inferring licenses."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

from rust_code_licenses import (LicenseError, canonical, checked_hash, digest,
                                parent_paths, regular_tree, relative, synchronize)

# The Rust standard library is linked into the staticlib but never appears in
# Cargo's graph, so it has its own manifest, kept apart from the 75 crates.
# Its scope is deliberately narrow: one toolchain (rust-toolchain, with the
# Rust and LLVM source commits) and one target, aarch64-apple-darwin, and
# within it only the 19 sysroot rlibs whose objects are actually in the
# release staticlib. COPYRIGHT-library.html is upstream's report for every
# platform and for build-only crates; it is kept unmodified as provenance,
# but it is not a list of what this app contains. Treating it as one would,
# for example, report another platform's MPL-2.0 vex-sdk as linked here.
#
# Originals come from two places. `toolchain` ones are copied from the pinned
# toolchain's share/doc/rust. The rest are `reviewed` originals, each with
# its URL in the manifest (and in NOTICE.txt): the license files of the ten
# registry crates from their crates.io archives (five of them, addr2line,
# adler2, memchr, miniz_oxide and object, are not listed in the report at
# all, so these files supply their text), the
# compiler-builtins, libm and stdarch files and the top-level LICENSE-MIT from
# rust-lang/rust at the pinned Rust commit, and compiler-rt's LICENSE and
# CREDITS from rust-lang/llvm-project at the pinned LLVM commit. Every file is
# bound by its SHA-256 and copied byte for byte.
ROOT = Path(__file__).resolve().parents[1]
DIRECTORY = Path('THIRD_PARTY_LICENSES/rust')
CONTENTS = 'Resources/THIRD_PARTY_LICENSES/rust'


def file_digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def manifest_at(repository):
    regular_tree(repository / DIRECTORY)
    manifest = json.loads((repository / DIRECTORY / 'manifest.json').read_bytes())
    if (manifest['schema_version'] != 1 or manifest['target'] != 'aarch64-apple-darwin'
            or (repository / 'rust-toolchain').read_text().strip() != manifest['version']):
        raise LicenseError('standard library scope or toolchain version differs')
    relative(manifest['version'])
    for field in ('rust_commit', 'llvm_commit'):
        if len(manifest[field]) != 40 or any(c not in '0123456789abcdef' for c in manifest[field]):
            raise LicenseError('invalid pinned source commit')
    sources = manifest['sources']
    if not sources or len({x['path'] for x in sources}) != len(sources):
        raise LicenseError('empty or duplicate standard library originals')
    for item in sources:
        if not str(relative(item['path'])).startswith(manifest['version'] + '/'):
            raise LicenseError('standard library original is outside version directory')
        checked_hash(item['sha256'])
        origin = item['origin']
        if origin['kind'] == 'toolchain':
            if not str(relative(origin['path'])).startswith('share/doc/rust/'):
                raise LicenseError('invalid toolchain notice path')
        elif origin['kind'] != 'reviewed':
            raise LicenseError('unknown standard library original source')
    components = manifest['components']
    if (not components or len({x['name'] for x in components}) != len(components)
            or len({x['rlib'] for x in components}) != len(components)):
        raise LicenseError('empty or duplicate sysroot components')
    for item in components:
        if str(relative(item['rlib'])) != Path(item['rlib']).name or not item['rlib'].endswith('.rlib'):
            raise LicenseError('invalid sysroot archive name')
        checked_hash(item['sha256'])
        if not item['notice_files'] or not item['selected_license']:
            raise LicenseError('sysroot component has no reviewed license coverage')
        if not set(item['notice_files']) <= {x['path'] for x in sources}:
            raise LicenseError('sysroot component notice is missing from originals')
    return manifest


def notice(manifest):
    lines = ['Rust standard library notices', '=============================', '',
             'Toolchain: ' + manifest['version'], 'Target: ' + manifest['target'],
             'Rust source commit: ' + manifest['rust_commit'], '', 'Scope:']
    lines.extend('- ' + item for item in manifest['scope_notes'])
    lines += ['', 'Sysroot components included in the release staticlib:']
    lines.extend('- ' + x['name'] + ' ' + x['version'] + ': ' + x['selected_license'] for x in manifest['components'])
    lines += ['', 'In-tree copyright notices from COPYRIGHT-library.html:',
              '(The upstream report remains unmodified alongside these notices.)']
    for item in manifest['in_tree']:
        lines += ['', item['path'] + ': ' + item['license'], 'Selected: ' + item['selected_license']]
        lines.extend('Copyright: ' + text for text in item['copyright'])
    lines += ['', 'Additional original source copyright headers:']
    for item in manifest['source_copyrights']:
        lines += [item['crate'] + ', ' + item['path'] + ':' + str(item['line']), item['text']]
    lines += ['', 'Original license files and sources (SHA-256 values are in manifest.json):']
    for item in manifest['sources']:
        origin = item['origin']
        lines += ['', item['path'], origin.get('url', 'Rust toolchain: ' + origin.get('path', ''))]
    return ('\n'.join(lines).rstrip() + '\n').encode()


def plan(repository, manifest, sysroot=None):
    expected = {}
    for item in manifest['sources']:
        path = str(relative(item['path']))
        source = repository / DIRECTORY / path
        if sysroot is not None and item['origin']['kind'] == 'toolchain':
            source = sysroot / relative(item['origin']['path'])
        data = source.read_bytes()
        if digest(data) != item['sha256']:
            raise LicenseError('standard library original bytes differ: ' + path)
        expected[path] = data
    output = notice(manifest)
    result = dict(manifest, notice_sha256=digest(output))
    expected[manifest['version'] + '/NOTICE.txt'] = output
    expected['manifest.json'] = canonical(result)
    return expected


def allowlist_bytes(data, expected):
    rows = [('file', CONTENTS + '/' + name) for name in expected]
    rows += [('directory', CONTENTS)] + [('directory', CONTENTS + '/' + name) for name in parent_paths(expected)]
    output = [line for line in data.decode().splitlines() if not (
        line and not line.startswith('#') and
        (line.split('\t')[1] == CONTENTS or line.split('\t')[1].startswith(CONTENTS + '/')))]
    output.extend(kind + '\t' + name for kind, name in sorted(rows, key=lambda r: (r[1], r[0])))
    return ('\n'.join(output) + '\n').encode()


# Binds the review to the toolchain actually in use: release, commit and host
# must match, and each listed rlib must have its reviewed hash.
def toolchain(repository, manifest):
    wrapper = [sys.executable, str(repository / 'scripts/rust_toolchain.py'), 'rustc']
    version = subprocess.check_output(wrapper + ['-vV'], cwd=repository, text=True)
    facts = dict(line.split(': ', 1) for line in version.splitlines() if ': ' in line)
    if (facts.get('release') != manifest['version'] or facts.get('commit-hash') != manifest['rust_commit']
            or facts.get('host') != manifest['target']):
        raise LicenseError('unreviewed Rust standard library toolchain')
    sysroot = Path(subprocess.check_output(wrapper + ['--print', 'sysroot'], cwd=repository, text=True).strip())
    library = sysroot / 'lib/rustlib' / manifest['target'] / 'lib'
    for item in manifest['components']:
        if file_digest(library / item['rlib']) != item['sha256']:
            raise LicenseError('sysroot component changed: ' + item['name'])
    return sysroot


def archive_components(library, archive):
    """Match actual object member names, including compiler_builtins' C/assembly."""
    # A sysroot rlib counts as included when any of its object members is in
    # the staticlib. That is what makes compiler_builtins included: all 144
    # non-Rust objects in the staticlib (compiler-rt C and assembly, such as
    # the aarch64 LSE variants) come from it. Its coverage must therefore
    # carry the compiler-builtins and libm licenses and compiler-rt's LICENSE
    # and CREDITS, and its license is recorded as the full AND expression,
    # never as MIT alone.
    def members(path):
        return {name for name in subprocess.check_output(['xcrun', 'ar', '-t', str(path)], text=True).splitlines()
                if name.endswith('.o')}
    linked = members(archive)
    if not linked:
        raise LicenseError('staticlib has no object members')
    return {path.name for path in library.glob('*.rlib') if members(path) & linked}


# Bundle side: manifest and files only, with no toolchain or external
# command, so a built app can be checked anywhere.
def check_bundle(repository, app, manifest):
    expected = plan(repository, manifest)
    if digest(expected[manifest['version'] + '/NOTICE.txt']) != manifest['notice_sha256']:
        raise LicenseError('standard library generated notice hash differs')
    synchronize(repository / DIRECTORY, expected)
    synchronize(app / 'Contents' / CONTENTS, expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument('--check', action='store_true')
    modes.add_argument('--write', action='store_true')
    modes.add_argument('--check-bundle', type=Path, metavar='APP')
    parser.add_argument('--archive', type=Path, help='also check actual release staticlib sysroot membership')
    parser.add_argument('--repository', type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        manifest = manifest_at(args.repository)
        if args.check_bundle:
            if args.archive:
                raise LicenseError('archive inspection belongs to the source gate')
            check_bundle(args.repository, args.check_bundle, manifest)
        else:
            sysroot = toolchain(args.repository, manifest)
            if args.archive:
                actual = archive_components(sysroot / 'lib/rustlib' / manifest['target'] / 'lib', args.archive)
                if actual != {x['rlib'] for x in manifest['components']}:
                    raise LicenseError('release staticlib sysroot component set differs')
            expected = plan(args.repository, manifest, sysroot)
            allowlist = args.repository / 'test-support/release-bundle-contents.tsv'
            data = allowlist_bytes(allowlist.read_bytes(), expected)
            synchronize(args.repository / DIRECTORY, expected, args.write)
            if args.write:
                allowlist.write_bytes(data)
            elif data != allowlist.read_bytes():
                raise LicenseError('release allowlist standard library subtree differs')
        print('Rust standard library licenses verified')
    except (LicenseError, OSError, KeyError, ValueError, subprocess.CalledProcessError) as error:
        print('Rust standard library license check failed: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
