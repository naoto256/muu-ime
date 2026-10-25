#!/usr/bin/env python3
"""Generate Muu's pinned SudachiDict-core lexical supplement.

The product build consumes the generated TSV, not the upstream archives.  This
tool is the auditable re-import path: it verifies the exact official archives,
maps selected noun classes onto pinned Mozc context IDs, and removes entries
already supplied by the pinned Mozc dictionary.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import os
from pathlib import Path
import re
import tempfile
import unicodedata
import zipfile


SOURCE_VERSION = "20260723"
SMALL_SHA256 = "b578ac9545899783d5d7e30d5d78d5d9dcf40b36965d4af0663ec2eb041c1093"
CORE_SHA256 = "a2b39e1572adab08a649b1390b134517adc55f1d733c59358b113298788bf31c"
# Context ids of the pinned Mozc id.def. A new Mozc pin can renumber id.def,
# so re-check these against it before regenerating.
GENERAL_NOUN_CONTEXT_ID = 1851  # 名詞,一般,*,*,*,*,*
PROPER_OTHER_CONTEXT_ID = 1920  # 名詞,固有名詞,一般,*,*,*,*
PERSON_GENERAL_CONTEXT_ID = 1921  # 名詞,固有名詞,人名,一般,*,*,*
PERSON_GIVEN_CONTEXT_ID = 1922  # 名詞,固有名詞,人名,名,*,*,*
PERSON_FAMILY_CONTEXT_ID = 1923  # 名詞,固有名詞,人名,姓,*,*,*
PLACE_GENERAL_CONTEXT_ID = 1924  # 名詞,固有名詞,地域,一般,*,*,*
PLACE_COUNTRY_CONTEXT_ID = 1928  # 名詞,固有名詞,地域,国,*,*,*
WORD_COST_BASE = 6_500
MAX_ENTRIES = 1_000_000
MAX_SURFACES_PER_READING = 4_096
MAX_READING_BYTES = 512
MAX_SURFACE_BYTES = 4 * 1024
UNICODE_ESCAPE = re.compile(r"\\u([0-9a-fA-F]{4})")


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--small", required=True, type=Path)
    parser.add_argument("--core", required=True, type=Path)
    parser.add_argument("--mozc-source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_archive(path: Path, expected: str) -> None:
    actual = sha256(path)
    if actual != expected:
        raise ValueError(f"{path}: SHA-256 {actual}, expected {expected}")


def decode_sudachi_escapes(value: str) -> str:
    return UNICODE_ESCAPE.sub(lambda match: chr(int(match.group(1), 16)), value)


def katakana_to_hiragana(value: str) -> str:
    converted = []
    for character in value:
        if "ァ" <= character <= "ヶ":
            converted.append(chr(ord(character) - 0x60))
        else:
            converted.append(character)
    return "".join(converted)


def is_supported_reading(value: str) -> bool:
    return bool(value) and all("ぁ" <= character <= "ゖ" or character == "ー" for character in value)


def load_mozc_pairs(source: Path) -> set[tuple[str, str]]:
    pairs: set[tuple[str, str]] = set()
    for shard in range(10):
        path = source / f"dictionary{shard:02}.txt"
        with path.open(encoding="utf-8") as rows:
            for line_number, line in enumerate(rows, 1):
                columns = line.rstrip("\n").split("\t")
                if len(columns) not in (5, 6):
                    raise ValueError(f"{path}:{line_number}: expected 5 or 6 columns")
                pairs.add(
                    (
                        unicodedata.normalize("NFC", columns[0]),
                        unicodedata.normalize("NFC", columns[4]),
                    )
                )
    return pairs


def iter_archive_rows(path: Path, member: str):
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        if names != [member]:
            raise ValueError(f"{path}: expected only {member}, found {names!r}")
        with archive.open(member) as raw:
            text = io.TextIOWrapper(raw, encoding="utf-8", newline="")
            yield from enumerate(csv.reader(text), 1)


# Only common nouns and the listed proper-noun classes are imported; any other
# Sudachi POS returns None and the row is skipped.
def context_id_for_pos(columns: list[str]) -> int | None:
    pos = tuple(columns[5:9])
    if pos[:2] == ("名詞", "普通名詞"):
        return GENERAL_NOUN_CONTEXT_ID
    if pos[:3] == ("名詞", "固有名詞", "一般"):
        return PROPER_OTHER_CONTEXT_ID
    if pos[:3] == ("名詞", "固有名詞", "人名"):
        return {
            "一般": PERSON_GENERAL_CONTEXT_ID,
            "名": PERSON_GIVEN_CONTEXT_ID,
            "姓": PERSON_FAMILY_CONTEXT_ID,
        }.get(pos[3])
    if pos[:3] == ("名詞", "固有名詞", "地名"):
        return {
            "一般": PLACE_GENERAL_CONTEXT_ID,
            "国": PLACE_COUNTRY_CONTEXT_ID,
        }.get(pos[3])
    return None


def load_sudachi_entries(
    small: Path, core: Path
) -> dict[tuple[str, str, int], int]:
    entries: dict[tuple[str, str, int], int] = {}
    for path, member in ((small, "small_lex.csv"), (core, "core_lex.csv")):
        for line_number, columns in iter_archive_rows(path, member):
            if len(columns) != 19:
                raise ValueError(f"{path}:{line_number}: expected 19 columns, got {len(columns)}")
            context_id = context_id_for_pos(columns)
            if context_id is None:
                continue

            reading = unicodedata.normalize(
                "NFC", katakana_to_hiragana(decode_sudachi_escapes(columns[11]))
            )
            surface = unicodedata.normalize("NFC", decode_sudachi_escapes(columns[12]))
            # Skip rows that would only add noise: readings that are not plain
            # hiragana, readings under three characters, one-character
            # surfaces, surfaces equal to their reading, and anything over the
            # User dictionary's byte bounds.
            if not is_supported_reading(reading) or not surface or surface == "*":
                continue
            if len(reading) < 3 or len(surface) < 2:
                continue
            if reading == surface:
                continue
            if len(reading.encode("utf-8")) > MAX_READING_BYTES:
                continue
            if len(surface.encode("utf-8")) > MAX_SURFACE_BYTES:
                continue

            source_cost = int(columns[3])
            # Preserve Sudachi's relative frequency signal at a gentler scale.
            # The base is calibrated so a complete lexical entry can beat a
            # plausible but accidental multi-token decomposition.
            word_cost = WORD_COST_BASE + source_cost // 10
            key = (reading, surface, context_id)
            previous = entries.get(key)
            if previous is None or word_cost < previous:
                entries[key] = word_cost
    return entries


# The muu-dict-cli supplement format: `#` header lines, then rows in
# (reading, surface, context id) order with the same id on both sides. The
# per-reading bound matches the conversion candidate-surface limit.
def render(entries: dict[tuple[str, str, int], int]) -> bytes:
    counts: dict[str, int] = {}
    lines = [
        "# Muu SudachiDict-core lexical supplement v1",
        f"# source_version={SOURCE_VERSION}",
        f"# small_lex_sha256={SMALL_SHA256}",
        f"# core_lex_sha256={CORE_SHA256}",
        "# fields=reading,left_context_id,right_context_id,word_cost,surface",
    ]
    for (reading, surface, context_id), word_cost in sorted(entries.items()):
        counts[reading] = counts.get(reading, 0) + 1
        if counts[reading] > MAX_SURFACES_PER_READING:
            raise ValueError(f"reading {reading!r} exceeds surface bound")
        lines.append(
            f"{reading}\t{context_id}\t{context_id}\t{word_cost}\t{surface}"
        )
    if len(entries) > MAX_ENTRIES:
        raise ValueError(f"entry count {len(entries)} exceeds {MAX_ENTRIES}")
    return ("\n".join(lines) + "\n").encode("utf-8")


def publish_atomically(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def main() -> None:
    arguments = parse_arguments()
    verify_archive(arguments.small, SMALL_SHA256)
    verify_archive(arguments.core, CORE_SHA256)
    mozc_pairs = load_mozc_pairs(arguments.mozc_source)
    sudachi_entries = load_sudachi_entries(arguments.small, arguments.core)
    # Only pairs Mozc lacks are added; a pair Mozc has keeps Mozc's entry.
    additions = {
        key: cost
        for key, cost in sudachi_entries.items()
        if (key[0], key[1]) not in mozc_pairs
    }
    publish_atomically(arguments.output, render(additions))
    print(
        f"wrote {len(additions)} entries to {arguments.output} "
        f"sha256={sha256(arguments.output)}"
    )


if __name__ == "__main__":
    main()
