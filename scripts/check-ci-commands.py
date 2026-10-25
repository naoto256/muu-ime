#!/usr/bin/env python3
"""Check executable dependencies against the macOS CI tool set."""

import argparse
import ast
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
STANDARD = (Path('/usr/bin'), Path('/bin'), Path('/usr/sbin'), Path('/sbin'))
RUST = {'rustup', 'cargo', 'rustc', 'rustdoc', 'rustfmt', 'cargo-fmt', 'cargo-clippy', 'clippy-driver'}
INSTALLED = {'just', 'cbindgen'}
RUNNER = {'python3'}
SITE = {'node', 'npm'}
BUILTINS = set('! : [ [[ ]] alias break builtin cd command continue declare echo eval exec exit '
               'export false getopts hash local printf pwd read readonly return set shift source '
               'test times trap true type ulimit umask unalias unset wait'.split())
LAUNCHERS = {'run', 'Popen', 'check_output', 'check_call', 'execv', 'execve', 'execvp', 'execvpe', 'system', 'popen'}


@dataclass(frozen=True, order=True)
class Command:
    source: str
    scope: str
    name: str
    dynamic: bool = False


def call_name(node):
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return node.attr
    return ''


def strip_shell_comments(source):
    result = []
    quote = None
    escaped = False
    i = 0
    while i < len(source):
        character = source[i]
        if escaped:
            escaped = False
        elif character == '\\' and quote != "'":
            escaped = True
        elif character in ('"', "'"):
            if quote == character:
                quote = None
            elif quote is None:
                quote = character
        elif character == '#' and quote is None and (i == 0 or source[i - 1].isspace()):
            while i < len(source) and source[i] != '\n':
                i += 1
            continue
        result.append(character)
        i += 1
    return ''.join(result)


def mask_substitutions(source):
    values = {}
    bodies = []

    def end_of_call(start):
        depth = 1
        quote = None
        i = start
        while i < len(source):
            character = source[i]
            if character == '\\' and quote != "'":
                i += 2
                continue
            if quote == "'":
                if character == "'":
                    quote = None
            elif source.startswith('$(', i):
                i = end_of_call(i + 2) + 1
                continue
            elif character in ('"', "'"):
                if quote == character:
                    quote = None
                elif quote is None:
                    quote = character
            elif quote is None:
                if character == '(':
                    depth += 1
                elif character == ')':
                    depth -= 1
                    if depth == 0:
                        return i
            i += 1
        raise ValueError('unterminated command substitution')

    result = []
    quote = None
    i = 0
    while i < len(source):
        character = source[i]
        if character == '\\' and quote != "'" and i + 1 < len(source):
            result.append(source[i:i + 2])
            i += 2
            continue
        if quote != "'" and (source.startswith('$(', i) or character == '`'):
            start = i
            if character == '`':
                i += 1
                body_start = i
                while i < len(source) and source[i] != '`':
                    i += 2 if source[i] == '\\' else 1
                if i == len(source):
                    raise ValueError('unterminated backtick command')
            else:
                body_start = i + 2
                i = end_of_call(body_start)
            key = f'__muu_substitution_{len(values)}__'
            bodies.append(source[body_start:i])
            values[key] = source[start:i + 1]
            result.append(key)
            i += 1
            continue
        if character in ('"', "'"):
            if quote == character:
                quote = None
            elif quote is None:
                quote = character
        result.append(character)
        i += 1
    return ''.join(result), values, bodies


