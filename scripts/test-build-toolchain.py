#!/usr/bin/env python3
"""Exercise real build recipes with hostile PATH tools and a fake toolchain."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
JUST = Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else None

# One script installed under every tool name. It logs each call with the
# toolchain variables it saw, then answers by its own name. `rustup which`
# resolves into "pinned tools", or gives a bad answer chosen by FIXTURE_FAULT.
FAKE_TOOL = r"""
import json, os, subprocess, sys
from pathlib import Path
name=Path(sys.argv[0]).name
args=sys.argv[1:]
root=Path(os.environ["FIXTURE_ROOT"])
event={"tool":name,"args":args,"RUSTC":os.environ.get("RUSTC"),
       "RUSTDOC":os.environ.get("RUSTDOC"),
       "RUSTUP_TOOLCHAIN":os.environ.get("RUSTUP_TOOLCHAIN"),
       "source":os.environ.get("MUU_RUST_SOURCE_COMMIT")}
with (root/"calls.jsonl").open("a") as log: log.write(json.dumps(event)+"\n")
version=(root/"rust-toolchain").read_text().strip()
if name=="rustup":
    if args==["target","add","--toolchain",version,"aarch64-apple-darwin"]:
        pass
    else:
        assert args[:3]==["which","--toolchain",version], args
        tool=root/"pinned tools"/args[3]
        fault=os.environ.get("FIXTURE_FAULT")
        if fault=="relative": print("relative/"+args[3])
        elif fault=="missing" or (fault=="missing_plugin" and args[3].startswith("cargo-")):
            print(root/"missing")
        elif fault=="mixed" and args[3]=="rustdoc": print(root/"other tools"/"rustdoc")
        else: print(tool)
elif name=="git":
    if args==["rev-parse","HEAD"]: print("1"*40)
    elif args==["rev-list","--count","HEAD"]: print("2")
    elif args==["rev-parse","--is-shallow-repository"]:
        print(os.environ.get("FIXTURE_SHALLOW","false"))
    else: raise SystemExit(90)
elif name=="lipo":
    print("arm64")
elif name=="rust_std_licenses.py":
    if args != ["--check","--archive","build/libmuu_ffi.a"]:
        raise SystemExit(92)
elif name=="xcodebuild":
    raise SystemExit(85) # Stop after the real history preflight; never run Xcode.
elif name=="compiled-fixture":
    names=["unit","asset","scale"]
    if "--list" in args:
        for test in names if "--ignored" not in args else names[1:]: print(test+": test")
    else:
        selected=[test for test in names if test in args]
        print("test result: ok. "+str(len(selected))+" passed; 0 failed;")
elif Path(sys.argv[0]).parent.name=="poison":
    raise SystemExit(97)
elif name=="cargo":
    if args==["--version"]: print("cargo "+version)
    elif args[0]=="clippy":
        subprocess.run(["cargo-clippy",*args[1:]],check=True)
    elif args[0]=="fmt":
        subprocess.run(["cargo-fmt",*args[1:]],check=True)
    elif args[0]=="metadata":
        print(json.dumps({"packages":[{"id":"fixture-id","name":"fixture-package"}]}))
    elif args[0]=="test" and "--no-run" in args:
        print(json.dumps({"reason":"compiler-artifact","package_id":"fixture-id",
              "executable":str(root/"compiled-fixture"),"profile":{"test":True},
              "target":{"kind":["lib"],"name":"fixture-package"}}))
    else:
        subprocess.run([os.environ["RUSTC"],"--version"],check=True)
        subprocess.run([os.environ["RUSTDOC"],"--version"],check=True)
        if args[0]=="build":
            output=Path(os.environ["CARGO_TARGET_DIR"])/"aarch64-apple-darwin/release/libmuu_ffi.a"
            output.parent.mkdir(parents=True,exist_ok=True)
            output.write_bytes(b"fixture archive")
        if args[0]=="test": print("test result: ok. 1 passed; 0 failed;")
        if args[0]=="run" and "--output" in args:
            output=Path(args[args.index("--output")+1])
            output.parent.mkdir(parents=True,exist_ok=True)
            output.write_bytes(b"fixture dictionary")
elif name in ("rustc","rustdoc"):
    print(name+" "+version+" (fixture)")
elif name=="cargo-clippy":
    print("clippy 0.1.96 (fixture)")
elif name=="cargo-fmt":
    subprocess.run([os.environ["RUSTFMT"],"--version"],check=True)
elif name=="rustfmt":
    print("rustfmt (fixture)")
