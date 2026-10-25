#!/usr/bin/env python3
"""Exercise generator selection through the real recipes with isolated tools."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
HEADER = "synthetic header\n"


def fake_generator(path: Path, label: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"#!{sys.executable}\n"
        "import os, sys\nfrom pathlib import Path\n"
        f"with Path(os.environ['GENERATOR_EVENTS']).open('a') as f: f.write({label!r} + '\\n')\n"
        "args = sys.argv[1:]\n"
        f"Path(args[args.index('--output') + 1]).write_text({HEADER!r})\n"
    )
    path.chmod(0o700)


class GeneratorSelection(unittest.TestCase):
    # Each mode installs fakes that log their own label, and the expected
    # label is the only one that may run: `explicit` (CBINDGEN set, with a
    # PATH and a Cargo fake also present), `path` (PATH beats Cargo's bin),
    # `cargo_home` (no PATH fake). The negatives, `missing_explicit` and
    # `empty_explicit`, set invalid CBINDGEN values and must fail with no
    # generator run at all, not quietly use PATH. The real justfile is copied
    # into a scratch root, so both recipes are tested as written.
    def test_override_and_fallbacks_for_both_recipes(self):
        just = shutil.which("just")
        if just is None:
            self.fail("just is required")
        for recipe in ("header", "header-check"):
            for mode in ("explicit", "path", "cargo_home", "missing_explicit", "empty_explicit"):
                with self.subTest(recipe=recipe, mode=mode), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    shutil.copyfile(REPO / "justfile", root / "justfile")
                    (root / "include").mkdir()
                    (root / "include/muu_ffi.h").write_text(HEADER)
                    tools = root / "tools"
                    tools.mkdir()
                    # Supply only the recipe's standard utilities. A real
                    # cbindgen anywhere on the host PATH cannot affect this test.
                    for name in ("bash", "mkdir", "mktemp", "rm", "diff"):
                        executable = shutil.which(name)
                        if executable is None:
                            self.fail(f"{name} is required")
                        (tools / name).symlink_to(executable)
                    cargo_home = root / "cargo"
                    fake_generator(cargo_home / "bin/cbindgen", "cargo_home")
                    if mode != "cargo_home":
                        fake_generator(tools / "cbindgen", "path")
                    explicit = root / "explicit tool/cbindgen"
                    fake_generator(explicit, "explicit")
                    events = root / "events"
                    env = dict(os.environ, PATH=str(tools), CARGO_HOME=str(cargo_home),
                               GENERATOR_EVENTS=str(events))
                    env.pop("CBINDGEN", None)
                    if mode == "explicit":
                        env["CBINDGEN"] = str(explicit)
                    elif mode == "missing_explicit":
                        env["CBINDGEN"] = str(root / "missing")
                    elif mode == "empty_explicit":
                        env["CBINDGEN"] = ""
                    result = subprocess.run([just, recipe], cwd=root, env=env,
                                            capture_output=True, text=True, timeout=30)
                    observed = events.read_text().splitlines() if events.exists() else []
                    if mode in ("missing_explicit", "empty_explicit"):
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("cbindgen is required", result.stderr)
                        self.assertEqual(observed, [])
                    else:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertEqual(observed, [mode])
                        self.assertEqual((root / "include/muu_ffi.h").read_text(), HEADER)


if __name__ == "__main__":
    unittest.main()
