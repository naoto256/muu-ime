#!/usr/bin/env python3
"""Check changelog extraction without building assets or contacting a service."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "release_notes", ROOT / ".github/scripts/extract-release-notes.py"
)
notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notes)


def section(body="> A short tagline\n\nSelected release body.\n", date="2026-10-25"):
    return f"## [0.1.0] - {date}\n\n{body}"


class ReleaseNotesTests(unittest.TestCase):
    # Only the exact version's section is taken: Unreleased, a similar-looking
    # version, older releases and the link footer stay out of the notes.
    def test_selects_only_the_exact_version_between_other_sections(self):
        changelog = (
            "# Changelog\n\nPreamble.\n\n## [Unreleased]\n\nFuture changes.\n\n"
            "## [0.1.00] - 2026-10-26\n\n> Another release\n\nOther changes.\n\n"
            + section()
            + "\n## [0.0.9] - 2026-10-24\n\n> Older release\n\nOlder changes.\n\n"
            "[0.1.0]: https://example.com/releases/v0.1.0\n"
        )
        title, body = notes.extract_release_notes(changelog, "0.1.0")
        self.assertEqual("Muu IME 0.1.0 — A short tagline", title)
        self.assertEqual("> A short tagline\n\nSelected release body.\n", body)

    def test_last_section_omits_all_footer_definitions_and_their_continuations(self):
        changelog = section() + (
            "\n[Unreleased]: https://example.com/compare\n"
            "[0.1.0]: https://example.com/release\n"
            "  \"link title\"\n"
        )
        _, body = notes.extract_release_notes(changelog, "0.1.0")
        self.assertEqual("> A short tagline\n\nSelected release body.\n", body)

    def test_existing_calendar_date_including_leap_day_is_accepted(self):
        title, _ = notes.extract_release_notes(section(date="2024-02-29"), "0.1.0")
        self.assertEqual("Muu IME 0.1.0 — A short tagline", title)

    # Every malformed section, tagline or date exits 1 with a message and no
    # stdout, and leaves both previous files byte-for-byte as they were.
    def test_all_invalid_metadata_fails_before_changing_either_output(self):
        cases = {
            "missing section": "## [Unreleased]\n\nFuture changes.\n",
            "duplicate section": section() + "\n" + section(),
            "duplicate malformed section": section() + "\n## [0.1.0]\n",
            "missing tagline": section("Body without a tagline.\n"),
            "empty tagline": section("> \n\nBody.\n"),
            "multiline quote": section("> First line\n> Second line\n\nBody.\n"),
            "lazy quote continuation": section("> First line\nSecond line\n\nBody.\n"),
            "tagline control character": section("> First\x00line\n\nBody.\n"),
            "empty body": section("> Tagline\n\n \t\n"),
            "only footer after tagline": section("> Tagline\n\n[0.1.0]: https://example.com/release\n"),
            "missing date": "## [0.1.0]\n\n> Tagline\n\nBody.\n",
            "date without separators": section(date="20261025"),
            "short date": section(date="2026-1-2"),
            "nonexistent day": section(date="2026-02-30"),
            "nonleap day": section(date="2026-02-29"),
            "nonexistent month": section(date="2026-13-01"),
            "zero year": section(date="0000-01-01"),
            "date suffix": section(date="2026-10-25 extra"),
        }
        for label, changelog in cases.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source = root / "CHANGELOG.md"
                output = root / "notes.md"
                github = root / "github-output"
                source.write_text(changelog, encoding="utf-8")
                output.write_text("previous notes", encoding="utf-8")
                github.write_text("previous=value\n", encoding="utf-8")
                with redirect_stdout(io.StringIO()) as stdout, redirect_stderr(io.StringIO()) as stderr:
                    with self.assertRaises(SystemExit) as failed:
                        notes.main(["--version", "0.1.0", "--changelog", str(source),
                                    "--notes-output", str(output), "--github-output", str(github)])
                self.assertEqual(1, failed.exception.code)
                self.assertIn("release notes:", stderr.getvalue())
                self.assertEqual("", stdout.getvalue())
                self.assertEqual("previous notes", output.read_text(encoding="utf-8"))
                self.assertEqual("previous=value\n", github.read_text(encoding="utf-8"))

    def test_invalid_version_is_not_interpreted_as_a_heading_pattern(self):
        for version in ["v0.1.0", "0x1x0", "0.1", "0.1.0\n", "0.1.0-rc1"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                notes.extract_release_notes(section(), version)

    # Shell metacharacters in the tagline stay literal text in the title, and
    # the output file only gains one appended title line.
    def test_cli_writes_utf8_notes_and_a_literal_single_line_title_output(self):
        tagline = "文字を、打ちたいように打つ！ $(no-command); 'quoted'"
        body = f"> {tagline}\n\nMoji wo Uchitai youni Utsu!\n\n本文。\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "CHANGELOG.md"
            output = root / "build/notes.md"
            github = root / "github-output"
            source.write_text(section(body), encoding="utf-8")
            github.write_text("previous=value\n", encoding="utf-8")
            with redirect_stdout(io.StringIO()) as stdout:
                result = notes.main(["--version", "0.1.0", "--changelog", str(source),
                                     "--notes-output", str(output), "--github-output", str(github)])
            title = f"Muu IME 0.1.0 — {tagline}"
            self.assertEqual(0, result)
            self.assertEqual(title + "\n", stdout.getvalue())
            self.assertEqual(body.encode("utf-8"), output.read_bytes())
            self.assertEqual("previous=value\ntitle=" + title + "\n", github.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