else: raise SystemExit(91)
"""


class BuildToolchainTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="muu-build-tools-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name) / "fixture with spaces"
        (self.root / "scripts").mkdir(parents=True)
        for name in ("justfile", "rust-toolchain"):
            shutil.copyfile(ROOT / name, self.root / name)
        for name in ("rust_toolchain.py", "run-named-rust-test.py", "run-rust-test-group.py"):
            shutil.copyfile(ROOT / "scripts" / name, self.root / "scripts" / name)
        # xcode-ci checks standard-library licenses before calling Xcode.
        # This fixture checks ordering and arguments; the license gates check real bytes.
        self.tool(self.root / "scripts/rust_std_licenses.py")
        (self.root / "test-support").mkdir()
        row = {"package": "fixture-package", "target": "lib", "name": "asset", "requires": ["dictionary"]}
        (self.root / "test-support/rust-test-groups.json").write_text(json.dumps({
            "format_version": 1, "assets": [row], "scale": [{**row, "name": "scale", "requires": []}],
        }))
        self.tool(self.root / "compiled-fixture")
        # "poison" leads PATH. Its Rust tools exit 97, so a recipe that falls
        # back to a bare `cargo`/`rustc` fails instead of running. rustup, git,
        # lipo and xcodebuild also live there, but as working fakes: the
        # recipes are meant to find those through PATH. The root contains
        # spaces to catch an unquoted path in a recipe.
        for directory in ("pinned tools", "other tools", "poison"):
            (self.root / directory).mkdir()
            for name in ("cargo", "rustc", "rustdoc", "cargo-clippy", "cargo-fmt", "rustfmt"):
                self.tool(self.root / directory / name)
        for name in ("rustup", "git", "lipo", "xcodebuild"):
            self.tool(self.root / "poison" / name)
        for name in ("model.safetensors", "tokenizer.json", "ggml-model-Q5_K_M.gguf"):
            (self.root / name).touch()
        self.environment = os.environ | {
            "FIXTURE_ROOT": str(self.root),
            "PATH": str(self.root / "poison") + os.pathsep + os.environ.get("PATH", ""),
            "CARGO_TARGET_DIR": str(self.root / "cargo target"),
            # Must be overridden by the pin, never inherited.
            "RUSTUP_TOOLCHAIN": "unrelated-ambient-default",
            "MUU_LM_MODEL_DIR": str(self.root),
            "MUU_ZENZ_MODEL_DIR": str(self.root),
            "MUU_REAL_DICT_PATH": str(self.root / "dictionary"),
            "PYTHONDONTWRITEBYTECODE": "1",
        }

    @staticmethod
    def tool(path):
        path.write_text("#!" + sys.executable + "\n" + FAKE_TOOL, encoding="utf-8")
        path.chmod(0o755)

    def invoke(self, recipe, *arguments, **environment):
        return subprocess.run(
            [str(JUST), "--justfile", str(self.root / "justfile"),
             "--working-directory", str(self.root), "--no-deps", recipe, *arguments],
            env=self.environment | environment, capture_output=True, text=True,
        )

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    # Contract: the real recipes reach only the pinned tool directory, for
    # cargo, its clippy/fmt plugins, RUSTC/RUSTDOC and rustfmt, and pass the
    # source commit. A bare cargo, or a plugin left to PATH, hits "poison".
    def test_real_recipes_use_absolute_tools_and_pinned_subcommands(self):
        for recipe in ("rust-targets", "rust-arm64", "fmt-check", "clippy", "rust-fast-test",
                       "rust-assets-test", "rust-scale-test", "public-conversion-consumer", "dict-build"):
            with self.subTest(recipe=recipe):
                result = self.invoke(recipe)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls()
        for name in ("cargo", "rustc", "rustdoc", "cargo-clippy", "cargo-fmt", "rustfmt"):
            self.assertTrue(any(call["tool"] == name for call in calls), name)
        for call in calls:
            if call["tool"] in ("cargo", "rustc", "rustdoc", "cargo-clippy", "cargo-fmt"):
                self.assertEqual(call["RUSTC"], str(self.root / "pinned tools/rustc"))
                self.assertEqual(call["RUSTDOC"], str(self.root / "pinned tools/rustdoc"))
                self.assertEqual(call["RUSTUP_TOOLCHAIN"], (self.root / "rust-toolchain").read_text().strip())
        self.assertEqual((self.root / "build/libmuu_ffi.a").read_bytes(), b"fixture archive")
        self.assertTrue(any(c["tool"] == "cargo" and c["source"] == "1" * 40 for c in calls))

    # The counted wrapper, run directly rather than through a recipe, also
    # resolves cargo through the pin.
    def test_named_gate_uses_the_same_pinned_cargo(self):
        result = subprocess.run(
            [sys.executable, str(self.root / "scripts/run-named-rust-test.py"), "1", "fixture"],
            cwd=self.root, env=self.environment, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(any(c["tool"] == "cargo" and c["args"] == ["test", "fixture"] for c in self.calls()))

    # A relative, missing or mixed-toolchain path, or a missing plugin, must
    # stop resolution itself: no cargo call at all, not a later fallback.
    def test_invalid_or_mixed_tool_paths_fail_before_cargo(self):
        for fault in ("relative", "missing", "mixed", "missing_plugin"):
            result = self.invoke("clippy", FIXTURE_FAULT=fault)
            self.assertNotEqual(result.returncode, 0, fault)
        self.assertFalse(any(c["tool"] == "cargo" for c in self.calls()))

    # Each variant reaches Xcode with its own plist value, Swift condition and
    # DerivedData, and `settings` repeats exactly the build's arguments, so the
    # release check inspects what was built. An unknown variant never reaches
    # Xcode.
    def test_explicit_build_variants_use_separate_artifacts_and_the_same_settings(self):
        for recipe, variant, directory, condition in (
                ("xcode-ci", "standard", "build/xcode", ""),
                ("xcode-dogfood", "dogfood", "build/xcode-dogfood", "MUU_DOGFOOD")):
            before = len(self.calls())
            result = self.invoke(recipe, FIXTURE_SHALLOW="false")
            self.assertNotEqual(result.returncode, 0)  # The fake stops at Xcode.
            calls = [c for c in self.calls()[before:] if c["tool"] == "xcodebuild"]
            self.assertEqual(len(calls), 1, result.stdout + result.stderr)
            args = calls[0]["args"]
            self.assertIn("MUU_BUILD_VARIANT=" + variant, args)
            self.assertIn("SWIFT_ACTIVE_COMPILATION_CONDITIONS=" + condition, args)
            self.assertEqual(args[args.index("-derivedDataPath") + 1], directory)
            self.assertEqual(args[-1], "build")
            before = len(self.calls())
            result = self.invoke("_xcode-release", "settings", variant)
            self.assertNotEqual(result.returncode, 0)
            settings = [c for c in self.calls()[before:] if c["tool"] == "xcodebuild"]
            self.assertEqual(len(settings), 1, result.stdout + result.stderr)
            self.assertEqual(settings[0]["args"][:-2], args[:-1])
            self.assertEqual(settings[0]["args"][-2:], ["-showBuildSettings", "-json"])
        before = len(self.calls())
        self.assertNotEqual(self.invoke("_xcode-release", "build", "unapproved").returncode, 0)
        self.assertFalse(any(c["tool"] == "xcodebuild" for c in self.calls()[before:]))

    # A shallow clone cannot give the commit count used as the build number,
    # so xcode-ci must refuse before Xcode runs.
    def test_shallow_build_stops_before_xcode(self):
        result = self.invoke("xcode-ci", FIXTURE_SHALLOW="true")
        self.assertNotEqual(result.returncode, 0)
        tools = [call["tool"] for call in self.calls()]
        self.assertNotIn("rust_std_licenses.py", tools)
        self.assertNotIn("xcodebuild", tools)

    # Positive control for the shallow test: with full history the same
    # recipe does reach the (stopping) xcodebuild, so the refusal above is
    # caused by the shallow check and not by some earlier failure.
    def test_complete_history_reaches_xcode(self):
        result = self.invoke("xcode-ci", FIXTURE_SHALLOW="false")
        self.assertNotEqual(result.returncode, 0)  # The fixture stops at Xcode.
        calls = self.calls()
        stdlib_checks = [call for call in calls if call["tool"] == "rust_std_licenses.py"]
        self.assertEqual(len(stdlib_checks), 1, result.stdout + result.stderr)
        self.assertEqual(stdlib_checks[0]["args"], ["--check", "--archive", "build/libmuu_ffi.a"])
        tools = [call["tool"] for call in calls]
        self.assertIn("xcodebuild", tools, result.stdout + result.stderr)
        self.assertLess(tools.index("rust_std_licenses.py"), tools.index("xcodebuild"))


if __name__ == "__main__":
    if JUST is None or not JUST.is_file():
        raise SystemExit("usage: test-build-toolchain.py <just-executable>")
    unittest.main(argv=[sys.argv[0]], verbosity=2)
