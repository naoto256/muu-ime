#!/usr/bin/env python3
"""Regress the CI dependency boundary without executing fixture commands."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / 'scripts/check-ci-commands.py'
spec = importlib.util.spec_from_file_location('ci_commands', CHECKER)
commands = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = commands
spec.loader.exec_module(commands)


class CommandTests(unittest.TestCase):
    def test_shell_command_positions_include_conditions_pipes_wrappers_and_substitutions(self):
        sources = [
            'rg --version',
            'if rg needle tree; then echo found; fi',
            'printf data | rg needle',
            'result="$(rg needle tree)"',
            'printf "%s" "$(rg needle tree)"',
            'printf "%s" "$(rg "$(cat tree)")"',
            'env KEY=value nice -n 19 rg needle tree',
            'xcrun rg needle',
            'command -v rg',
            'scan() { rg needle tree; }; scan',
            "trap 'rg needle tree' EXIT",
            'for name in one two; do rg needle "$name"; done',
            'case "$value" in one) rg needle;; *) echo no;; esac',
            "python3 -c 'import subprocess; subprocess.run([\"rg\", \"needle\"])'",
            "python3 - <<'PY'\nimport subprocess\nsubprocess.run(['rg'])\nPY\n",
            "python3 -c 'import subprocess,sys; subprocess.run(sys.argv[1:])' rg needle",
        ]
        for source in sources:
            with self.subTest(source=source):
                found = commands.shell_commands(source, 'fixture.sh')
                self.assertIn('rg', {item.name for item in found if not item.dynamic})

    def test_shell_data_binary_flags_and_control_syntax_are_not_commands(self):
        source = r'''# don't run the example word rg
scan() { grep -r -n -E -I 'name' "$@"; }
printf '%s\n' rg '|' ';'
cat <<'DATA'
rg is only fixture data here
DATA
for value in rg two; do printf '%s' "$value"; done
case "$value" in rg) scan tree;; *) echo safe;; esac
if [[ -e "$file" && -f "$file" ]]; then echo "${value##*/}"; fi
find tree \( -type f -o -type l \) -print
attempt=$((attempt + 1))
'''
        found = {item.name for item in commands.shell_commands(source, 'fixture.sh') if not item.dynamic}
        self.assertEqual({'grep', 'cat', 'find'}, found)

    def test_python_argv_aliases_forwarders_shell_and_executable_override(self):
        sources = [
            'import subprocess\nsubprocess.run(["rg", "needle"])',
            'import subprocess as child\nchild.check_output(["rg"])',
            'from subprocess import run as child\nchild(["rg"])',
            'import subprocess\nargv=["rg", "needle"]\nsubprocess.Popen(argv)',
            'import subprocess\ndef invoke(argv):\n subprocess.run(argv)\ninvoke(["rg"])',
            'import subprocess\nsubprocess.run("rg needle", shell=True)',
            'import os\nos.system("rg needle")',
            'import subprocess\nsubprocess.run(["git"], executable="rg")',
            'import subprocess\nsubprocess.run(["xcrun", "rg"])',
        ]
        for source in sources:
            with self.subTest(source=source):
                found = commands.python_commands(source, 'fixture.py')
                self.assertIn('rg', {item.name for item in found if not item.dynamic})
        source = '''example='subprocess.run(["rg"])'\nprint(example)'''
        self.assertEqual([], commands.python_commands(source, 'fixture.py'))

    def test_swift_process_launch_forms(self):
        sources = [
            'process.executableURL = URL(fileURLWithPath: "/opt/homebrew/bin/rg")',
            'process.launchPath = "/opt/homebrew/bin/rg"',
            'let child = Process.launchedProcess(launchPath: "/opt/homebrew/bin/rg", arguments: [])',
        ]
        for source in sources:
            with self.subTest(source=source):
                found = commands.swift_commands(source, 'fixture.swift')
                self.assertEqual(['/opt/homebrew/bin/rg'], [item.name for item in found])
                self.assertFalse(commands.allowed(found[0], {'grep'}, {'swiftc'}))

    def test_only_the_declared_tool_categories_are_allowed_and_node_is_site_only(self):
        permitted = {'git', 'grep', 'swiftc', 'xcrun'} | commands.RUST | commands.INSTALLED | commands.RUNNER
        for name in permitted:
            with self.subTest(command=name):
                self.assertTrue(commands.allowed(commands.Command('justfile', 'check-fast', name),
                                                 {'git', 'grep'}, {'swiftc', 'xcrun'}))
        for name in ['rg', 'brew', 'shellcheck', 'jq', 'node', 'npm']:
            with self.subTest(command=name):
                self.assertFalse(commands.allowed(commands.Command('scripts/check.sh', 'script', name),
                                                  {'git', 'grep'}, {'swiftc', 'xcrun'}))
        for name in ('node', 'npm'):
            self.assertTrue(commands.allowed(commands.Command('site/build.sh', 'script', name), set(), set()))
            self.assertTrue(commands.allowed(commands.Command('justfile', 'site-build', name), set(), set()))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('node', 'npm'):
                tool = root / name
                tool.write_text('fixture')
                tool.chmod(0o700)
                self.assertFalse(commands.allowed(commands.Command('scripts/check.sh', 'script', str(tool)),
                                                  set(), set(), (root,)))
                self.assertTrue(commands.allowed(commands.Command('site/build.sh', 'script', str(tool)),
                                                 set(), set(), (root,)))

    def test_unknown_or_stale_dynamic_calls_cannot_disappear_from_the_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'test-support').mkdir()
            policy = root / 'test-support/ci-command-dynamic.json'
            policy.write_text('{"schema":1,"calls":[]}')
            call = commands.Command('fixture.sh', 'script', '$new_command', True)
            errors = commands.validate(root, [call], {'grep'}, set())
            self.assertIn('unreviewed_dynamic_command', errors[0])
            item = {'source': call.source, 'scope': call.scope, 'expression': call.name,
                    'reason': 'Private test binary.', 'dependencies': []}
            policy.write_text(json.dumps({'schema': 1, 'calls': [item]}))
            self.assertEqual([], commands.validate(root, [call], {'grep'}, set()))
            self.assertIn('stale_dynamic_review', commands.validate(root, [], {'grep'}, set())[0])
            item['dependencies'] = ['rg']
            policy.write_text(json.dumps({'schema': 1, 'calls': [item]}))
            with self.assertRaises(ValueError):
                commands.validate(root, [call], {'grep'}, set())

    def test_ci_must_supply_the_non_system_build_tools_and_pin(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.github/workflows').mkdir(parents=True)
            (root / 'rust-toolchain').write_text('1.96.0\n')
            workflow = root / '.github/workflows/ci.yml'
            workflow.write_text('  run: nice -n 19 python3 scripts/rust_toolchain.py cargo install just cbindgen --locked\n')
            commands.check_ci_setup(root)
            workflow.write_text('  run: nice -n 19 python3 scripts/rust_toolchain.py cargo install cbindgen@0.29.4 just@1.59.0 --locked\n')
            commands.check_ci_setup(root)
            workflow.write_text('  run: echo cargo install just cbindgen\n')
            with self.assertRaises(ValueError):
                commands.check_ci_setup(root)
            workflow.write_text('  run: cargo install just --locked\n')
            with self.assertRaises(ValueError):
                commands.check_ci_setup(root)
            workflow.write_text('  run: cargo install cbindgen@0.29.4 --locked # just@1.59.0\n')
            with self.assertRaises(ValueError):
                commands.check_ci_setup(root)

    def test_cli_rejects_the_previous_scan_dependency_without_executing_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old = root / 'justfile'
            source = (ROOT / 'justfile').read_text()
            self.assertIn('grep -r -n -E -I "$forbidden_apis"', source)
            old.write_text(source.replace('grep -r -n -E -I "$forbidden_apis"',
                                          'rg --line-number "$forbidden_apis"', 1))
            failed = subprocess.run([sys.executable, str(CHECKER), '--justfile', str(old)],
                                    capture_output=True, text=True)
            self.assertEqual(1, failed.returncode, failed.stderr)
            self.assertIn('rg', {item.get('unavailable_command') for item in json.loads(failed.stdout)['errors']})
            passed = subprocess.run([sys.executable, str(CHECKER)], capture_output=True, text=True)
            self.assertEqual(0, passed.returncode, passed.stderr + passed.stdout)


if __name__ == '__main__':
    unittest.main()