def python_commands(source, path, scope='module'):
    tree = ast.parse(source, filename=path)
    bindings = {}
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    bindings.setdefault(target.id, []).append(node.value)
                elif (isinstance(target, ast.Tuple) and target.elts
                      and isinstance(target.elts[0], ast.Name)
                      and isinstance(node.value, ast.Call)
                      and call_name(node.value.func) == 'tool_command' and node.value.args):
                    bindings.setdefault(target.elts[0].id, []).append(node.value.args[0])
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name) and node.value:
            bindings.setdefault(node.target.id, []).append(node.value)
    for node in ast.walk(tree):
        if isinstance(node, ast.For) and isinstance(node.target, ast.Name):
            values = bindings.get(node.iter.id, []) if isinstance(node.iter, ast.Name) else [node.iter]
            for value in values:
                if isinstance(value, (ast.List, ast.Tuple)):
                    bindings.setdefault(node.target.id, []).extend(value.elts)

    def heads(node, seen=frozenset()):
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return {node.value}
        if isinstance(node, (ast.List, ast.Tuple)) and node.elts:
            return heads(node.elts[0], seen)
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            if isinstance(node.left, ast.Constant) and isinstance(node.left.value, str):
                if isinstance(node.right, ast.Constant) and isinstance(node.right.value, str):
                    return {node.left.value + node.right.value}
                return set()
            return heads(node.left, seen)
        if isinstance(node, ast.Name) and node.id in bindings and node.id not in seen:
            return set().union(*(heads(value, seen | {node.id}) for value in bindings[node.id]))
        if isinstance(node, ast.Attribute) and ast.unparse(node) == 'sys.executable':
            return {'python3'}
        if isinstance(node, ast.Call) and call_name(node.func) == 'which' and node.args:
            return heads(node.args[0], seen)
        if isinstance(node, ast.Name) and node.id == 'just':
            return {'just'}
        return set()

    # Local argv-forwarding helpers have the same dependency boundary as
    # subprocess.run. Examine their callers as well as their implementation.
    launchers = {alias.asname or alias.name for node in ast.walk(tree)
                 if isinstance(node, ast.ImportFrom) and node.module == 'subprocess'
                 for alias in node.names if alias.name in LAUNCHERS}
    subprocess_names = {alias.asname or alias.name for node in ast.walk(tree)
                        if isinstance(node, ast.Import) for alias in node.names
                        if alias.name == 'subprocess'}

    def is_launcher(function):
        if isinstance(function, ast.Name):
            return function.id in launchers
        return (isinstance(function, ast.Attribute) and isinstance(function.value, ast.Name)
                and function.value.id in subprocess_names | {'os'} and function.attr in LAUNCHERS)

    for _ in range(len(list(ast.walk(tree)))):
        previous = set(launchers)
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            parameters = {arg.arg for arg in node.args.args}
            if any(isinstance(child, ast.Call) and is_launcher(child.func)
                   and child.args and isinstance(child.args[0], ast.Name)
                   and child.args[0].id in parameters for child in ast.walk(node)):
                launchers.add(node.name)
        if launchers == previous:
            break

    result = []

    def visit(node, owner):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            owner = node.name
        if isinstance(node, ast.Call) and is_launcher(node.func) and node.args:
            argument = node.args[0]
            executable = next((item.value for item in node.keywords if item.arg == 'executable'), None)
            if executable is not None:
                argument = executable
            names = heads(argument)
            if names:
                for name in names:
                    if call_name(node.func) in ('system', 'popen') or any(item.arg == 'shell' and isinstance(item.value, ast.Constant)
                           and item.value.value is True for item in node.keywords):
                        result.extend(shell_commands(name, path, owner))
                    else:
                        result.append(Command(path, owner, name))
                if (isinstance(argument, (ast.List, ast.Tuple)) and argument.elts
                        and next(iter(names)).rsplit('/', 1)[-1] in ('env', 'nice', 'xcrun', 'python3', 'bash', 'sh')):
                    prefix = []
                    for item in argument.elts:
                        if not isinstance(item, ast.Constant) or not isinstance(item.value, str):
                            break
                        prefix.append(item.value)
                    if len(prefix) > 1:
                        result.extend(shell_commands(shlex.join(prefix), path, owner))
            else:
                result.append(Command(path, owner, ast.unparse(argument), True))
        for child in ast.iter_child_nodes(node):
            visit(child, owner)

    visit(tree, scope)
    return result


