#!/usr/bin/env python3
"""Publish a checked standard ZIP, its one-line checksum and its source installer."""

import argparse
import hashlib
from pathlib import Path
import plistlib
import re
import stat
import subprocess
import tempfile


class PackageError(Exception):
    pass


def run(arguments, repository):
    return subprocess.run(arguments, cwd=repository, check=True, capture_output=True, text=True).stdout


# The app records HEAD as its source commit and the commit count as its build,
# so packaging needs a clean tree with full history.
def clean_head(repository):
    if run(['git', 'status', '--porcelain', '--untracked-files=all'], repository):
        raise PackageError('release packaging requires a clean working tree')
    if run(['git', 'rev-parse', '--is-shallow-repository'], repository).strip() != 'false':
        raise PackageError('release packaging requires full history')
    return run(['git', 'rev-parse', 'HEAD'], repository).strip()


def snapshot(app):
    """Compare every entry and byte, preserving executable bits without xattrs."""
    attributes = subprocess.run(['xattr', '-r', str(app)], check=True,
                                capture_output=True, text=True).stdout
    if 'com.apple.quarantine' in attributes:
        raise PackageError('app contains quarantine metadata')
    result = {}
    for path in [app, *sorted(app.rglob('*'))]:
        metadata = path.lstat()
        if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
            raise PackageError('app contains a link or special file')
        name = str(path.relative_to(app))
        if stat.S_ISREG(metadata.st_mode):
            with path.open('rb') as source:
                digest = hashlib.file_digest(source, 'sha256').hexdigest()
            result[name] = ('file', digest, metadata.st_mode & 0o111)
        else:
            result[name] = ('directory',)
    return result


def package(repository, just):
    head = clean_head(repository)
    output = repository / 'build/release'
    output.mkdir(parents=True, exist_ok=True)
    # One packager at a time; mkdir fails if another holds the lock.
    lock = output / '.package-lock'
    lock.mkdir(mode=0o700)
    try:
        run([just, 'release-bundle-check'], repository)
        app = repository / 'build/xcode/Build/Products/Release/MuuIME.app'
        info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
        if info.get('MuuBuildVariant') != 'standard':
            raise PackageError('only the standard app can be packaged')
        version = info.get('CFBundleShortVersionString')
        if not isinstance(version, str) or not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
            raise PackageError('invalid release version')
        tag = 'v' + version
        if run(['git', 'tag', '--list', tag], repository).strip():
            if run(['git', 'rev-parse', 'refs/tags/' + tag + '^{commit}'], repository).strip() != head:
                raise PackageError('release tag does not name the packaged source commit')
        # Read bytes from the commit, not text with newline conversion. A tag
        # created after packaging must point at the printed source_commit.
        installer_bytes = subprocess.run(['git', 'show', head + ':install.sh'],
            cwd=repository, check=True, capture_output=True).stdout
        source_installer = repository / 'install.sh'
        if not stat.S_ISREG(source_installer.lstat().st_mode) or source_installer.read_bytes() != installer_bytes:
            raise PackageError('installer differs from the packaged source commit')
        # A published version is never rebuilt in place: its checksum may
        # already be in use, so a changed ZIP needs a new version number.
        destination = output / ('v' + version)
        if destination.exists() or destination.is_symlink():
            raise PackageError('release version already exists; choose a new version')
        original = snapshot(app)
        with tempfile.TemporaryDirectory(prefix='.package-', dir=output) as temporary:
            work = Path(temporary)
            assets = work / 'assets'
            assets.mkdir(mode=0o700)
            installer = assets / 'install.sh'
            installer.write_bytes(installer_bytes)
            archive = assets / ('MuuIME-' + version + '-macos-arm64.zip')
            run(['ditto', '-c', '-k', '--keepParent', '--norsrc', '--noqtn', str(app), str(archive)], repository)
            extracted = work / 'extracted'
            extracted.mkdir()
            run(['ditto', '-x', '-k', '--norsrc', '--noqtn', str(archive), str(extracted)], repository)
            if sorted(path.name for path in extracted.iterdir()) != ['MuuIME.app']:
                raise PackageError('ZIP has unexpected top-level entries')
            # Check what users will extract, not only what was built: the
            # unpacked app must equal the built one and pass the same gate.
            roundtrip = extracted / 'MuuIME.app'
            if snapshot(roundtrip) != original or snapshot(app) != original:
                raise PackageError('ZIP roundtrip changed app entries, bytes or executable bits')
            settings = work / 'build-settings.json'
            settings.write_text(run([just, '_xcode-release', 'settings'], repository))
            run([just, '_release-app-check', str(roundtrip), str(settings), head], repository)
            with archive.open('rb') as source:
                digest = hashlib.file_digest(source, 'sha256').hexdigest()
            # Exactly one line, two spaces and a final LF: the form install.sh
            # accepts and from which it takes the version.
            (assets / 'SHA256SUMS').write_text(digest + '  ' + archive.name + '\n', encoding='ascii')
            if installer.read_bytes() != installer_bytes:
                raise PackageError('packaged installer differs from the source commit')
            if clean_head(repository) != head:
                raise PackageError('source commit changed during packaging')
            if destination.exists() or destination.is_symlink():
                raise PackageError('release version appeared during packaging')
            # Publish all three assets together, never a partial release set.
            assets.rename(destination)
        print('version: ' + version)
        print('build: ' + str(info.get('CFBundleVersion', '')))
        print('source_commit: ' + head)
        print('assets: ' + str(destination))
    finally:
        lock.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repository', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--just', default='just')
    args = parser.parse_args()
    try:
        package(args.repository.resolve(), args.just)
    except (PackageError, OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, 'release package failed: ' + str(error) + '\n')


if __name__ == '__main__':
    main()
