#!/usr/bin/env python3
"""Synthetic checks for sysroot notice coverage; no Rust install or network required."""
import copy
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

import rust_std_licenses as licenses
from rust_code_licenses import canonical, digest


class StdLicenseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repository = self.root / 'repo'
        self.original = self.repository / licenses.DIRECTORY / '1.96.0/COPYRIGHT-library.html'
        self.original.parent.mkdir(parents=True)
        self.original.write_bytes(b'Synthetic MIT and Unicode report\n')
        (self.repository / 'rust-toolchain').write_text('1.96.0\n')
        self.sysroot = self.root / 'sysroot'
        self.report = self.sysroot / 'share/doc/rust/COPYRIGHT-library.html'
        self.report.parent.mkdir(parents=True)
        shutil.copyfile(self.original, self.report)
        self.library = self.sysroot / 'lib/rustlib/aarch64-apple-darwin/lib'
        self.library.mkdir(parents=True)
        (self.library / 'libstd-abc.rlib').write_bytes(b'synthetic archive')
        self.manifest = {
            'schema_version': 1, 'target': 'aarch64-apple-darwin', 'version': '1.96.0',
            'rust_commit': 'a' * 40, 'llvm_commit': 'b' * 40,
            'components': [{'name': 'std', 'version': '0.0.0', 'rlib': 'libstd-abc.rlib',
                            'sha256': digest(b'synthetic archive'), 'selected_license': 'MIT',
                            'notice_files': ['1.96.0/COPYRIGHT-library.html']}],
            'sources': [{'path': '1.96.0/COPYRIGHT-library.html', 'sha256': digest(self.original.read_bytes()),
                         'origin': {'kind': 'toolchain', 'path': 'share/doc/rust/COPYRIGHT-library.html'}}],
            'in_tree': [{'path': '.', 'license': 'MIT OR Apache-2.0', 'selected_license': 'MIT',
                         'copyright': ['Synthetic Contributors']}],
            'source_copyrights': [], 'scope_notes': ['Synthetic target scope'],
        }
        expected = licenses.plan(self.repository, self.manifest, self.sysroot)
        licenses.synchronize(self.repository / licenses.DIRECTORY, expected, True)
        self.manifest = licenses.manifest_at(self.repository)
        self.app = self.root / 'app'
        shutil.copytree(self.repository / licenses.DIRECTORY, self.app / 'Contents' / licenses.CONTENTS)

    # Two runs give identical bytes, the report is copied unchanged, and the
    # generated NOTICE has no machine-specific path.
    def test_originals_and_generated_notices_are_deterministic(self):
        one = licenses.plan(self.repository, self.manifest, self.sysroot)
        two = licenses.plan(self.repository, self.manifest, self.sysroot)
        self.assertEqual(one, two)
        self.assertEqual(self.report.read_bytes(), one['1.96.0/COPYRIGHT-library.html'])
        self.assertIn(b'Copyright: Synthetic Contributors', one['1.96.0/NOTICE.txt'])
        self.assertNotIn(str(self.root).encode(), b''.join(one.values()))
        licenses.synchronize(self.repository / licenses.DIRECTORY, one)

    # A changed toolchain report, or a changed checked-in original, fails on
    # its hash.
    def test_report_and_reviewed_original_mutations_fail(self):
        self.report.write_bytes(b'changed toolchain report')
        with self.assertRaisesRegex(licenses.LicenseError, 'original bytes differ'):
            licenses.plan(self.repository, self.manifest, self.sysroot)
        self.original.write_bytes(b'changed checked-in original')
        with self.assertRaisesRegex(licenses.LicenseError, 'original bytes differ'):
            licenses.plan(self.repository, self.manifest)

    # An original deleted from both the source tree and the app still fails.
    def test_original_missing_on_both_sides_fails(self):
        self.original.unlink()
        (self.app / 'Contents' / licenses.CONTENTS / '1.96.0/COPYRIGHT-library.html').unlink()
        with self.assertRaises(OSError):
            licenses.check_bundle(self.repository, self.app, self.manifest)

    # In the app, an extra file, directory or symlink, and a wrong NOTICE, all
    # fail.
    def test_extra_file_directory_symlink_and_notice_corruption_fail(self):
        target = self.app / 'Contents' / licenses.CONTENTS
        extra = target / 'extra'
        for kind in ('file', 'directory', 'symlink'):
            if kind == 'file': extra.write_bytes(b'unexpected')
            elif kind == 'directory': extra.mkdir()
            else: extra.symlink_to(self.original)
            with self.assertRaises(licenses.LicenseError):
                licenses.check_bundle(self.repository, self.app, self.manifest)
            if kind == 'directory': extra.rmdir()
            else: extra.unlink()
        (target / '1.96.0/NOTICE.txt').write_bytes(b'wrong generated notice')
        with self.assertRaises(licenses.LicenseError):
            licenses.check_bundle(self.repository, self.app, self.manifest)

    # The bundle check needs no toolchain or other command.
    def test_bundle_check_never_invokes_external_commands(self):
        with patch.object(licenses.subprocess, 'check_output', side_effect=AssertionError('external command')):
            licenses.check_bundle(self.repository, self.app, self.manifest)

    # The target and version are fixed, paths stay inside, components are
    # unique, and every component's notices exist among the originals.
    def test_manifest_scope_paths_and_component_coverage_are_required(self):
        manifest_file = self.repository / licenses.DIRECTORY / 'manifest.json'
        for change in ('target', 'version', 'path', 'origin', 'duplicate', 'coverage'):
            data = copy.deepcopy(self.manifest)
            if change == 'target': data['target'] = 'x86_64-apple-darwin'
            elif change == 'version': data['version'] = '1.97.0'
            elif change == 'path': data['sources'][0]['path'] = '../outside'
            elif change == 'origin': data['sources'][0]['origin']['path'] = '../outside'
            elif change == 'duplicate': data['components'] += data['components']
            else: data['components'][0]['notice_files'] = ['1.96.0/missing-license']
            manifest_file.write_bytes(canonical(data))
            with self.subTest(change=change), self.assertRaises(licenses.LicenseError):
                licenses.manifest_at(self.repository)

    # Another toolchain release, or a changed rlib, fails.
    def test_toolchain_identity_and_component_hash_are_bound(self):
        facts = 'release: 1.96.0\ncommit-hash: ' + 'a' * 40 + '\nhost: aarch64-apple-darwin\n'
        with patch.object(licenses.subprocess, 'check_output', side_effect=[facts, str(self.sysroot)]):
            self.assertEqual(self.sysroot, licenses.toolchain(self.repository, self.manifest))
        with patch.object(licenses.subprocess, 'check_output', return_value=facts.replace('1.96.0', '1.97.0')):
            with self.assertRaisesRegex(licenses.LicenseError, 'unreviewed'):
                licenses.toolchain(self.repository, self.manifest)
        (self.library / 'libstd-abc.rlib').write_bytes(b'changed archive')
        with patch.object(licenses.subprocess, 'check_output', side_effect=[facts, str(self.sysroot)]):
            with self.assertRaisesRegex(licenses.LicenseError, 'component changed'):
                licenses.toolchain(self.repository, self.manifest)

    # An rlib whose only shared member is a native object (the compiler-rt
    # assembly in compiler_builtins) still counts as included; an rlib with no
    # shared member does not. A staticlib with no objects fails.
    def test_membership_includes_native_objects_and_detects_new_component(self):
        (self.library / 'libbuiltins-def.rlib').touch()
        (self.library / 'libunused-ghi.rlib').touch()
        archive = self.root / 'libmuu_ffi.a'
        def fake(command, **kwargs):
            return {'libmuu_ffi.a': 'muu.rcgu.o\nstd.rcgu.o\nlse_cas1_acq.o\n',
                    'libstd-abc.rlib': 'lib.rmeta\nstd.rcgu.o\n',
                    'libbuiltins-def.rlib': 'lib.rmeta\nlse_cas1_acq.o\n',
                    'libunused-ghi.rlib': 'lib.rmeta\nunused.rcgu.o\n'}[Path(command[-1]).name]
        with patch.object(licenses.subprocess, 'check_output', side_effect=fake):
            self.assertEqual({'libstd-abc.rlib', 'libbuiltins-def.rlib'},
                             licenses.archive_components(self.library, archive))
        with patch.object(licenses.subprocess, 'check_output', return_value='lib.rmeta\n'):
            with self.assertRaisesRegex(licenses.LicenseError, 'no object members'):
                licenses.archive_components(self.library, archive)

    # The allowlist rows are exact, keep unrelated rows, and regenerating is a
    # no-op.
    def test_allowlist_is_exact_and_preserves_other_resources(self):
        data = b'# retained\nfile\tInfo.plist\ndirectory\tResources/THIRD_PARTY_LICENSES/rust\n'
        expected = licenses.plan(self.repository, self.manifest)
        result = licenses.allowlist_bytes(data, expected)
        self.assertEqual(result, licenses.allowlist_bytes(result, expected))
        self.assertIn(b'file\tInfo.plist\n', result)
        for name in expected:
            self.assertIn(('file\t' + licenses.CONTENTS + '/' + name + '\n').encode(), result)
        self.assertNotIn(b'*', result)


if __name__ == '__main__':
    unittest.main()
