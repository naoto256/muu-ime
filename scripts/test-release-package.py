#!/usr/bin/env python3
"""Exercise release publication using a synthetic app, local ZIPs and fake gates."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest

REPOSITORY = Path(__file__).resolve().parents[1]
DRIVER = REPOSITORY / 'scripts/package-release.py'


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='muu-package-test-')
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / 'repository'
        self.repo.mkdir()
        for args in (['init', '-q'], ['config', 'user.name', 'Fixture'],
                     ['config', 'user.email', 'fixture@example.com'],
                     ['config', 'core.autocrlf', 'false'],
                     ['config', 'core.hooksPath', str(self.root / 'fixture-hooks')]):
            subprocess.run(['git', '-C', str(self.repo), *args], check=True)
        (self.repo / '.gitignore').write_text('/build/\n')
        self.installer = b'#!/bin/sh\n# synthetic installer\r\nprintf "synthetic only\\n"\n'
        (self.repo/'install.sh').write_bytes(self.installer)
        subprocess.run(['git', '-C', str(self.repo), 'add', '.gitignore', 'install.sh'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'commit', '-qm', 'Fixture'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'tag', 'v0.1.0'], check=True)
        self.app = self.repo / 'build/xcode/Build/Products/Release/MuuIME.app'
        (self.app / 'Contents/MacOS').mkdir(parents=True)
        (self.app / 'Contents/Resources').mkdir()
        (self.app / 'Contents/Info.plist').write_bytes(plistlib.dumps({
            'CFBundleShortVersionString': '0.1.0', 'CFBundleVersion': '1',
            'MuuBuildVariant': 'standard',
        }))
        executable = self.app / 'Contents/MacOS/MuuIME'
        executable.write_bytes(b'synthetic signed executable')
        executable.chmod(0o755)
        (self.app / 'Contents/Resources/data.txt').write_bytes(b'synthetic data')
        self.events = self.root / 'events.jsonl'
        self.gate = self.root / 'just'
        self.gate.write_text('#!' + sys.executable + '\n' + '''
import json, os, pathlib, subprocess, sys
with open(os.environ['FIXTURE_EVENTS'], 'a') as out:
    out.write(json.dumps(sys.argv[1:]) + '\\n')
mode = os.environ.get('FIXTURE_FAULT', '')
if sys.argv[1] == 'release-bundle-check':
    if mode == 'gate': sys.exit(17)
    if mode == 'head':
        subprocess.run(['git', 'commit', '--allow-empty', '-qm', 'Changed'], check=True)
elif sys.argv[1] == '_xcode-release':
    print('[]')
elif sys.argv[1] == '_release-app-check':
    app = pathlib.Path(sys.argv[2])
    if not (app / 'Contents/MacOS/MuuIME').is_file(): sys.exit(19)
    if mode == 'roundtrip': sys.exit(18)
    if mode == 'installer_asset':
        for installer in pathlib.Path('build/release').glob('.package-*/assets/install.sh'):
            installer.write_bytes(b'changed installer')
else:
    sys.exit(20)
''')
        self.gate.chmod(0o755)
        self.env = dict(os.environ, FIXTURE_EVENTS=str(self.events))

    def tearDown(self):
        self.temp.cleanup()

    def run_package(self, fault='', success=False):
        result = subprocess.run([sys.executable, str(DRIVER), '--repository', str(self.repo),
                                 '--just', str(self.gate)], env=dict(self.env, FIXTURE_FAULT=fault),
                                capture_output=True, text=True, timeout=30)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse((self.repo / 'build/release/v0.1.0').exists())
        return result

    # All assets belong to one source commit; the checksum keeps its ZIP-only
    # line so existing installers can read it. Installer bytes retain CRLF too.
    def test_complete_assets_and_roundtrip_gate(self):
        self.run_package(success=True)
        published = self.repo / 'build/release/v0.1.0'
        self.assertEqual(sorted(p.name for p in published.iterdir()),
                         ['MuuIME-0.1.0-macos-arm64.zip', 'SHA256SUMS', 'install.sh'])
        source=subprocess.check_output(['git','-C',str(self.repo),'show','v0.1.0:install.sh'])
        self.assertEqual((published/'install.sh').read_bytes(),source)
        self.assertEqual(source,self.installer)
        archive = published / 'MuuIME-0.1.0-macos-arm64.zip'
        self.assertEqual((published / 'SHA256SUMS').read_text(),
                         hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name + '\n')
        events = [json.loads(line) for line in self.events.read_text().splitlines()]
        self.assertEqual([event[0] for event in events],
                         ['release-bundle-check', '_xcode-release', '_release-app-check'])
        self.assertFalse((self.repo / 'build/release/.package-lock').exists())
        again = subprocess.run([sys.executable, str(DRIVER), '--repository', str(self.repo),
                                '--just', str(self.gate)], env=self.env, capture_output=True)
        self.assertNotEqual(again.returncode, 0)
        self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(),
                         (published / 'SHA256SUMS').read_text().split()[0])

    # No version directory appears if either gate fails or HEAD moves during
    # packaging, so the published assets always match one commit.
    # Each failure gets setUp's fresh repository: a prior HEAD change must
    # not make a later asset corruption fail an unrelated tag check first.
    def test_release_gate_failure_prevents_publication(self):
        self.run_package('gate')

    def test_roundtrip_gate_failure_prevents_publication(self):
        self.run_package('roundtrip')

    def test_head_changed_during_packaging_prevents_publication(self):
        self.run_package('head')

    def test_modified_installer_asset_prevents_publication(self):
        result=self.run_package('installer_asset')
        self.assertIn('packaged installer differs from the source commit',result.stderr)

    def test_existing_release_tag_must_name_the_packaged_source(self):
        subprocess.run(['git','-C',str(self.repo),'commit','--allow-empty','-qm','Another source'],check=True)
        result=self.run_package()
        self.assertIn('tag',result.stderr)

    # An untracked file stops packaging before any gate runs: the source commit
    # recorded in the app could not describe it.
    def test_dirty_tree_is_rejected_before_build(self):
        (self.repo / 'untracked').write_text('pending')
        self.run_package()
        self.assertFalse(self.events.exists())

    # Only the standard variant is publishable, and the version must be plain
    # x.y.z so it cannot escape build/release or confuse install.sh.
    def test_dogfood_and_invalid_version_are_rejected(self):
        for values in ({'MuuBuildVariant': 'dogfood'}, {'CFBundleShortVersionString': '../1'}):
            with self.subTest(values=values):
                path = self.app / 'Contents/Info.plist'
                info = {'CFBundleShortVersionString': '0.1.0', 'CFBundleVersion': '1',
                        'MuuBuildVariant': 'standard'}
                info.update(values)
                path.write_bytes(plistlib.dumps(info))
                self.run_package()

    # The snapshot refuses links and quarantine metadata in the app; either
    # would make the extracted copy differ from what was checked.
    def test_symlink_and_quarantine_are_rejected(self):
        extra = self.app / 'Contents/Resources/link'
        extra.symlink_to('data.txt')
        self.run_package()
        extra.unlink()
        subprocess.run(['/usr/bin/xattr', '-w', 'com.apple.quarantine',
                        '0081;0;fixture;', str(self.app)], check=True)
        self.run_package()

    # The roundtrip comparison catches an extraction that differs from the built
    # app even when the ZIP itself was created successfully.
    def test_zip_copy_corruption_is_rejected(self):
        # A fake ditto alters extraction after a valid local ZIP was created.
        directory = self.root / 'bin'
        directory.mkdir()
        ditto = directory / 'ditto'
        ditto.write_text('#!' + sys.executable + '\n' + '''
import pathlib, subprocess, sys
subprocess.run(['/usr/bin/ditto', *sys.argv[1:]], check=True)
if '-x' in sys.argv:
    app = pathlib.Path(sys.argv[-1]) / 'MuuIME.app'
    (app / 'Contents/Resources/extra.txt').write_text('unexpected')
''')
        ditto.chmod(0o755)
        self.env['PATH'] = str(directory) + ':' + os.environ['PATH']
        self.run_package()


if __name__ == '__main__':
    unittest.main()