def shell_commands(source, path, scope='script', inherited_functions=frozenset()):
    functions = set(inherited_functions)
    # A here-document is data unless it is input to an interpreter. In
    # particular, generated fake tools must not become host dependencies.
    lines = source.splitlines()
    kept = []
    embedded = []
    i = 0
    while i < len(lines):
        line = lines[i]
        match = re.search(r"<<-?\s*['\"]?([A-Za-z_]\w*)['\"]?", line)
        kept.append(line)
        i += 1
        if match:
            body = []
            while i < len(lines) and lines[i].strip() != match[1]:
                body.append(lines[i])
                i += 1
            if i == len(lines):
                raise ValueError(f'{path}: unterminated here-document')
            i += 1
            if re.search(r'\bpython3\b', line):
                embedded.extend(python_commands('\n'.join(body), path, scope + ':python'))
            elif re.match(r'\s*(?:/bin/)?(?:ba)?sh\b', line):
                embedded.extend(shell_commands('\n'.join(body), path, scope + ':shell'))
    source = strip_shell_comments('\n'.join(kept)).replace('\\\n', ' ')
    functions.update(re.findall(r'(?m)^\s*(?:function\s+)?([A-Za-z_]\w*)\s*\(\s*\)', source))
    source = re.sub(r'\$\(\([^()]*\)\)', '__muu_arithmetic__', source)
    source = re.sub(r'\(\([^()]*\)\)', ':', source)
    source, substitution_values, substitution_bodies = mask_substitutions(source)
    source = source.replace('\\(', '__muu_escaped_lparen__').replace('\\)', '__muu_escaped_rparen__')
    quoted_operators = {}
    def operator_literal(match):
        key = f'__muu_quoted_operator_{len(quoted_operators)}__'
        quoted_operators[key] = match[2]
        return key
    source = re.sub(r'''(['"])([;&|()<>\n]+)\1''', operator_literal, source)
    parameters = {}
    while re.search(r'\$\{[^{}]*\}', source):
        def parameter(match):
            key = f'__muu_parameter_{len(parameters)}__'
            parameters[key] = match[0]
            return key
        source = re.sub(r'\$\{[^{}]*\}', parameter, source)
    lexer = shlex.shlex(source, posix=True, punctuation_chars=';&|()<>\n')
    lexer.whitespace = ' \t\r'
    lexer.whitespace_split = True
    lexer.commenters = ''
    tokens = []
    for token in lexer:
        if token and all(character in ';&|()<>\n' for character in token):
            tokens.extend(re.findall(r'\n|;;|&&|\|\||>&|<&|&>|<<|>>|[;&|()<>]', token))
        else:
            tokens.append(token)
    for key, value in reversed(parameters.items()):
        tokens = [token.replace(key, value) for token in tokens]
    for key, value in substitution_values.items():
        tokens = [token.replace(key, value) for token in tokens]
    result = list(embedded)
    for text in substitution_bodies:
        result.extend(shell_commands(text, path, scope + ':substitution', functions))
    command_start = True
    control = None
    cases = []
    array_depth = 0
    skip_option_value = False
    wrapper = None
    conditional = None
    i = 0
    while i < len(tokens):
        token = tokens[i]
        i += 1
        if conditional:
            if token == conditional:
                conditional = None
            continue
        if token in ('[', '[[') and command_start:
            conditional = ']' if token == '[' else ']]'
            command_start = False
            continue
        if token in ('\n', ';', '&&', '||', '|', '&', ';&'):
            command_start = True
            wrapper = None
            continue
        if token in ('<', '>', '>>', '<<', '>&', '<&', '&>'):
            i += 1
            continue
        if token.isdecimal() and i < len(tokens) and tokens[i] in ('<', '>', '>>', '>&', '<&'):
            continue
        if token == ';;':
            if cases:
                cases[-1] = 'pattern'
            command_start = True
            continue
        if token == 'esac':
            if cases:
                cases.pop()
            command_start = False
            continue
        if control:
            if token == control:
                if control == 'in':
                    cases.append('pattern')
                control = None
                command_start = True
            continue
        if cases and cases[-1] == 'pattern':
            if token == ')':
                cases[-1] = 'body'
                command_start = True
            continue
        if token in quoted_operators:
            if command_start and quoted_operators[token] not in BUILTINS:
                result.append(Command(path, scope, quoted_operators[token]))
            command_start = False
            continue
        if token in ('for', 'select') and command_start:
            control = 'do'
            continue
        if token == 'case' and command_start:
            control = 'in'
            continue
        if token == '(':
            if i >= 2 and tokens[i - 2].endswith('='):
                array_depth = 1
            elif array_depth:
                array_depth += 1
            else:
                command_start = True
            continue
        if token == ')':
            if array_depth:
                array_depth -= 1
            command_start = False
            continue
        if array_depth:
            continue
        if token in ('if', 'then', 'elif', 'else', 'while', 'until', 'do', '{', '!') and command_start:
            command_start = True
            continue
        if token in ('fi', 'done', '}'):
            command_start = False
            continue
        if not command_start:
            continue
        if re.match(r'^[A-Za-z_]\w*(?:\[[^]]*\])?\+?=', token):
            continue
        if wrapper:
            if skip_option_value:
                skip_option_value = False
                continue
            if token.startswith('-'):
                skip_option_value = token in ('-n', '-u', '--unset', '--sdk', '--toolchain')
                continue
        if token in functions and tokens[i:i + 2] == ['(', ')']:
            i += 2
            continue
        if token in ('exec', 'command', 'builtin'):
            wrapper = token
            continue
        if token == 'trap' and i < len(tokens):
            if tokens[i] != '-':
                result.extend(shell_commands(tokens[i], path, scope + ':trap', functions))
            command_start = False
            continue
        if token not in BUILTINS and token not in functions:
            result.append(Command(path, scope, token, '$' in token))
        basename = token.rsplit('/', 1)[-1]
        if basename in ('env', 'nice', 'xcrun', 'time'):
            wrapper = basename
            continue
        if basename in ('python3', 'sh', 'bash') and i + 1 < len(tokens) and tokens[i] == '-c':
            text = tokens[i + 1]
            if basename == 'python3':
                calls = python_commands(text, path, scope + ':python')
                result.extend(calls)
                if any(call.dynamic and call.name == 'sys.argv[1:]' for call in calls) and i + 2 < len(tokens):
                    executable = tokens[i + 2]
                    result.append(Command(path, scope + ':python-argv', executable, '$' in executable))
            else:
                result.extend(shell_commands(text, path, scope + ':shell'))
        command_start = False
    return result


