#!/usr/bin/env python3
"""Compile and check the shipped ABI; all checks also run with Python -O."""
import argparse
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
EXPECTED = ["initialized", "status", "user_dictionary", "key", "convert", "commit",
            "learning", "learning_version", "malloc_usage"]
ITERATIONS = 5000
SAMPLES = ("first", "second")
ALLOCATION_HEADROOM = 128 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


# 4096 x 2 is the least that passes: 32 x 4096 x 2 = 262,144, exactly twice
# the headroom. Both `check` and `run` validate this first, so lowering
# ITERATIONS or dropping a sample fails before anything is built or run.
def validate_allocation_contract(iterations, sample_count):
    # A 29-byte output occupies 32 bytes in the controlled malloc zones. Keep
    # at least 2x headroom over the limit across all measured samples; changing
    # the iteration count or dropping a sample must not weaken this silently.
    require(32 * iterations * sample_count >= 2 * ALLOCATION_HEADROOM,
            "insufficient theoretical growth to detect the smallest buffer leak")


def check_consumer(consumer, dictionary):
    # Normal runs grew Nano's size_in_use even with unchanged learning bytes
    # and version, and even when commit was replaced by cancel. With Nano off,
    # both 5,000-call samples had zero growth; Nano's internal breakdown remains
    # unknown. Only this private consumer changes the allocator setting: this
    # is an owned-buffer leak check, not the product's default allocator setup.
    # With Nano on, the correct sequence grew by about 180 KiB over 10,000
    # calls (about 147 KiB with cancel), which is above the headroom, so the
    # check would fail on correct code. libmalloc reads the variable when a
    # process starts, so only this child is affected, not the checker.
    result = subprocess.run([str(consumer), dictionary], env=dict(os.environ, MallocNanoZone="0"),
                            capture_output=True, text=True, timeout=30)
    require(result.returncode == 0, f"consumer exited with {result.returncode}: {result.stderr}")
    observations = [json.loads(line) for line in result.stdout.splitlines()]
    # Exact names and order catch a missing, repeated or reordered
    # observation before any value is read.
    require([row["name"] for row in observations] == EXPECTED,
            "observation names, count or order differ")
    require(all(type(row["buffer_len"]) is int and row["buffer_len"] > 0 for row in observations[:-1]),
            "invalid returned buffer lengths")
    values = {row["name"]: row["value"] for row in observations}
    require(isinstance(values["initialized"], dict) and values["initialized"], "initialization report")
    require(values["initialized"]["degraded"]["user_dictionary_invalid"] is False,
            "supplied user dictionary was not accepted")
    require(values["status"] == {
        "schema_version": 2, "system_dictionary": "kana_only", "language_model": "unloaded",
        "commit_stream_enabled": False, "language_model_details": {
            "requested_model": None, "actual_model": None, "route": "disabled",
            "fallback_reason": None, "unavailable_reason": None,
        },
    }, "runtime status differs")
    # "missing" must report no rows; "minimal" must report the one supplied
    # row with all four columns, so the file was applied, not just accepted.
    view = values["user_dictionary"]
    require(view["schema_version"] == 1 and view["access"] == "editable"
            and view["layer_status"] == "active" and view["reason"] is None,
            "user dictionary view differs")
    entries = view["entries"]
    count = 1 if dictionary == "minimal" else 0
    require(view["entry_count"] == count and len(entries) == count, "user dictionary count differs")
    if count:
        require({key: entries[0][key] for key in ("reading", "surface", "part_of_speech", "priority")} == {
            "reading": "あび", "surface": "阿備", "part_of_speech": "NOUN", "priority": "normal",
        }, "synthetic user dictionary entry differs")
    require("".join(run["text"] for run in values["key"]["preedit"]) == "きょう", "preedit differs")
    require(values["key"]["caret_utf16"] == 3 and values["key"]["consumed"] is True, "caret differs")
    require(values["convert"]["consumed"] is True, "conversion was not consumed")
    require(values["commit"]["commit_text"] == "きょう" and values["commit"]["preedit"] == [],
            "committed view differs")
    require(values["learning"]["schema_version"] == 2, "learning schema differs")
    require(values["learning"]["dictionary_payload_sha256"] is None, "learning dictionary differs")
    require(isinstance(values["learning_version"], dict) and values["learning_version"],
            "missing learning version")
    usage = values["malloc_usage"]
    require(usage["iterations_per_sample"] == ITERATIONS, "allocation sample size differs")
    require(all(type(usage[key]) is int and usage[key] >= 0 for key in ("baseline", *SAMPLES)),
            "invalid allocation observations")
    # The controlled allocator stayed constant after warmup. The approved
    # headroom tolerates lazy resources while a repeated 29-byte leak exceeds it.
    # Leaking only that buffer reached +160,000 B in the first sample, just
    # over 131,072, and +320,000 B in the second. The larger sample gives the
    # margin, which is why both are compared.
    require(max(usage[key] for key in SAMPLES) <= usage["baseline"] + ALLOCATION_HEADROOM,
            f"buffer allocations did not plateau: {usage}; {result.stderr}")
    return observations


