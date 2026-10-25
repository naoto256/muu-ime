#!/usr/bin/env python3
"""Shared process cases for the controller tests and their comparison traces."""

import argparse


# Each case has its own Core startup configuration. About and the scripted
# adapter have assertions of their own but do not produce the native trace.
# The flag is "writes a native trace"; the last field is the asset group:
# "fast" cases start without a system dictionary, "assets" cases need the
# public one. The controller recipes run the cases of their group (or all),
# and the trace recipe and comparator read only flagged ones, so adding a
# case here reaches all of them; a list kept elsewhere is what drifted before.
# A restart pair (learning-restart-first/second) must stay in one group and
# in this order, because the second reads what the first saved.
CASES = (
    ("scripted-session", False, "fast"),
    ("controller", True, "fast"),
    ("commit-recovery", False, "fast"),
    ("about-status", False, "fast"),
    ("lifecycle", True, "fast"),
    ("closing-input", True, "fast"),
    ("core-invalid", True, "fast"),
    ("personal-frequency-store", True, "fast"),
    ("learning-corrupt", True, "assets"),
    ("learning-restart-first", True, "assets"),
    ("learning-restart-second", True, "assets"),
    ("learning-no-file", True, "assets"),
    ("personal-frequency-mismatch", True, "assets"),
    ("native-frequency-empty", True, "assets"),
    ("native-frequency-nonempty", True, "assets"),
    ("native-frequency-legacy", True, "assets"),
    ("generation-baseline-learning", True, "assets"),
    ("generation-baseline-user", True, "assets"),
    ("dictionary-only-recording", True, "assets"),
    ("commit-recovery-native", False, "assets"),
    ("commit-stream-delivery", True, "assets"),
    ("imported-completion-internal", True, "assets"),
    ("imported-completion-busy", True, "assets"),
)


def case_names(trace=False, group="all"):
    if group not in ("all", "fast", "assets"):
        raise ValueError("unknown input-path asset group")
    names = tuple(name for name, recorded, asset_group in CASES
                  if (not trace or recorded) and (group == "all" or asset_group == group))
    if not names or len(names) != len(set(names)):
        raise ValueError("input-path cases must be nonempty and unique")
    return names


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace", action="store_true")
    parser.add_argument("--group", choices=("all", "fast", "assets"), default="all")
    args = parser.parse_args()
    print("\n".join(case_names(trace=args.trace, group=args.group)))


if __name__ == "__main__":
    main()
