#!/usr/bin/env python3
"""Require one missing Swift test factory; confine inference errors to its call."""

import json
from pathlib import Path
import re
import sys


EXPECTED = "type 'CommitStreamRecorder' has no member 'makeProcessRecorderForTesting'"
# `--model-download` checks the model-download factory instead. The self-test
# also proves one factory's error can never stand in for the other's.
MODEL_DOWNLOAD_EXPECTED = "type 'ModelDownloadStore' has no member 'forTesting'"
FIXTURES = {
    EXPECTED: "RecorderFactoryVisibility.swift",
    MODEL_DOWNLOAD_EXPECTED: "ModelDownloadFactoryVisibility.swift",
}
MISSING_MEMBER = re.compile(r"type '([^']+)' has no member '([^']+)'")
# Classify lost type context, independently of the factory or argument names.
# A conversion/type mismatch is not an inference diagnostic.
LOST_CONTEXT = re.compile(
    r"'[^']+' requires a contextual type"
    r"|cannot infer contextual base in reference to member '[^']+'"
    r"|cannot infer type of closure parameter '[^']+' without a type annotation"
    r"|generic parameter '[^']+' could not be inferred"
    r"|type of expression is ambiguous without a type annotation"
)
# These controlled fixtures use ordinary strings and comments. Keeping them
# whole prevents parentheses or factory names inside them from widening a call.
SWIFT_TOKEN = re.compile(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|[A-Za-z_]\w*|\S', re.DOTALL)
ERROR = re.compile(r"^(?:(.*?):(\d+)(?::(\d+))?: )?error: (.+)$")
RECIPE_ERROR = re.compile(r"^error: recipe `_swift-test-compile` failed with exit code \d+$")
SOURCE_LINE = re.compile(r"^\s*(?:\d+\s*)?\|")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def compiler_diagnostics(log):
    diagnostics = []
    for raw in log.splitlines():
        line = ANSI.sub("", raw)
        # Swift repeats a diagnostic beside its source excerpt. The primary
        # location header is the diagnostic; the gutter copy is not a second one.
        if SOURCE_LINE.match(line) or RECIPE_ERROR.fullmatch(line):
            continue
        match = ERROR.fullmatch(line)
        if match:
            path, row, column, message = match.groups()
            location = (path, int(row), int(column) if column else None) if path else None
            diagnostics.append((location, message))
        elif "error:" in line:
            diagnostics.append((None, line))
    return diagnostics


def compiler_errors(log):
    return [message for _, message in compiler_diagnostics(log)]


def fixture_call_span(source, expected):
    member = MISSING_MEMBER.fullmatch(expected)
    if member is None:
        return None
    base, name = member.groups()
    tokens = [token for token in SWIFT_TOKEN.finditer(source)
              if not token.group().startswith(("//", "/*"))]
    starts = [i for i in range(len(tokens) - 3)
              if [token.group() for token in tokens[i:i + 4]] == [base, ".", name, "("]]
    if len(starts) != 1:
        return None
    start = starts[0]
    depth = 0
    for token in tokens[start + 3:]:
        if token.group() == "(":
            depth += 1
        elif token.group() == ")":
            depth -= 1
            if depth == 0:
                return tokens[start].start(), token.end()
    return None


def source_offset(source, location):
    if location is None or location[2] is None:
        return None
    _, row, column = location
    lines = source.splitlines(keepends=True)
    if not (1 <= row <= len(lines)) or column < 1:
        return None
    # Swift diagnostics count UTF-8 bytes, whereas the span uses Python string
    # offsets. Reject invalid byte boundaries rather than guessing a position.
    line = lines[row - 1].rstrip("\r\n").encode("utf-8")
    if column > len(line):
        return None
    try:
        prefix = line[:column - 1].decode("utf-8")
    except UnicodeDecodeError:
        return None
    return sum(len(line) for line in lines[:row - 1]) + len(prefix)


def accepted(log, expected=EXPECTED, *, source=None, source_path=None):
    diagnostics = compiler_diagnostics(log)
    primary = [location for location, message in diagnostics if message == expected]
    if len(primary) != 1:
        return False
    secondary = [(location, message) for location, message in diagnostics if message != expected]
    if not secondary:
        return True
    origin = primary[0]
    if origin is None:
        return False
    if source is None:
        filename = FIXTURES.get(expected)
        if filename is None:
            return False
        source_path = Path(__file__).resolve().parent / filename
        try:
            source = source_path.read_text()
        except (OSError, UnicodeError):
            return False
    if source_path is None or Path(origin[0]).resolve() != Path(source_path).resolve():
        return False
    span = fixture_call_span(source, expected)
    if span is None:
        return False

    def in_call(location):
        if location is None or location[0] != origin[0]:
            return False
        offset = source_offset(source, location)
        return offset is not None and span[0] <= offset < span[1]

    return (in_call(origin)
            and all(LOST_CONTEXT.fullmatch(message) and in_call(location)
                    for location, message in secondary))


def self_test():
    expected = "fixture.swift:15:45: error: " + EXPECTED + "\n"
    excerpt = "15 | let recorder = fixture()\n   | `- error: " + EXPECTED + "\n"
    wrapper = "error: recipe `_swift-test-compile` failed with exit code 1\n"
    unrelated = "fixture.swift:12:30: error: cannot convert value of type 'String' to specified type 'Int'\n"
    # Require one missing-member diagnostic. The gutter copy and just's
    # wrapper line are not extra errors. Rejected:
    # none, unrelated only, mixed, duplicated, unlocated and fatal errors.
    cases = [
        ("expected only", expected, True),
        ("source gutter and recipe result", expected + excerpt + wrapper, True),
        ("no compiler errors", wrapper, False),
        ("unrelated only", unrelated + wrapper, False),
        ("mixed errors", unrelated + expected + excerpt + wrapper, False),
        ("duplicate expected errors", expected + expected, False),
        ("unlocated compiler error", expected + "error: failed to load module\n", False),
        ("fatal compiler error", expected + "fatal error: unavailable\n", False),
    ]
    for name, log, wanted in cases:
        if accepted(log) != wanted:
            raise AssertionError(name)
        model_log = log.replace(EXPECTED, MODEL_DOWNLOAD_EXPECTED)
        if accepted(model_log, MODEL_DOWNLOAD_EXPECTED) != wanted:
            raise AssertionError("model download: " + name)
    if accepted(expected, MODEL_DOWNLOAD_EXPECTED):
        raise AssertionError("another factory's error must not prove this one absent")
    nil_error = "'nil' requires a contextual type"
    member_error = "cannot infer contextual base in reference to member 'main'"
    count = len(cases) * 2 + 1
    for target, filename in FIXTURES.items():
        path = Path(__file__).resolve().parent / filename
        source = path.read_text()
        base, member = MISSING_MEMBER.fullmatch(target).groups()

        def diagnostic(offset, message, filename=str(path)):
            prefix = source[:offset]
            row = prefix.count("\n") + 1
            column = len(prefix.rsplit("\n", 1)[-1].encode("utf-8")) + 1
            return f"{filename}:{row}:{column}: error: {message}\n"

        start, end = fixture_call_span(source, target)
        primary = diagnostic(source.index(member + "("), target)
        argument = source.index(".main" if target == EXPECTED else "nil", start)
        cascade = diagnostic(argument, member_error if target == EXPECTED else nil_error)
        inside = diagnostic(argument, "cannot convert value of type 'String' to specified type 'Int'")
        cascade_cases = [
            ("same-call inference cascade", primary + cascade, True),
            ("cascade before primary", cascade + primary, True),
            ("primary at base", diagnostic(start, target) + cascade, True),
            ("multiple inference errors", primary + cascade + cascade
             + diagnostic(argument, "cannot infer type of closure parameter 'value' without a type annotation"), True),
            ("source gutter", primary + cascade + "   | `- error: " + nil_error + "\n" + wrapper, True),
            ("cascade without primary", cascade, False),
            ("duplicate primary", primary + primary + cascade, False),
            ("unrelated type error inside call", primary + cascade + inside, False),
            ("unrelated type error outside call", primary + cascade + unrelated, False),
            ("inference outside call", primary + diagnostic(end, nil_error), False),
            ("inference before call", primary + diagnostic(start - 1, nil_error), False),
            ("inference in another file", primary + diagnostic(argument, nil_error, "other.swift"), False),
            ("inference without location", primary + "error: " + nil_error + "\n", False),
            ("inference without column", primary + str(path) + ":1: error: " + nil_error + "\n", False),
            ("primary without location", "error: " + target + "\n" + cascade, False),
            ("primary outside call", diagnostic(0, target) + cascade, False),
        ]
        for name, log, wanted in cascade_cases:
            if accepted(log, target) != wanted:
                raise AssertionError(filename + ": " + name)
            count += 1

    # A multiline call with nested calls and a quoted closing parenthesis must
    # end at its matching delimiter, even with another expression on that line.
    source = ('let before = nil\nlet value = ModelDownloadStore.forTesting(\n'
              '    support: nil, bytes: Data(")".utf8), configuration: {\n'
              '        URLSessionConfiguration.ephemeral\n'
              '    }); let after = nil\n')
    target = MODEL_DOWNLOAD_EXPECTED
    start, end = fixture_call_span(source, target)
    primary = diagnostic(source.index("forTesting("), target, "fixture.swift")
    for marker, wanted in [("support: nil", True), ("ephemeral", True),
                           ("before = nil", False), ("after = nil", False)]:
        offset = source.index(marker) + len(marker) - 1
        log = primary + diagnostic(offset, nil_error, "fixture.swift")
        if accepted(log, target, source=source, source_path="fixture.swift") != wanted:
            raise AssertionError(marker)
        count += 1
    # Swift uses byte columns; an unrelated expression after a call on a
    # Japanese source line must not be mistaken for an argument.
    source = 'let 説明 = "文字"; let value = ModelDownloadStore.forTesting(support: nil); let after = nil\n'
    primary = diagnostic(source.index("forTesting("), target, "fixture.swift")
    for offset, wanted in [(source.index("nil"), True), (source.rindex("nil"), False)]:
        if accepted(primary + diagnostic(offset, nil_error, "fixture.swift"), target,
                    source=source, source_path="fixture.swift") != wanted:
            raise AssertionError("UTF-8 diagnostic column")
        count += 1
    print(f"Swift factory visibility diagnostic parser: {count} cases passed")


def main():
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return 0
    args = sys.argv[1:]
    expected = EXPECTED
    if args[:1] == ["--model-download"]:
        args = args[1:]
        expected = MODEL_DOWNLOAD_EXPECTED
    if len(args) != 1:
        print("usage: check-recorder-visibility-diagnostics.py [--model-download] <compiler-log>", file=sys.stderr)
        return 2
    try:
        log = Path(args[0]).read_text()
    except (OSError, UnicodeError) as error:
        print(f"cannot read compiler log: {error}", file=sys.stderr)
        return 2
    errors = compiler_errors(log)
    print(json.dumps({"compiler_errors": errors}, ensure_ascii=False))
    return 0 if accepted(log, expected) else 1


if __name__ == "__main__":
    raise SystemExit(main())
