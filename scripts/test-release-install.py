#!/usr/bin/env python3
"""Run the standalone installer with local archives, fake commands and a private HOME."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import time
import warnings
import unittest
import zipfile

REPOSITORY = Path(__file__).resolve().parents[1]
INSTALLER = REPOSITORY / 'install.sh'
HEADER = bytes.fromhex('cffaedfe0c0000010000000002000000') + bytes(16)
VERSION = '0.1.0'
NAME = 'MuuIME-' + VERSION + '-macos-arm64.zip'
TEST_BASE = 'https://muu-installer-test.invalid/releases'

# This executable never contacts a server or operates outside the fixture.
# Unregistered commands are absent from PATH, including all developer tools.
TOOL = r'''
import hashlib, json, os, pathlib, plistlib, shutil, signal, subprocess, sys, time
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
config = json.loads((root / 'config.json').read_text())
args = sys.argv[1:]
command = pathlib.Path(sys.argv[0]).name
faults = config.get('faults', [])
phase = ''
app = None
if command in ('codesign', 'plutil', 'xattr'):
    app = pathlib.Path(args[-1])
    if command == 'plutil': app = app.parent.parent
    phase = ('installed' if app == root / 'home 空白/Library/Input Methods/MuuIME.app'
             else 'stage' if app.name == 'new-MuuIME.app' else 'source')
if command == 'ditto': phase = 'extract' if '-x' in args else 'copy'
if command == 'mv':
    src, dst = map(pathlib.Path, args)
    if dst.name == 'previous-MuuIME.app': phase = 'backup'
    elif dst.name == 'rejected-MuuIME.app': phase = 'quarantine'
    elif src.name == 'previous-MuuIME.app' and dst.name == 'MuuIME.app': phase = 'restore'
    elif src.name == 'new-MuuIME.app': phase = 'replace'
    elif dst.parent.name == '.Trash': phase = 'trash'
    else: phase = 'retire'
if command == 'rm': phase = 'stage_cleanup' if '.muu-stage.' in args[-1] else 'download_cleanup'
with (root / 'events.jsonl').open('a') as log:
    log.write(json.dumps({'command': command, 'args': args, 'phase': phase}) + '\n')
def confined(path):
    if not pathlib.Path(path).absolute().is_relative_to(root):
        raise RuntimeError('fixture command escaped private directory')
def fail(name):
    if name in faults: sys.exit(19)
if command == 'uname': print(config.get('system', 'Darwin'))
elif command == 'sysctl': print(config.get('arm64', '1'))
elif command == 'sw_vers': print(config.get('os', '15.0'))
elif command == 'curl':
    expected = ['-q','-fsSL','--proto','=https','--proto-redir','=https','--connect-timeout','15',
                '--max-time','900','--max-redirs','10','--max-filesize',
                '4096' if args[-1].endswith('/SHA256SUMS') else str(128 * 1024 * 1024),'--output']
    if args[:len(expected)] != expected or len(args) != len(expected)+2: sys.exit(21)
    out = pathlib.Path(args[-2]); confined(out)
    url = args[-1]
    if url.endswith('/SHA256SUMS'):
        if url not in (config['base']+'/latest/download/SHA256SUMS',
                       config['base']+'/download/v0.1.0/SHA256SUMS'): sys.exit(22)
        out.write_bytes((root / 'SHA256SUMS').read_bytes())
        fail('checksum_download')
        if 'block_curl' in faults:
            while not (root/'continue').exists(): time.sleep(0.01)
    else:
        if url != config['base']+'/download/v0.1.0/MuuIME-0.1.0-macos-arm64.zip': sys.exit(23)
        data = (root / 'release.zip').read_bytes()
        out.write_bytes(data[:len(data)//2] if 'zip_download' in faults else data)
        if 'download_oversize' in faults:
            with out.open('wb') as stream: stream.truncate(128 * 1024 * 1024 + 1)
        fail('zip_download')
    if stat_mode := out.stat().st_mode & 0o777:
        if stat_mode != 0o600: sys.exit(24)
elif command == 'plutil':
    if args[:1] != ['-extract'] or args[2:5] != ['raw','-o','-']: sys.exit(25)
    if not app.is_dir(): sys.exit(26)
    fail('plist_'+phase)
    value = plistlib.loads((app/'Contents/Info.plist').read_bytes())[args[1]]
    print(value)
elif command == 'codesign':
    if not app.is_dir(): sys.exit(27)
    if args[:1] == ['--verify']:
        if args[:-1] != ['--verify','--deep','--strict','--verbose=2']: sys.exit(28)
        fail('signature_'+phase)
    elif args[:-1] == ['-d','--verbose=4']:
        print('Signature=' + ('invalid' if 'not_adhoc' in faults else 'adhoc'))
        print('CDHash=' + hashlib.sha1((app/'Contents/MacOS/MuuIME').read_bytes()).hexdigest())
    else: sys.exit(29)
elif command == 'ditto':
    for path in args[-2:]: confined(path)
    if phase == 'extract':
        fail('extract')
        subprocess.run(['/usr/bin/ditto', *args], check=True)
        if 'expanded_oversize' in faults:
            large=pathlib.Path(args[-1])/'MuuIME.app/Contents/Resources/large.bin'
            with large.open('wb') as stream: stream.truncate(512 * 1024 * 1024 + 1)
    else:
        if not pathlib.Path(args[-2]).is_dir(): sys.exit(30)
        if 'copy' in faults:
            pathlib.Path(args[-1]).mkdir()
            sys.exit(31)
        subprocess.run(['/usr/bin/ditto', *args], check=True)
        if 'stage_changed' in faults:
            (pathlib.Path(args[-1])/'Contents/Resources/data.txt').write_bytes(b'changed')
elif command == 'mv':
    for path in args: confined(path)
    if phase == 'trash' and 'retirement_signal' in config:
        os.kill(os.getppid(), config['retirement_signal'])
        sys.exit(19)
    fail(phase)
    if phase == 'replace' and 'target_created' in faults:
        dst.mkdir()
        (dst/'external-sentinel').write_bytes(b'keep external directory')
    subprocess.run(['/bin/mv', *args], check=True)
    if phase == 'replace' and 'target_swapped' in faults:
        dst.rename(root/'moved-new.app')
        dst.mkdir()
        (dst/'external-sentinel').write_bytes(b'keep external directory')
    if phase == 'trash' and 'retirement_signal_after' in config:
        os.kill(os.getppid(), config['retirement_signal_after'])
        sys.exit(19)
    if phase == 'replace':
        if 'installed_changed' in faults:
            with (pathlib.Path(args[1])/'Contents/MacOS/MuuIME').open('ab') as stream: stream.write(b'changed')
        if 'installed_resource' in faults:
            (pathlib.Path(args[1])/'Contents/Resources/data.txt').write_bytes(b'changed')
        if 'signal' in config: os.kill(os.getppid(), config['signal'])
    fail(phase+'_after')
elif command == 'rm':
    confined(args[-1]); fail(phase)
    subprocess.run(['/bin/rm', *args], check=True)
elif command == 'stat':
    confined(args[-1])
    if 'trash_device' in faults and pathlib.Path(args[-1]).name == '.Trash': print('different')
    elif ('cleanup_rejected_swapped' in faults and args[:2] == ['-f','%d:%i'] and
          pathlib.Path(args[-1]).name == 'rejected-MuuIME.app'):
        target=pathlib.Path(args[-1])
        value=subprocess.check_output(['/usr/bin/stat', *args], text=True)
        target.rename(root/'displaced-owned.app')
        target.mkdir()
        (target/'external-sentinel').write_bytes(b'keep external directory')
        (root/'race-injected').write_text('after quarantine stat, before cleanup')
        sys.stdout.write(value)
    elif ('cleanup_target_swapped' in faults and args[:2] == ['-f','%d:%i'] and
          pathlib.Path(args[-1]) == root/'home 空白/Library/Input Methods/MuuIME.app'):
        counter=root/'identity-calls'
        count=int(counter.read_text())+1 if counter.exists() else 1
        counter.write_text(str(count))
        value=subprocess.check_output(['/usr/bin/stat', *args], text=True)
        if count == 2:
            # Installed verification has failed. Return cleanup's valid stat
            # result after another process replaces that exact pathname.
            target=pathlib.Path(args[-1])
            target.rename(root/'displaced-owned.app')
            target.mkdir()
            (target/'external-sentinel').write_bytes(b'keep external directory')
            (root/'race-injected').write_text('after stat, before cleanup mv')
        sys.stdout.write(value)
    else: subprocess.run(['/usr/bin/stat', *args], check=True)
elif command == 'xattr':
    confined(args[-1])
    subprocess.run(['/usr/bin/xattr', *args], check=True)
elif command == 'unzip':
    subprocess.run(['/usr/bin/unzip', *args], check=True)
else:
    raise RuntimeError('unknown fixture tool')
'''


class Installation:
    def __init__(self, old=True):
        self.temporary = tempfile.TemporaryDirectory(prefix='muu-release-install-')
        self.root = Path(self.temporary.name).resolve()
        self.home = self.root / 'home 空白'
        self.target = self.home / 'Library/Input Methods/MuuIME.app'
        self.target.parent.mkdir(parents=True)
        self.old = HEADER + b'old synthetic app'
        self.new = HEADER + b'new synthetic app'
        if old:
            (self.target / 'Contents/MacOS').mkdir(parents=True)
            (self.target / 'Contents/MacOS/MuuIME').write_bytes(self.old)
            (self.target / 'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier':'dev.muu.inputmethod.Muu'}))
        self.source = self.root / 'MuuIME.app'
        (self.source / 'Contents/MacOS').mkdir(parents=True)
        (self.source / 'Contents/Resources').mkdir()
        (self.source / 'Contents/MacOS/MuuIME').write_bytes(self.new)
        (self.source / 'Contents/MacOS/MuuIME').chmod(0o755)
        (self.source / 'Contents/Resources/data.txt').write_bytes(b'public synthetic resource')
        self.info = {'CFBundleIdentifier':'dev.muu.inputmethod.Muu', 'CFBundleExecutable':'MuuIME',
                     'CFBundlePackageType':'APPL', 'MuuBuildVariant':'standard',
                     'CFBundleShortVersionString':VERSION, 'CFBundleVersion':'1',
                     'MuuSourceCommit':'a'*40, 'LSMinimumSystemVersion':'15.0'}
        self.config = {'base':TEST_BASE, 'faults':[]}
        self.events_path = self.root / 'events.jsonl'
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        launcher = '#!' + sys.executable + '\n' + TOOL
        for name in ('curl','uname','sysctl','sw_vers','plutil','codesign','ditto','mv','rm','stat','xattr','unzip'):
            path = self.bin / name
            path.write_text(launcher)
            path.chmod(0o755)
        for name in ('awk','grep','sed','shasum','tr','od','wc','cat','cmp','diff','find','mkdir','mktemp','rmdir','zipinfo'):
            actual = shutil.which(name, path='/usr/bin:/bin')
            if actual is None: raise RuntimeError('missing system fixture command: ' + name)
            (self.bin / name).symlink_to(actual)
        (self.root / 'tmp').mkdir()
        self.env = dict(os.environ, HOME=str(self.home), TMPDIR=str(self.root/'tmp'), PATH=str(self.bin),
                        MUU_INSTALL_RELEASE_BASE_URL=TEST_BASE, FIXTURE_ROOT=str(self.root))
        self.data = self.home / 'Library/Application Support/MuuIME/synthetic-state'
        self.data.parent.mkdir(parents=True)
        self.data.write_bytes(b'never read or changed by the installer')
        self.make_zip()

    def close(self):
        self.temporary.cleanup()

    def make_zip(self, entries=None):
        (self.source/'Contents/Info.plist').write_bytes(plistlib.dumps(self.info))
        path = self.root / 'release.zip'
        if entries is None:
            subprocess.run(['/usr/bin/ditto','-c','-k','--keepParent','--norsrc','--noqtn',
                            str(self.source),str(path)],check=True)
        else:
            with zipfile.ZipFile(path,'w') as archive:
                for name, mode, data in entries:
                    entry=zipfile.ZipInfo(name)
                    entry.create_system=3
                    entry.external_attr=mode<<16
                    with warnings.catch_warnings():
                        warnings.simplefilter('ignore',UserWarning)
                        archive.writestr(entry,data)
        (self.root/'SHA256SUMS').write_text(hashlib.sha256(path.read_bytes()).hexdigest()+'  '+NAME+'\n')

    def run(self, *arguments, expected=0, installer=INSTALLER):
        (self.root/'config.json').write_text(json.dumps(self.config))
        try:
            result=subprocess.run(['/bin/sh',str(installer),*arguments],env=self.env,
                                  capture_output=True,text=True,timeout=30)
        except subprocess.TimeoutExpired as error:
            output=REPOSITORY/'build/test-evidence'
            output.mkdir(parents=True,exist_ok=True)
            destination=Path(tempfile.mkdtemp(prefix='release-installer-timeout-',dir=output))
            for name,value in (('stdout',error.stdout),('stderr',error.stderr)):
                (destination/name).write_bytes(value or b'')
            if self.events_path.exists(): shutil.copyfile(self.events_path,destination/'events.jsonl')
            raise AssertionError('installer timeout; evidence: '+str(destination)) from error
        if expected == 0 and result.returncode != 0:
            raise AssertionError(result.stdout+result.stderr)
        if expected != 0 and result.returncode == 0:
            raise AssertionError('unexpected installer success: '+result.stdout)
        if self.data.read_bytes() != b'never read or changed by the installer':
            raise AssertionError('user-state sentinel changed')
        return result

    def events(self, command=None):
        events=[json.loads(line) for line in self.events_path.read_text().splitlines()] if self.events_path.exists() else []
        return [event for event in events if command is None or event['command']==command]

    def old_copies(self):
        return [p for p in self.home.rglob('MuuIME') if p.is_file() and p.read_bytes()==self.old]


class InstallerTests(unittest.TestCase):
    # A download over 128 MiB stops before extraction, and an expanded app over
    # 512 MiB of logical bytes (a sparse file here) stops before the bundle is
    # verified. Either way the previous app is never backed up or moved.
    def test_download_and_expanded_size_limits_precede_replacement(self):
        for fault, message in [('download_oversize', 'download size limit'),
                               ('expanded_oversize', 'expanded app size limit')]:
            with self.subTest(fault=fault):
                f=self.fixture(); f.config['faults']=[fault, 'signature_source']
                result=f.run(expected=1)
                self.assertIn(message, result.stderr)
                self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)
                self.assertFalse(any(e['phase']=='backup' for e in f.events()))
                if fault=='download_oversize':
                    self.assertFalse(f.events('ditto'))
                else:
                    self.assertFalse(any(e['phase']=='source' for e in f.events('codesign')))

    # A declared size over the cap or more than 1,024 entries is refused from
    # the listing alone, before unzip -t or ditto inflates anything.
    def test_archive_size_and_entry_count_are_bounded_before_inflation(self):
        for excessive in ('size', 'entries'):
            with self.subTest(excessive=excessive):
                f=self.fixture()
                path=f.root/'release.zip'
                if excessive=='size':
                    data=bytearray(path.read_bytes())
                    offset=data.index(b'PK\x01\x02')
                    struct.pack_into('<I',data,offset+24,512 * 1024 * 1024 + 1)
                    path.write_bytes(data)
                else:
                    entries=[('MuuIME.app/', stat.S_IFDIR|0o700, b'')]
                    entries += [('MuuIME.app/f'+str(i), stat.S_IFREG|0o600, b'') for i in range(1024)]
                    f.make_zip(entries)
                (f.root/'SHA256SUMS').write_text(hashlib.sha256(path.read_bytes()).hexdigest()+'  '+NAME+'\n')
                f.run(expected=1)
                self.assertFalse(f.events('unzip'), 'archive limits must reject before inflation')
                self.assertFalse(f.events('ditto'))
                self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)

    # With a lock left behind, the error names the lock and every stage, and
    # the stage (which may hold the only previous app) is left untouched.
    def test_abandoned_lock_reports_lock_and_preserved_stage_paths(self):
        f=self.fixture()
        lock=f.target.parent/'.muu-install-lock'; lock.mkdir()
        stage=f.target.parent/'.muu-stage.abandoned'; stage.mkdir()
        (stage/'sentinel').write_bytes(b'preserved')
        result=f.run(expected=1)
        self.assertIn(str(lock),next(line for line in result.stderr.splitlines() if line.startswith('Muu IME:')))
        self.assertIn(str(stage),result.stderr)
        self.assertEqual((stage/'sentinel').read_bytes(),b'preserved')
        self.assertFalse(f.events('curl'))

    # Another process creates, or swaps in, a directory at the destination just
    # as the new app is renamed. Its contents survive untouched, and the
    # previous app stays in the kept stage instead of being lost.
    def test_destination_races_preserve_external_directory_and_previous_app(self):
        for fault in ('target_created','target_swapped'):
            for old in (True,False):
                with self.subTest(fault=fault,old=old):
                    f=self.fixture(old); f.config['faults']=[fault]
                    f.run(expected=1)
                    self.assertEqual((f.target/'external-sentinel').read_bytes(),b'keep external directory')
                    self.assertEqual(len(f.old_copies()),int(old))
                    if old:
                        self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))

    # A correct observation becomes stale before quarantine moves the path.
    # Both fresh installs and upgrades must retain that foreign directory.
    def test_cleanup_rechecks_identity_after_quarantine_move(self):
        for old in (True,False):
            with self.subTest(old=old):
                f=self.fixture(old)
                f.config['faults']=['signature_installed','cleanup_target_swapped']
                result=f.run(expected=1)
                self.assertTrue((f.root/'race-injected').is_file())
                survivors=list(f.root.rglob('external-sentinel'))
                self.assertEqual(len(survivors),1)
                self.assertEqual(survivors[0].read_bytes(),b'keep external directory')
                self.assertEqual(survivors[0].parent.name,'rejected-MuuIME.app')
                self.assertIn(str(survivors[0].parent),result.stderr)
                self.assertEqual(len(f.old_copies()),int(old))
                self.assertFalse(any(event['phase']=='stage_cleanup' for event in f.events('rm')))

    # Even a matching inode cannot authorize deletion: another same-user
    # process may replace a path inside the private stage after that stat.
    def test_failed_quarantine_keeps_stage_after_final_identity_observation(self):
        for old in (True,False):
            with self.subTest(old=old):
                f=self.fixture(old)
                f.config['faults']=['signature_installed','cleanup_rejected_swapped']
                result=f.run(expected=1)
                self.assertTrue((f.root/'race-injected').is_file())
                survivors=list(f.root.rglob('external-sentinel'))
                self.assertEqual(len(survivors),1)
                self.assertEqual(survivors[0].read_bytes(),b'keep external directory')
                stage=survivors[0].parent.parent
                self.assertTrue(stage.name.startswith('.muu-stage.'))
                self.assertIn(str(stage),result.stderr)
                self.assertEqual(len(f.old_copies()),int(old))
                self.assertFalse(any(event['phase']=='stage_cleanup' for event in f.events('rm')))

    def test_failed_quarantine_is_retained_and_reported_without_a_race(self):
        for old in (True,False):
            with self.subTest(old=old):
                f=self.fixture(old); f.config['faults']=['signature_installed']
                result=f.run(expected=1)
                rejected=list(f.target.parent.glob('.muu-stage.*/rejected-MuuIME.app'))
                self.assertEqual(len(rejected),1)
                self.assertEqual((rejected[0]/'Contents/MacOS/MuuIME').read_bytes(),f.new)
                self.assertIn(str(rejected[0].parent),result.stderr)
                self.assertEqual(len(f.old_copies()),int(old))
                self.assertFalse(any(event['phase']=='stage_cleanup' for event in f.events('rm')))

    def fixture(self, old=True):
        fixture=Installation(old)
        self.addCleanup(fixture.close)
        return fixture

    # Fresh install, update and an explicit version: the new bytes land, at most
    # one previous copy goes to Trash, scratch and lock are removed, and the
    # ZIP is always fetched from the version named by SHA256SUMS.
    def test_install_update_and_version_urls(self):
        for old,args in ((False,()),(True,()),(True,('--version','v0.1.0'))):
            with self.subTest(old=old,args=args):
                f=self.fixture(old)
                f.run(*args)
                self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.new)
                self.assertEqual(len(f.old_copies()),int(old))
                self.assertFalse(list(f.root.glob('tmp/muu-install.*')))
                self.assertFalse((f.target.parent/'.muu-install-lock').exists())
                urls=[e['args'][-1] for e in f.events('curl')]
                prefix='/download/v0.1.0/' if args else '/latest/download/'
                self.assertEqual(urls,[TEST_BASE+prefix+'SHA256SUMS',TEST_BASE+'/download/v0.1.0/'+NAME])
                phases=[e['phase'] for e in f.events() if e['command']=='mv']
                self.assertEqual(phases,['backup','replace','trash'] if old else ['replace'])

    # Without the test override the real release URL is used, and a second
    # installer refuses at the lock without downloading anything.
    def test_default_endpoint_and_concurrent_installer(self):
        f=self.fixture()
        f.env.pop('MUU_INSTALL_RELEASE_BASE_URL')
        f.config['base']='https://github.com/naoto256/muu-ime/releases'
        f.run()
        self.assertEqual(f.events('curl')[0]['args'][-1],f.config['base']+'/latest/download/SHA256SUMS')
        f=self.fixture(); f.config['faults']=['block_curl']
        (f.root/'config.json').write_text(json.dumps(f.config))
        first=subprocess.Popen(['/bin/sh',str(INSTALLER)],env=f.env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            deadline=time.monotonic()+5
            while not f.events('curl'):
                if time.monotonic()>deadline: self.fail('first installer did not reach download')
                time.sleep(0.01)
            f.run(expected=1)
            self.assertEqual(len(f.events('curl')),1)
        finally:
            (f.root/'continue').touch()
            stdout,stderr=first.communicate(timeout=30)
        self.assertEqual(first.returncode,0,(stdout+stderr).decode())

    # A local header name that disagrees with the central directory is caught
    # by the archive test before extraction ever runs.
    def test_local_central_filename_disagreement_rejected_before_ditto(self):
        f=self.fixture()
        f.make_zip([('MuuIME.app/',stat.S_IFDIR|0o755,b''),
                    ('MuuIME.app/ok',stat.S_IFREG|0o644,b'fixture')])
        archive=f.root/'release.zip'
        data=archive.read_bytes()
        data=data.replace(b'MuuIME.app/ok',b'../xxxxxxxxxx',1)
        archive.write_bytes(data)
        (f.root/'SHA256SUMS').write_text(hashlib.sha256(data).hexdigest()+'  '+NAME+'\n')
        f.run(expected=1)
        self.assertFalse(f.events('ditto'))

    # Download failures and every malformed SHA256SUMS (empty, extra line, path,
    # upper-case digest, oversized, other version) stop before extraction and
    # leave the previous app where it was.
    def test_network_and_manifest_failures_leave_old_untouched(self):
        for fault in ('checksum_download','zip_download'):
            f=self.fixture(); f.config['faults']=[fault]; f.run(expected=1)
            self.assertEqual(len(f.events('ditto')),0)
            self.assertEqual(len(f.old_copies()),1)
        for manifest in ('','a'*64+'  '+NAME+'\nextra','a'*64+'  ../bad.zip\n',
                         ('a'*64+'  '+NAME+'\n')*2,'A'*64+'  '+NAME+'\n',
                         'x'*4097, 'a'*64+'  MuuIME-0.2.0-macos-arm64.zip\n'):
            with self.subTest(manifest=manifest[:20]):
                f=self.fixture(); (f.root/'SHA256SUMS').write_text(manifest)
                f.run('--version','0.1.0',expected=1)
                self.assertEqual(len(f.events('ditto')),0)
                self.assertEqual(len(f.old_copies()),1)

    # A ZIP that is otherwise valid but does not match the checksum is refused
    # before extraction, so the SHA check is not masked by later checks.
    def test_hash_mismatch_is_rejected_before_extraction(self):
        f=self.fixture()
        # Change only the ZIP comment: every app byte and the ZIP structure
        # remain valid, so a later archive/bundle rejection cannot mask SHA.
        with zipfile.ZipFile(f.root/'release.zip','a') as archive:
            archive.comment=b'synthetic archive comment'
        subprocess.run(['/usr/bin/unzip','-t',str(f.root/'release.zip')],check=True,capture_output=True)
        expected=(f.root/'SHA256SUMS').read_text().split()[0]
        self.assertNotEqual(hashlib.sha256((f.root/'release.zip').read_bytes()).hexdigest(),expected)
        f.run(expected=1)
        self.assertFalse(f.events('ditto'))
        self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)

    # A correct digest without the final LF is still refused, and the ZIP is
    # not even downloaded: the manifest format is checked as a whole.
    def test_manifest_requires_terminal_lf_with_valid_digest(self):
        f=self.fixture()
        manifest=(f.root/'SHA256SUMS').read_bytes()
        self.assertTrue(manifest.endswith(b'\n'))
        self.assertEqual(manifest.split()[0].decode(),hashlib.sha256((f.root/'release.zip').read_bytes()).hexdigest())
        (f.root/'SHA256SUMS').write_bytes(manifest[:-1])
        f.run(expected=1)
        self.assertEqual(len(f.events('curl')),1)
        self.assertFalse(f.events('ditto'))
        self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)

    # Entries outside MuuIME.app, unsafe names, links, FIFOs, case-insensitive
    # duplicates and paths through a link never reach ditto, and a file outside
    # the fixture stays intact.
    def test_archive_boundary_rejected_before_ditto(self):
        root=[('MuuIME.app/',stat.S_IFDIR|0o755,b'')]
        invalid=[('../outside',stat.S_IFREG|0o644,b'x'),('/outside',stat.S_IFREG|0o644,b'x'),
                 ('MuuIME.app/../outside',stat.S_IFREG|0o644,b'x'),
                 ('MuuIME.app/bad\nname',stat.S_IFREG|0o644,b'x'),
                 ('MuuIME.app/link',stat.S_IFLNK|0o777,b'../outside'),
                 ('MuuIME.app/fifo',stat.S_IFIFO|0o644,b''),
                 ('MuuIME.app//bad',stat.S_IFREG|0o644,b'x'),
                 ('MuuIME.app/./bad',stat.S_IFREG|0o644,b'x'),
                 ('MuuIME.app/bad\\name',stat.S_IFREG|0o644,b'x'),
                 ('Other.app/',stat.S_IFDIR|0o755,b'')]
        matrices=[root+[entry] for entry in invalid]
        matrices += [root+[('MuuIME.app/A',stat.S_IFREG|0o644,b'x'),
                           ('MuuIME.app/'+name,stat.S_IFREG|0o644,b'x')] for name in ('A','a')]
        matrices += [root+[('MuuIME.app/link',stat.S_IFLNK|0o777,b'../outside'),
                           ('MuuIME.app/link/file',stat.S_IFREG|0o644,b'x')]]
        for entries in matrices:
            with self.subTest(entries=entries[-1][0]):
                f=self.fixture(); sentinel=f.root/'outside'; sentinel.write_bytes(b'intact')
                f.make_zip(entries); f.run(expected=1)
                self.assertFalse(f.events('ditto'))
                self.assertEqual(sentinel.read_bytes(),b'intact')

    # A different identifier, executable, package type, variant, version, build,
    # commit or minimum OS, a non-arm64 or truncated executable, and a missing
    # execute bit each stop the install before anything is moved.
    def test_bundle_fields_and_macho_are_checked(self):
        cases={'CFBundleIdentifier':'other.app','CFBundleExecutable':'other','CFBundlePackageType':'BNDL',
               'MuuBuildVariant':'dogfood','CFBundleShortVersionString':'0.2.0','CFBundleVersion':'0',
               'MuuSourceCommit':'not a commit','LSMinimumSystemVersion':'99.0'}
        for key,value in cases.items():
            with self.subTest(key=key):
                f=self.fixture(); f.info[key]=value; f.make_zip(); f.run(expected=1)
                self.assertEqual(len(f.old_copies()),1)
                self.assertFalse(f.events('mv'))
        for data in (b'truncated',bytes.fromhex('cafebabe')+bytes(60),
                     HEADER[:4]+bytes.fromhex('07000001')+HEADER[8:],
                     HEADER[:8]+bytes.fromhex('02000000')+HEADER[12:]):
            f=self.fixture(); (f.source/'Contents/MacOS/MuuIME').write_bytes(data)
            f.make_zip(); f.run(expected=1); self.assertFalse(f.events('mv'))
        f=self.fixture(); (f.source/'Contents/MacOS/MuuIME').chmod(0o644)
        f.make_zip(); f.run(expected=1)

    # Any failure before the backup rename leaves the previous app installed and
    # untouched, and the failure happens at the expected phase.
    def test_all_prepublication_faults_preserve_old(self):
        for fault in ('extract','copy','signature_source','signature_stage','plist_stage','stage_changed','not_adhoc','backup'):
            with self.subTest(fault=fault):
                f=self.fixture(); f.config['faults']=[fault]; f.run(expected=1)
                self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)
                self.assertEqual(len(f.old_copies()),1)
                expected_phase={'signature_source':'source','signature_stage':'stage',
                                'plist_stage':'stage','stage_changed':'stage','not_adhoc':'source'}.get(fault,fault)
                self.assertIn(expected_phase,[event['phase'] for event in f.events()])

    # Failures during or after replacement roll back: the previous app returns,
    # or, on a fresh install, the unverified copy leaves the destination.
    def test_failed_replacement_restores_or_removes_new_copy(self):
        for old in (True,False):
            for fault in ('backup_after','replace','replace_after','installed_changed',
                          'installed_resource','signature_installed','plist_installed'):
                if not old and fault=='backup_after': continue
                with self.subTest(old=old,fault=fault):
                    f=self.fixture(old); f.config['faults']=[fault]; f.run(expected=1)
                    if old: self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)
                    else: self.assertFalse(f.target.exists())
                    self.assertEqual(len(f.old_copies()),int(old))
                    expected_phase=('backup' if fault=='backup_after' else 'replace'
                                    if fault in ('replace','replace_after') else 'installed')
                    self.assertIn(expected_phase,[event['phase'] for event in f.events()])

    # When rollback itself fails, the stage holding the previous app is kept and
    # reported instead of being cleaned up.
    def test_recovery_failures_keep_the_only_old_copy(self):
        for fault in ('restore','quarantine'):
            f=self.fixture(); f.config['faults']=['installed_changed',fault]; result=f.run(expected=1)
            self.assertEqual(len(f.old_copies()),1)
            self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))
            self.assertIn('退避',result.stderr)

    # Once the new app is verified, failures to trash the old one or to clean up
    # never undo the install; the old copy is kept, and cleanup errors set the
    # exit status.
    def test_trash_or_cleanup_failure_never_rolls_back_verified_new(self):
        for fault in ('trash','trash_after','trash_device','stage_cleanup','download_cleanup'):
            f=self.fixture(); f.config['faults']=[fault]
            f.run(expected=1 if fault.endswith('cleanup') else 0)
            self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.new)
            self.assertEqual(len(f.old_copies()),1)

    # A relative, absolute or dangling link at the app, Input Methods, Library
    # or HOME is refused before any download. A linked Trash is not used; the
    # previous app stays in the stage.
    def test_symlink_ancestors_and_target_are_rejected_before_network(self):
        for part in ('app','input','library','home'):
            for link in ('relative','absolute','dangling'):
                with self.subTest(part=part,link=link):
                    f=self.fixture()
                    path={'app':f.target,'input':f.target.parent,'library':f.home/'Library','home':f.home}[part]
                    aside=path.with_name(path.name+'-original')
                    if f.data.is_relative_to(path): f.data=aside/f.data.relative_to(path)
                    path.rename(aside)
                    path.symlink_to(aside if link=='absolute' else aside.name if link=='relative' else 'missing')
                    f.run(expected=1)
                    self.assertFalse(f.events('curl'))
        f=self.fixture(); (f.home/'.Trash').symlink_to(f.root/'missing')
        f.run(); self.assertEqual(len(f.old_copies()),1)
        self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))

    # Bad arguments, another OS or CPU, an old macOS, an existing lock, a bad
    # HOME, a non-HTTPS override and a file in place of Input Methods all stop
    # before the network.
    def test_invalid_arguments_platform_parent_or_lock_do_not_download(self):
        for args in (('--unknown',),('--version','../1'),('--version',),('--version','1.2.3','--version','1.2.3')):
            f=self.fixture(); f.run(*args,expected=1); self.assertFalse(f.events('curl'))
        for key,value in (('system','Linux'),('arm64','0'),('os','14.0')):
            f=self.fixture(); f.config[key]=value; f.run(expected=1); self.assertFalse(f.events('curl'))
        f=self.fixture(); (f.target.parent/'.muu-install-lock').mkdir()
        f.run(expected=1); self.assertFalse(f.events('curl'))
        for home in ('','relative'):
            f=self.fixture(); f.env['HOME']=home; f.run(expected=1); self.assertFalse(f.events('curl'))
        f=self.fixture(); f.env['MUU_INSTALL_RELEASE_BASE_URL']='http://example.com/releases'
        f.run(expected=1); self.assertFalse(f.events('curl'))
        f=self.fixture(); shutil.rmtree(f.target.parent); f.target.parent.write_bytes(b'file')
        f.run(expected=1); self.assertFalse(f.events('curl'))

    # HUP, INT and TERM during installation exit with 128+signal after restoring
    # the previous app. A script cut off mid-download runs nothing at all.
    def test_signals_restore_old_and_truncated_script_never_starts(self):
        for number in (signal.SIGHUP,signal.SIGINT,signal.SIGTERM):
            f=self.fixture(); f.config['signal']=int(number); result=f.run(expected=1)
            self.assertEqual(result.returncode,128+number)
            self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)
        f=self.fixture(); partial=f.root/'truncated.sh'
        data=INSTALLER.read_text(); partial.write_text(data[:data.index('muu_main() {')+80])
        f.run(expected=1,installer=partial)
        self.assertFalse(f.events())

    # Uninstall removes only the app, never touches the network, and leaves
    # every user data area byte-for-byte unchanged.
    def test_uninstall_or_noop_never_downloads(self):
        for old in (True,False):
            f=self.fixture(old)
            # Freeze the stores' actual paths independently of their code.
            # Opaque synthetic bytes need not be decoded during uninstall.
            data_root=f.home/'Library/Application Support/dev.muu.inputmethod.Muu'
            generation='a'*64
            files=[data_root/'user_dict/user-dictionary.tsv',
                   data_root/'personal_frequency/personal-frequency-v1.json',
                   data_root/'imported_dict/catalog-v1.json',
                   data_root/'imported_dict/v2/current',
                   data_root/('imported_dict/v2/blobs/'+generation+'.muudict'),
                   data_root/('imported_dict/v2/generations/'+generation+'.json'),
                   data_root/'postal_code/v1/current',
                   data_root/('postal_code/v1/generations/'+generation+'/postal-code.tsv'),
                   data_root/('postal_code/v1/generations/'+generation+'/manifest.json'),
                   f.home/'Library/Application Support/MuuIME/Models/synthetic-model',
                   f.home/'Library/Preferences/dev.muu.inputmethod.Muu.plist']
            for index,path in enumerate(files):
                path.parent.mkdir(parents=True,exist_ok=True)
                path.write_bytes(bytes([index])*13)
            before={str(path):path.read_bytes() for path in files}
            f.run('--uninstall')
            self.assertFalse(f.target.exists())
            self.assertEqual(len(f.old_copies()),int(old))
            self.assertFalse(f.events('curl'))
            self.assertFalse(f.events('ditto'))
            self.assertFalse(f.events('codesign'))
            for path in files:
                self.assertTrue(path.is_file(),str(path))
            self.assertEqual({str(path):path.read_bytes() for path in files},before)
            self.assertFalse((f.target.parent/'.muu-install-lock').exists())

    # When Trash cannot take the app, uninstall keeps it in the stage and says so.
    def test_uninstall_keeps_app_when_trash_is_unusable(self):
        for fault in ('trash','trash_device'):
            f=self.fixture(); f.config['faults']=[fault]
            result=f.run('--uninstall')
            self.assertFalse(f.target.exists())
            self.assertEqual(len(f.old_copies()),1)
            self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))
            self.assertIn('退避',result.stderr)
            self.assertFalse(f.events('curl'))

    # A failed move out of the destination puts the app back, or keeps the stage
    # when even that fails; the one copy always survives.
    def test_uninstall_move_failure_restores_or_preserves_app(self):
        for faults in (['backup'],['backup_after'],['backup_after','restore']):
            f=self.fixture(); f.config['faults']=faults
            f.run('--uninstall',expected=1)
            self.assertEqual(len(f.old_copies()),1)
            self.assertFalse(f.events('curl'))
            if 'restore' in faults:
                self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))
            else:
                self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.old)

    # Uninstall refuses conflicting arguments, an app with another identifier and
    # a symlinked destination without moving anything.
    def test_uninstall_refuses_conflicts_foreign_app_and_symlink(self):
        for args in (('--version','0.1.0','--uninstall'),('--uninstall','--version','0.1.0'),
                     ('--uninstall','--uninstall')):
            f=self.fixture(); f.run(*args,expected=1)
            self.assertFalse(f.events('mv')); self.assertFalse(f.events('curl'))
        f=self.fixture()
        (f.target/'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier':'other.app'}))
        f.run('--uninstall',expected=1)
        self.assertFalse(f.events('mv')); self.assertEqual(len(f.old_copies()),1)
        f=self.fixture(); aside=f.target.with_name('original.app'); f.target.rename(aside)
        f.target.symlink_to('missing')
        f.run('--uninstall',expected=1)
        self.assertFalse(f.events('mv')); self.assertFalse(f.events('curl'))

    # A signal just before or just after the Trash move still leaves exactly one
    # previous copy, in the stage or in Trash, and never undoes a verified
    # install. This is why cleanup keeps any stage holding the previous app.
    def test_retirement_signals_keep_the_old_app_before_or_after_trash(self):
        for args in ((),('--uninstall',)):
            for number in (signal.SIGHUP,signal.SIGINT,signal.SIGTERM):
                for point in ('retirement_signal','retirement_signal_after'):
                    with self.subTest(args=args,signal=number,point=point):
                        f=self.fixture(); f.config[point]=int(number)
                        result=f.run(*args,expected=1)
                        self.assertEqual(result.returncode,128+number)
                        self.assertEqual(len(f.old_copies()),1)
                        self.assertIn('trash',[event['phase'] for event in f.events('mv')])
                        if point=='retirement_signal':
                            self.assertTrue(list(f.target.parent.glob('.muu-stage.*/previous-MuuIME.app')))
                        else:
                            self.assertTrue(list((f.home/'.Trash').glob('MuuIME-previous-*.app')))
                        if not args:
                            self.assertEqual((f.target/'Contents/MacOS/MuuIME').read_bytes(),f.new)
                        else:
                            self.assertFalse(f.target.exists())


if __name__=='__main__':
    unittest.main()