# -DNDEBUG is always on, so the consumer cannot rely on assert. ITERATIONS
# goes in as MUU_LEAK_ITERATIONS, so the C loops, their printed count and the
# margin guard all use the one constant. `include` can point at a mutated
# header copy for the negative below.
def compile_consumer(library, target, include, output, extra_flags=(), extra_objects=()):
    command = ["xcrun", "clang", "-std=c17", "-Wall", "-Wextra", "-Werror", "-DNDEBUG",
               f"-DMUU_LEAK_ITERATIONS={ITERATIONS}",
               "-target", target, "-I", str(include), *extra_flags,
               str(ROOT / "scripts/test-c-abi-consumer.c"), *map(str, extra_objects),
               str(library), "-framework", "Accelerate", "-o", str(output)]
    result = subprocess.run(command, capture_output=True, text=True, timeout=60)
    require(result.returncode == 0, f"C compilation failed: {result.stderr}")


def expect_rejected(consumer, reason, detail=None):
    # A fresh optimized Python process must reject the real C failure or fake
    # transport; running just the parent under -O would not test this boundary.
    # The reason text must appear, so an unrelated failure does not count.
    result = subprocess.run([sys.executable, str(Path(__file__).resolve()), "check",
                             str(consumer), "--dictionary", "missing"],
                            env=dict(os.environ, PYTHONOPTIMIZE="1"),
                            capture_output=True, text=True, timeout=30)
    require(result.returncode != 0, f"optimized checker accepted {consumer.name}")
    require(reason in result.stderr, f"wrong rejection for {consumer.name}: {result.stderr}")
    if detail is not None:
        require(detail in result.stderr, f"wrong injected leak count: {result.stderr}")


# A consumer whose muu_buffer_free calls go to a shim instead. The -D rename
# applies only to the consumer, where it also turns the header declaration
# into the shim's prototype. The shim is compiled without it, so the buffers
# it does not skip reach the real free. The rest of the ABI and the archive
# are the real ones. At exit it prints how many buffers it skipped, so a
# rejection can be tied to the leak actually injected.
def free_mutant(scratch, library, target, smallest=None):
    name = "leak-smallest" if smallest is not None else "leak-all"
    shim = scratch / f"{name}.c"
    condition = f"buffer.len == {smallest}" if smallest is not None else "1"
    shim.write_text('''#include "muu_ffi.h"
#include <stdio.h>
#include <stdlib.h>
static size_t leaked;
static void report(void) { fprintf(stderr, "deliberately_not_freed=%zu\\n", leaked); }
void consumer_leak_free(struct MuuBuffer buffer) {
    if (buffer.data != NULL && (''' + condition + ''')) {
        if (leaked == 0 && atexit(report) != 0) exit(91);
        ++leaked;
    } else {
        muu_buffer_free(buffer);
    }
}
''')
    object_file = scratch / f"{name}.o"
    result = subprocess.run(["xcrun", "clang", "-std=c17", "-Wall", "-Wextra", "-Werror",
                             "-target", target, "-I", str(ROOT / "include"),
                             "-c", str(shim), "-o", str(object_file)], capture_output=True, text=True, timeout=60)
    require(result.returncode == 0, f"free mutant compilation failed: {result.stderr}")
    consumer = scratch / name
    compile_consumer(library, target, ROOT / "include", consumer,
                     extra_flags=("-Dmuu_buffer_free=consumer_leak_free",), extra_objects=(object_file,))
    return consumer


