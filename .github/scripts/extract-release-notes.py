#!/usr/bin/env python3
"""Extract one dated changelog section and its single-line release tagline."""

import argparse
from datetime import date
from pathlib import Path
import re


def extract_release_notes(changelog: str, version: str) -> tuple[str, str]:
    # The version is compared as a literal string, so only x.y.z is accepted;
    # anything else could match the wrong heading or none at all.
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("expected a release version with three numbers")
    lines = changelog.splitlines(keepends=True)
    heading = re.compile(r"^## \[([^\]]+)\](.*)$")
    matches = []
    for index, line in enumerate(lines):
        match = heading.fullmatch(line.rstrip("\r\n"))
        if match and match[1] == version:
            matches.append((index, match[2]))
    if len(matches) != 1:
        raise ValueError("expected exactly one changelog section for the version")
    start, suffix = matches[0]
    release_date = re.fullmatch(r" - ([0-9]{4}-[0-9]{2}-[0-9]{2})", suffix)
    if not release_date:
        raise ValueError("release date must be YYYY-MM-DD")
    # A real calendar date, not just the shape: a typo should stop the release.
    date.fromisoformat(release_date[1])

    section = []
    for line in lines[start + 1:]:
        # Link definitions belong to the changelog footer, not release notes.
        if line.startswith("## ") or re.match(r"^[ \t]{0,3}\[[^\]]+\]:", line):
            break
        section.append(line)
    while section and not section[0].strip():
        section.pop(0)
    paragraph = []
    for line in section:
        if not line.strip():
            break
        paragraph.append(line)
    if not paragraph or not paragraph[0].startswith(">"):
        raise ValueError("release tagline is missing")
    if len(paragraph) != 1:
        raise ValueError("release tagline must occupy one line")
    tagline = paragraph[0][1:].strip()
    # The title is written as one `title=` line to the workflow output file. A
    # newline or other control character could end that line early and add
    # outputs the changelog author did not intend.
    if not tagline or any(ord(character) < 32 for character in tagline):
        raise ValueError("release tagline must be a nonempty single line")
    if not "".join(section[1:]).strip():
        raise ValueError("release body is empty")
    title = f"Muu IME {version} — {tagline}"
    return title, "".join(section).strip() + "\n"


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--changelog", type=Path,
                        default=Path(__file__).resolve().parents[2] / "CHANGELOG.md")
    parser.add_argument("--notes-output", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args(argv)
    # Everything is validated before either file is written, so a validation
    # failure leaves an existing notes file and workflow output unchanged.
    try:
        title, notes = extract_release_notes(args.changelog.read_text(encoding="utf-8"), args.version)
        args.notes_output.parent.mkdir(parents=True, exist_ok=True)
        args.notes_output.write_text(notes, encoding="utf-8")
        if args.github_output is not None:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"title={title}\n")
    except (OSError, ValueError) as error:
        parser.exit(1, f"release notes: {error}\n")
    print(title)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