def swift_commands(source, path):
    result = []
    for match in re.finditer(r'(?m)^\s*\w+\.(executableURL|launchPath)\s*=\s*(.+)$', source):
        expression = match[2].strip()
        pattern = r'URL\(fileURLWithPath:\s*"([^"\n]+)"\)' if match[1] == 'executableURL' else r'"([^"\n]+)"'
        literal = re.fullmatch(pattern, expression)
        result.append(Command(path, 'Process.executableURL', literal[1] if literal else expression, literal is None))
    for match in re.finditer(r'Process\.launchedProcess\(\s*launchPath:\s*"([^"\n]+)"', source):
        result.append(Command(path, 'Process.launchedProcess', match[1]))
    return result


def just_commands(repository, justfile):
    dump = json.loads(subprocess.check_output(
        ['just', '--justfile', str(justfile), '--dump', '--dump-format', 'json'],
        cwd=repository, text=True))

    def render(fragment):
        if isinstance(fragment, str):
            return fragment
        if fragment == [['call', 'just_executable']]:
            return 'just'
        return '${DYNAMIC}'

    result = []
    for name, recipe in dump['recipes'].items():
        body = '\n'.join(''.join(render(part) for part in line) for line in recipe['body'])
        result.extend(shell_commands(body, 'justfile', name))
    return result


def inventory(repository, justfile=None):
    result = just_commands(repository, justfile or repository / 'justfile')
    paths = sorted((repository / 'scripts').rglob('*')) + [repository / 'install.sh']
    for file in paths:
        if not file.is_file() or file.suffix not in ('.py', '.sh', '.swift'):
            continue
        relative = file.relative_to(repository).as_posix()
        source = file.read_text()
        if source.startswith('#!'):
            result.extend(shell_commands(source.splitlines()[0][2:], relative, 'shebang'))
        if file.suffix == '.py':
            result.extend(python_commands(source, relative))
        elif file.suffix == '.sh':
            result.extend(shell_commands(source, relative))
        else:
            result.extend(swift_commands(source, relative))
    return sorted(set(result))


def executable_names(directories):
    return {entry.name for directory in directories if directory.is_dir()
            for entry in directory.iterdir() if entry.is_file() and os.access(entry, os.X_OK)}