def fake_consumer(path, observations, exit_code=0):
    text = "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in observations)
    path.write_text(f"#!{sys.executable}\nimport sys\nsys.stdout.write({text!r})\nsys.exit({exit_code})\n")
    path.chmod(0o755)
    return path


def run_checks(library, target):
    # The guard itself: the boundary passes, while one iteration fewer or the
    # current count with one sample (160,000 B, under 2x) must be refused.
    validate_allocation_contract(4096, 2)
    for iterations, samples in ((4095, 2), (ITERATIONS, 1)):
        try:
            validate_allocation_contract(iterations, samples)
        except RuntimeError:
            pass
        else:
            raise RuntimeError("allocation contract accepted insufficient theoretical growth")
    with tempfile.TemporaryDirectory(prefix="muu-c-abi-consumer.") as temporary:
        scratch = Path(temporary)
        normal = scratch / "consumer"
        compile_consumer(library, target, ROOT / "include", normal)
        missing = check_consumer(normal, "missing")
        check_consumer(normal, "minimal")
        print("C consumer with NDEBUG: missing and synthetic user dictionary passed")

        # CREATE_FAILED changed from 3 to 7 in a header copy: a real
        # header/library disagreement must fail even when the checker
        # runs with PYTHONOPTIMIZE=1 and the C caller has NDEBUG defined.
        include = scratch / "broken-header"
        include.mkdir()
        header = (ROOT / "include/muu_ffi.h").read_text()
        original = "#define MUU_RESULT_CREATE_FAILED 3"
        require(header.count(original) == 1, "header mutation anchor differs")
        (include / "muu_ffi.h").write_text(header.replace(original, "#define MUU_RESULT_CREATE_FAILED 7"))
        broken = scratch / "broken-constant"
        compile_consumer(library, target, include, broken)
        expect_rejected(broken, "before_initialization == MUU_RESULT_CREATE_FAILED")

        # Fake children replay the real observations with one fault each, so
        # the checker itself is tested: a nonzero exit despite valid output,
        # no output, a changed status, and a missing, repeated or swapped row.
        expect_rejected(fake_consumer(scratch / "failed-child", missing, 23), "consumer exited with 23")
        expect_rejected(fake_consumer(scratch / "empty-child", []), "observation names")
        invalid = copy.deepcopy(missing)
        invalid[1]["value"]["system_dictionary"] = "wrong"
        expect_rejected(fake_consumer(scratch / "wrong-status", invalid), "runtime status differs")
        for name, rows in (("missing-row", missing[:-1]), ("duplicate-row", missing + missing[:1]),
                           ("reordered-row", [missing[1], missing[0], *missing[2:]])):
            expect_rejected(fake_consumer(scratch / name, rows), "observation names")
        print("Optimized checker: header mismatch, failed/empty child and changed observations rejected")

        # The shim picks buffers by length, so the smallest must be unique
        # among the eight. Each sequence then leaks exactly one buffer: 1 run
        # with output, 32 warmup runs and two samples of ITERATIONS. The
        # leak-all mutant skips all eight per sequence.
        smallest = min(row["buffer_len"] for row in missing[:-1])
        require(sum(row["buffer_len"] == smallest for row in missing[:-1]) == 1,
                "smallest-output mutation must leak exactly one buffer per sequence")
        sequences = 1 + 32 + len(SAMPLES) * ITERATIONS
        for consumer, count in ((free_mutant(scratch, library, target), sequences * 8),
                                (free_mutant(scratch, library, target, smallest), sequences)):
            expect_rejected(consumer, "buffer allocations did not plateau", f"deliberately_not_freed={count}\n")
        print(f"Free regressions rejected: all eight outputs and the smallest single output ({smallest} bytes)")


def main():
    validate_allocation_contract(ITERATIONS, len(SAMPLES))
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check")
    check.add_argument("consumer", type=Path)
    check.add_argument("--dictionary", choices=("missing", "minimal"), required=True)
    run = commands.add_parser("run")
    run.add_argument("--library", type=Path, required=True)
    run.add_argument("--target", required=True)
    args = parser.parse_args()
    if args.command == "check":
        check_consumer(args.consumer.resolve(), args.dictionary)
        print("C ABI 13 consumer observations passed")
    else:
        run_checks(args.library.resolve(), args.target)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"C ABI consumer check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
