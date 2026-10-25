#!/usr/bin/env python3
"""Run the model fetcher with synthetic bytes and confined fake commands."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


PROFILES = {
    "zenz": ("zenz-v3.2-small", "Miwa-Keita/zenz-v3.2-small-gguf",
             "c67e03e07d215c869f591b274c1631170d3e11fe", {
                 "ggml-model-Q5_K_M.gguf": "29c223d4c23327b80fd13ebb5ab2555057a46317997d5da391584ffbef0db673",
             }),
    "edge": ("kanakanji-edge-33m", "yuuki14202028/kanakanji-edge-33m",
             "da588df2db17e5ab69316986823806ec5f911450", {
                 "model.safetensors": "038af5f10e8b289c647c5a2d0dccf2efcf3f05c954c4e561c126a550fc2f2e2f",
                 "tokenizer.json": "75318016885462c2f56a41deb19caa8eebf70c70fee466d8ce06a783d5df4c82",
             }),
}


def model_bytes(name):
    return ("synthetic model fixture: " + name).encode()


class Fetch:
    def __init__(self, profile, **faults):
        self.scratch = tempfile.TemporaryDirectory(prefix="muu-fetch-test-")
        self.root = Path(self.scratch.name).resolve()
        self.profile = profile
        self.home = self.root / "home"
        self.destination = self.root / "chosen destination"
        self.default_destination = self.home / "Library/Application Support/MuuIME/Models" / PROFILES[profile][0]
        self.events_file = self.root / "events.jsonl"
        self.events_file.write_text("")
        self.config_file = self.root / "config.json"
        self.config = {"root": str(self.root), **faults}
        self.save_config()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        launcher = self.bin / "fixture-tool"
        launcher.write_text(
            f"#!{sys.executable}\nimport os, sys\n"
            f"os.execv({sys.executable!r}, [{sys.executable!r}, '-I', {str(Path(__file__).resolve())!r}, "
            "'--fixture-command', os.path.basename(sys.argv[0]), *sys.argv[1:]])\n"
        )
        launcher.chmod(0o755)
        # PATH holds only these fakes and awk, so the script cannot reach the
        # network, the user's Models directory or any other tool.
        for name in ("curl", "shasum", "mkdir", "mv", "rm"):
            (self.bin / name).symlink_to(launcher)
        (self.bin / "awk").symlink_to("/usr/bin/awk")

    def save_config(self):
        self.config_file.write_text(json.dumps(self.config))

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.scratch.cleanup()

    def run(self, arguments=None):
        script = Path(__file__).with_name("fetch-conversion-lm.sh")
        if arguments is None:
            arguments = ["--model", self.profile, str(self.destination)]
        env = {"HOME": str(self.home), "PATH": str(self.bin), "LC_ALL": "C", "LANG": "C",
               "MUU_FETCH_FIXTURE_CONFIG": str(self.config_file), "MUU_FETCH_FIXTURE_EVENTS": str(self.events_file)}
        return subprocess.run(["/bin/bash", str(script), *arguments], env=env,
                              capture_output=True, text=True, timeout=30)

    @property
    def events(self):
        return [json.loads(row) for row in self.events_file.read_text().splitlines()]

    # A complete cache of good bytes, except that `invalid` gets bytes whose
    # digest misses the pin, so the script must refetch exactly that file.
    def seed_cache(self, invalid=None):
        self.destination.mkdir()
        for name in PROFILES[self.profile][3]:
            (self.destination / name).write_bytes(b"previous final bytes" if name == invalid else model_bytes(name))


def fixture_command(tool, args):
    config = json.loads(Path(os.environ["MUU_FETCH_FIXTURE_CONFIG"]).read_text())
    root = Path(config["root"])

    def confined(value):
        path = Path(value).resolve()
        if not path.is_relative_to(root):
            raise ValueError("fetch fixture command attempted to escape its private directory")
        return path

    with Path(os.environ["MUU_FETCH_FIXTURE_EVENTS"]).open("a") as stream:
        stream.write(json.dumps({"tool": tool, "args": args}) + "\n")
    if tool == "curl":
        assert args[:4] == ["--fail", "--location", "--silent", "--show-error"]
        assert args[5:6] == ["--output"] and len(args) == 7
        target = confined(args[6])
        name = target.name.removesuffix(".partial")
        urls = {f"https://huggingface.co/{repo}/resolve/{revision}/{file}"
                for _, repo, revision, files in PROFILES.values() for file in files if file == name}
        assert args[4] in urls, args[4]
        # Fault points: "download" writes truncated bytes to the partial and
        # exits 22 like curl --fail; "hash" exits 0 with bytes whose digest
        # misses the pin. Both only ever touch the .partial path.
        failure = config.get("failure") if config.get("file") == name else None
        target.write_bytes(b"incomplete download" if failure else model_bytes(name))
        return 22 if failure == "download" else 0
    if tool == "shasum":
        assert args[:2] == ["-a", "256"] and len(args) == 3
        target = confined(args[2])
        name = target.name.removesuffix(".partial")
        pins = {name: pin for _, _, _, files in PROFILES.values() for name, pin in files.items()}
        data = target.read_bytes()
        # Real model bytes are intentionally not inputs. Synthetic good bytes
        # stand for the pinned object; changed bytes have a different digest.
        digest = pins[name] if data == model_bytes(name) else hashlib.sha256(data).hexdigest()
        print(digest + "  " + str(target))
        return 0
    if tool == "mkdir":
        assert args[:1] == ["-p"] and len(args) == 2
        confined(args[1]).mkdir(parents=True, exist_ok=True)
        return 0
    if tool == "mv":
        assert len(args) == 2
        confined(args[0]).replace(confined(args[1]))
        return 0
    if tool == "rm":
        assert args[:1] == ["-f"] and len(args) == 2
        confined(args[1]).unlink(missing_ok=True)
        return 0
    return 90


class FetchTests(unittest.TestCase):
    def assert_complete(self, fixture, destination):
        for name in PROFILES[fixture.profile][3]:
            self.assertEqual((destination / name).read_bytes(), model_bytes(name))
            self.assertFalse((destination / (name + ".partial")).exists())

    # Each pinned file is fetched from its pinned revision URL into a .partial,
    # checked once, and only then renamed to its final name, for both models.
    def test_both_profiles_fetch_only_the_reviewed_revision_and_names(self):
        for profile in PROFILES:
            with self.subTest(profile=profile), Fetch(profile) as fixture:
                result = fixture.run()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(fixture.destination))
                self.assert_complete(fixture, fixture.destination)
                _, repo, revision, files = PROFILES[profile]
                self.assertEqual([e["args"][4] for e in fixture.events if e["tool"] == "curl"],
                                 [f"https://huggingface.co/{repo}/resolve/{revision}/{name}" for name in files])
                for name in files:
                    partial = str(fixture.destination / (name + ".partial"))
                    checks = [i for i, e in enumerate(fixture.events)
                              if e["tool"] == "shasum" and e["args"][-1] == partial]
                    moves = [i for i, e in enumerate(fixture.events)
                             if e["tool"] == "mv" and e["args"][0] == partial]
                    self.assertEqual(len(checks), 1)
                    self.assertEqual(len(moves), 1)
                    self.assertLess(checks[0], moves[0])

    # Invocation compatibility: no arguments fetches zenz to its default
    # directory; `--model edge` uses edge's default; the legacy single
    # destination argument still fetches edge there.
    def test_default_and_legacy_destinations_are_preserved(self):
        for profile, invocation in (("zenz", "default"), ("edge", "explicit"), ("edge", "legacy")):
            with self.subTest(invocation=invocation), Fetch(profile) as fixture:
                args = [] if invocation == "default" else ["--model", profile]
                destination = fixture.default_destination
                if invocation == "legacy":
                    args, destination = [str(fixture.destination)], fixture.destination
                result = fixture.run(args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(destination))
                self.assert_complete(fixture, destination)

    # Main contract, case 1: a complete, valid cache is left alone. No fetch,
    # move or delete happens, and every file keeps its bytes.
    def test_valid_cache_does_not_download_or_replace_any_file(self):
        for profile in PROFILES:
            with self.subTest(profile=profile), Fetch(profile) as fixture:
                fixture.seed_cache()
                self.assertEqual(fixture.run().returncode, 0)
                self.assert_complete(fixture, fixture.destination)
                self.assertFalse(any(e["tool"] in ("curl", "mv", "rm") for e in fixture.events))

    # Main contract, case 2. One file per run is stale, so it is refetched, and
    # that fetch fails, by download error or checksum mismatch. The stale final
    # bytes and every other good file must survive unchanged, nothing may be
    # renamed into place, and the exit status reports the failure. A
    # mismatched partial is removed; a failed download's partial is left.
    def test_download_and_hash_failures_preserve_previous_final_bytes(self):
        for profile, spec in PROFILES.items():
            for name in spec[3]:
                for failure, expected_status in (("download", 22), ("hash", 1)):
                    with self.subTest(profile=profile, name=name, failure=failure), \
                            Fetch(profile, file=name, failure=failure) as fixture:
                        fixture.seed_cache(invalid=name)
                        before = {p.name: p.read_bytes() for p in fixture.destination.iterdir()}
                        result = fixture.run()
                        self.assertEqual(result.returncode, expected_status, result.stderr)
                        for file, data in before.items():
                            self.assertEqual((fixture.destination / file).read_bytes(), data)
                        self.assertFalse(any(e["tool"] == "mv" for e in fixture.events))
                        self.assertEqual((fixture.destination / (name + ".partial")).exists(), failure == "download")
                        if failure == "hash":
                            self.assertIn("checksum mismatch", result.stderr)

    # Leftover from a failed download: the next run must overwrite that
    # partial, verify it and only then install it, with no partial remaining.
    def test_a_later_retry_replaces_a_partial_download_after_verification(self):
        for profile, spec in PROFILES.items():
            with self.subTest(profile=profile), Fetch(profile, file=next(iter(spec[3])), failure="download") as fixture:
                self.assertEqual(fixture.run().returncode, 22)
                fixture.config["failure"] = None
                fixture.save_config()
                result = fixture.run()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_complete(fixture, fixture.destination)

    # Only the one stale file is fetched (one curl, one mv); the good files
    # in the same cache are not downloaded again.
    def test_bad_cache_is_replaced_only_with_verified_bytes(self):
        for profile, spec in PROFILES.items():
            with self.subTest(profile=profile), Fetch(profile) as fixture:
                fixture.seed_cache(invalid=next(iter(spec[3])))
                result = fixture.run()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_complete(fixture, fixture.destination)
                self.assertEqual(sum(e["tool"] == "curl" for e in fixture.events), 1)
                self.assertEqual(sum(e["tool"] == "mv" for e in fixture.events), 1)

    # Negatives: a missing model name, an unknown model, too many arguments.
    # Each exits 2 before any tool runs and creates neither HOME nor storage.
    def test_invalid_arguments_do_not_create_storage(self):
        for arguments in (["--model"], ["--model", "unknown"], ["--model", "edge", "one", "two"]):
            with self.subTest(arguments=arguments), Fetch("zenz") as fixture:
                self.assertEqual(fixture.run(arguments).returncode, 2)
                self.assertEqual(fixture.events, [])
                self.assertFalse(fixture.home.exists())
                self.assertFalse(fixture.destination.exists())


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fixture-command"]:
        raise SystemExit(fixture_command(sys.argv[2], sys.argv[3:]))
    unittest.main()