def xcode_directories():
    developer = Path(subprocess.check_output(['/usr/bin/xcode-select', '-p'], text=True).strip())
    return (developer / 'usr/bin', *sorted((developer / 'Toolchains').glob('*.xctoolchain/usr/bin')))


def site_context(command):
    return (command.source.startswith('site/')
            or (command.source == 'justfile' and command.scope.split(':')[0].startswith('site-')))


def allowed(command, standard, xcode, xcode_roots=()):
    name = command.name
    if Path(name).name in SITE and not site_context(command):
        return False
    if name in SITE:
        return True
    if name in standard | xcode | RUST | INSTALLED | RUNNER:
        return True
    path = Path(name)
    if path.is_absolute():
        return ((path.parent in STANDARD or path.parent in xcode_roots)
                and path.is_file() and os.access(path, os.X_OK))
    return False


def validate(repository, commands, standard, xcode, xcode_roots=()):
    manifest = json.loads((repository / 'test-support/ci-command-dynamic.json').read_text())
    if manifest.get('schema') != 1:
        raise ValueError('unsupported dynamic command review schema')
    reviewed = {}
    for item in manifest['calls']:
        key = (item['source'], item['scope'], item['expression'])
        if key in reviewed or not item['reason'].strip():
            raise ValueError('duplicate or undocumented dynamic command review')
        if any(not allowed(Command(item['source'], item['scope'], name), standard, xcode, xcode_roots)
               for name in item.get('dependencies', [])):
            raise ValueError('dynamic review cannot add an executable to the allowed tool set')
        reviewed[key] = item
    seen = set()
    errors = []
    for command in commands:
        if command.dynamic:
            key = (command.source, command.scope, command.name)
            if key not in reviewed:
                errors.append({'source': command.source, 'scope': command.scope,
                               'unreviewed_dynamic_command': command.name})
            else:
                seen.add(key)
            continue
        path = repository / command.name
        if (command.name.startswith(('scripts/', './scripts/'))
                and path.is_file() and path.suffix in ('.py', '.sh', '.swift')):
            continue
        if not allowed(command, standard, xcode, xcode_roots):
            errors.append({'source': command.source, 'scope': command.scope,
                           'unavailable_command': command.name})
    for source, scope, expression in sorted(reviewed.keys() - seen):
        errors.append({'source': source, 'scope': scope, 'stale_dynamic_review': expression})
    return errors


def check_ci_setup(repository):
    if (repository / 'rust-toolchain').read_text().strip() != '1.96.0':
        raise ValueError('review the allowed Rust tool set when changing its pin')
    workflow = (repository / '.github/workflows/ci.yml').read_text()
    installed = set()
    for line in workflow.splitlines():
        match = re.match(r'\s+run:\s+(.+)', line)
        if not match or match[1] in ('|', '>'):
            continue
        tokens = shlex.split(strip_shell_comments(match[1]))
        if tokens[:2] == ['nice', '-n'] and len(tokens) > 2:
            tokens = tokens[3:]
        if tokens[:2] == ['python3', 'scripts/rust_toolchain.py']:
            tokens = tokens[2:]
        if tokens[:2] == ['cargo', 'install']:
            installed.update(token.split('@', 1)[0] for token in tokens[2:] if not token.startswith('-'))
    if not INSTALLED <= installed:
        raise ValueError('CI must install just and cbindgen before the gates')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repository', type=Path, default=ROOT)
    parser.add_argument('--justfile', type=Path)
    parser.add_argument('--inventory', action='store_true')
    args = parser.parse_args()
    try:
        repository = args.repository.resolve()
        check_ci_setup(repository)
        commands = inventory(repository, args.justfile)
        roots = xcode_directories()
        standard = executable_names(STANDARD)
        xcode = executable_names(roots)
        errors = validate(repository, commands, standard, xcode, roots)
        result = {'commands': len(commands), 'reviewed_dynamic_calls': sum(c.dynamic for c in commands),
                  'errors': errors}
        if args.inventory:
            result['inventory'] = [command.__dict__ for command in commands]
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 1 if errors else 0
    except (OSError, ValueError, SyntaxError, subprocess.CalledProcessError) as error:
        print(f'CI command check failed: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
