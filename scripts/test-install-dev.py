#!/usr/bin/env python3
"""Exercise the installer with a private HOME and no real OS integration tools."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


class Installation:
    def __init__(self, has_previous_app=True, variant="standard", **faults):
        self.variant = variant
        self.scratch = tempfile.TemporaryDirectory(prefix="muu-install-test-")
        self.root = Path(self.scratch.name).resolve()
        self.home = self.root / "home"
        self.target = self.home / "Library/Input Methods/MuuIME.app"
        self.source = self.root / "reviewed source/MuuIME.app"
        self.new_bytes, self.old_bytes = b"reviewed-new-executable", b"reviewed-old-executable"
        self.digest = hashlib.sha256(self.new_bytes).hexdigest()
        self.signature = {"identifier": "dev.muu.inputmethod.Muu", "flags": "runtime",
                          "cdHash": "c" * 40, "signatureIdentity": "fixture signer"}
        self.write_bundle(self.source, self.new_bytes, variant=variant)
        if has_previous_app:
            self.write_bundle(self.target, self.old_bytes)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.installer = scripts / "install-dev.sh"
        shutil.copyfile(Path(__file__).parent / "install-dev.sh", self.installer)
        self.sha_file = self.root / "reviewed.sha256"
        self.sha_file.write_text(self.digest + "\n")
        self.signature_file = self.root / "codesign.json"
        self.signature_file.write_text(json.dumps(self.signature, sort_keys=True) + "\n")
        self.events_file = self.root / "events.jsonl"
        self.events_file.write_text("")
        self.config = {"root": str(self.root), "source": str(self.source),
                       "target": str(self.target), "sha": self.digest,
                       "signature": self.signature, "variant": variant, **faults}
        self.config_file = self.root / "config.json"
        self.config_file.write_text(json.dumps(self.config))
        launcher = self.bin / "fixture-tool"
        launcher.write_text(
            f"#!{sys.executable}\nimport os, sys\n"
            f"os.execv({sys.executable!r}, [{sys.executable!r}, '-I', {str(Path(__file__).resolve())!r}, "
            "'--fixture-command', os.path.basename(sys.argv[0]), *sys.argv[1:]])\n"
        )
        launcher.chmod(0o755)
        for name in ("codesign", "ditto", "xcrun", "plutil", "mv", "rm", "sleep"):
            (self.bin / name).symlink_to(launcher)
        (scripts / "reload-ime.sh").symlink_to(launcher)
        # PATH contains only these safe file utilities and the fake OS tools.
        # Any other command the installer reaches (ps, defaults, killall, a
        # real reload) is simply not found, so it fails instead of running.
        for name, actual in {"bash": "/bin/bash", "dirname": "/usr/bin/dirname",
                             "awk": "/usr/bin/awk", "shasum": "/usr/bin/shasum",
                             "cmp": "/usr/bin/cmp", "cat": "/bin/cat",
                             "mkdir": "/bin/mkdir", "mktemp": "/usr/bin/mktemp"}.items():
            (self.bin / name).symlink_to(actual)
        (self.root / "tmp").mkdir()

    @staticmethod
    def write_bundle(app, executable, variant="standard"):
        (app / "Contents/MacOS").mkdir(parents=True)
        (app / "Contents/MacOS/MuuIME").write_bytes(executable)
        with (app / "Contents/Info.plist").open("wb") as stream:
            plistlib.dump({"CFBundleExecutable": "MuuIME", "CFBundleIdentifier": "dev.muu.inputmethod.Muu",
                          "CFBundleVersion": "42", "MuuSourceCommit": "a" * 40,
                          "MuuBuildVariant": variant}, stream)

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.scratch.cleanup()

    @property
    def environment(self):
        return {"HOME": str(self.home), "PATH": str(self.bin), "TMPDIR": str(self.root / "tmp"),
                "LC_ALL": "C", "LANG": "C", "MUU_INSTALL_FIXTURE_CONFIG": str(self.config_file),
                "MUU_INSTALL_FIXTURE_EVENTS": str(self.events_file)}

    def run(self, omit=None, **replace):
        arguments = {"--source-app": str(self.source), "--expected-commit": "a" * 40,
                     "--expected-build": "42", "--expected-variant": self.variant,
                     "--expected-executable-sha256-file": str(self.sha_file),
                     "--expected-codesign-json": str(self.signature_file)}
        arguments.update(replace)
        if omit:
            del arguments[omit]
        try:
            return subprocess.run(["/bin/bash", str(self.installer), *sum(([k, v] for k, v in arguments.items()), [])],
                                  env=self.environment, capture_output=True, text=True, timeout=30)
        except subprocess.TimeoutExpired as error:
            # Save before the fixture's TemporaryDirectory is removed, then
            # re-raise the same exception: the timeout stays a failure.
            self.preserve_timeout(error)
            raise

    def preserve_timeout(self, error):
        # This directory outlives TemporaryDirectory cleanup. Keep raw bytes:
        # a timeout can interrupt both a UTF-8 character and a fixture event.
        root = Path(os.environ.get("MUU_INSTALL_TEST_EVIDENCE_DIR",
                                   Path(__file__).resolve().parents[1] / "build/test-evidence/installer"))
        try:
            root.mkdir(parents=True, exist_ok=True)
            destination = Path(tempfile.mkdtemp(prefix="timeout-", dir=root))
            # TimeoutExpired carries bytes even with text=True; a str is
            # accepted too, and a stream that was never captured saves empty.
            for name, value in (("stdout.partial", error.stdout), ("stderr.partial", error.stderr)):
                data = value.encode("utf-8") if isinstance(value, str) else value or b""
                (destination / name).write_bytes(data)
            (destination / "events.jsonl").write_bytes(self.events_file.read_bytes())
            (destination / "timeout.json").write_text(json.dumps({
                "timeout_seconds": error.timeout, "command": error.cmd,
            }, sort_keys=True) + "\n")
            print(f"installer timeout evidence: {destination}", file=sys.stderr)
        except OSError as save_error:
            # An evidence write failure must not replace the original timeout.
            print(f"could not preserve installer timeout evidence: {save_error}", file=sys.stderr)

    @property
    def events(self):
        return [json.loads(line) for line in self.events_file.read_text().splitlines()]

    @property
    def target_bytes(self):
        return (self.target / "Contents/MacOS/MuuIME").read_bytes()

    @property
    def recoverable_previous_apps(self):
        return [app for app in self.root.rglob("previous-MuuIME.app")
                if (app / "Contents/MacOS/MuuIME").read_bytes() == self.old_bytes]


def fixture_command(tool, args):
    config = json.loads(Path(os.environ["MUU_INSTALL_FIXTURE_CONFIG"]).read_text())
    root = Path(config["root"])

    def confined(path):
        resolved = Path(path).resolve()
        if not resolved.is_relative_to(root):
            raise ValueError("fixture command attempted to escape its private directory")
        return resolved

    def phase(path):
        app = confined(path)
        if app == confined(config["source"]):
            return "source"
        if app == confined(config["target"]):
            return "installed"
        return "stage"

    def bundle_exists(path):
        app = confined(path)
        return (app.is_dir() and (app / "Contents/Info.plist").is_file()
                and (app / "Contents/MacOS/MuuIME").is_file())

    event = {"tool": tool, "args": args}
    if tool == "codesign":
        event["phase"] = phase(args[-1])
    elif tool == "xcrun":
        event["action"] = args[2]
        if Path(args[1]).name == "audit-installed-mode-icons.swift":
            event["phase"] = phase(args[3])
    elif tool == "mv":
        if Path(args[-1]).name == "previous-MuuIME.app":
            event["phase"] = "backup"
        elif Path(args[0]).name == "previous-MuuIME.app":
            event["phase"] = "restore"
        elif Path(args[-1]) == Path(config["target"]):
            event["phase"] = "replace"
        else:
            event["phase"] = "quarantine"
    events_path = Path(os.environ["MUU_INSTALL_FIXTURE_EVENTS"])
    with events_path.open("a") as stream:
        stream.write(json.dumps(event) + "\n")

    if tool == "codesign":
        # Anything other than strict verification (signing, --force, repair)
        # fails, which is how a reintroduced re-sign step is caught.
        if args[:-1] != ["--verify", "--deep", "--strict", "--verbose=2"]:
            return 90  # No signing or repair operation is simulated as success.
        if not bundle_exists(args[-1]):
            return 96
        return 71 if config.get("signature_failure") == event["phase"] else 0
    if tool == "plutil":
        assert args[:1] == ["-extract"] and args[2] == "raw"
        with confined(args[3]).open("rb") as stream:
            print(plistlib.load(stream)[args[1]])
        return 0
    if tool == "ditto":
        shutil.copytree(confined(args[0]), confined(args[1]))
        if config.get("stage_hash_changed"):
            (confined(args[1]) / "Contents/MacOS/MuuIME").write_bytes(b"changed-stage")
        return 72 if config.get("copy_failure") else 0
    if tool in ("mv", "rm"):
        for value in args:
            if not value.startswith("-"):
                confined(value)
        if tool == "mv" and (config.get("move_failure") == event["phase"]
                             or config.get("restore_failure") and event["phase"] == "restore"):
            return 76
        if tool == "rm" and config.get("cleanup_failure"):
            return 77
        status = subprocess.run(["/bin/" + tool, *args]).returncode
        if tool == "mv" and status == 0 and event["phase"] == "replace":
            if config.get("installed_hash_changed"):
                (Path(config["target"]) / "Contents/MacOS/MuuIME").write_bytes(b"changed-installed")
            if config.get("replacement_error_after_rename"):
                return 78
        return status
    if tool in ("sleep", "reload-ime.sh"):
        return 0
    if tool != "xcrun" or args[0] != "swift":
        return 91
    action = event["action"]
    if Path(args[1]).name == "audit-installed-mode-icons.swift":
        if not bundle_exists(args[3]):
            return 96
        if action == "verify-artifact":
            assert args[4:8] == ["a" * 40, "42", config["variant"], config["sha"]]
            with (Path(args[3]) / "Contents/Info.plist").open("rb") as stream:
                if plistlib.load(stream)["MuuBuildVariant"] != config["variant"]:
                    return 74
            assert confined(args[8]).name == "codesign.json"
            if config.get("authority_failure") == event["phase"]:
                return 73
            print('{"accepted":true}')
        elif action == "signature":
            signature = dict(config["signature"])
            if config.get("signature_changed") == event["phase"]:
                signature["cdHash"] = "d" * 40
            print(json.dumps(signature, sort_keys=True))
        else:
            return 92
        return 0
    if Path(args[1]).name != "tis-control.swift":
        return 93
    if action == "register":
        assert confined(args[3]) == confined(config["target"])
        count = sum(json.loads(row).get("action") == "register" for row in events_path.read_text().splitlines())
        return 74 if count <= config.get("registration_failures", 0) else 0
    if action in ("migrate-two-mode", "verify-two-mode"):
        assert args[3:] == ["dev.muu.inputmethod.Muu"]
        return 75 if config.get("tis_failure") == action else 0
    return 94


# Exercise installer checks with fake tools and a temporary home.
# Logout/login, menus and appearance require separate manual verification.
class InstallerTests(unittest.TestCase):
    # A synthetic TimeoutExpired, no real 30-second wait. Bytes cut inside a
    # UTF-8 character, a truncated event line, and str stdout with no stderr
    # must all be saved as-is and survive the fixture's removal, and the
    # very same exception must propagate with timeout=30.
    def test_timeout_preserves_partial_output_and_fixture_events(self):
        for stdout, stderr in ((b"partial-out\xe3", b"partial-err\xe3"), ("text-output", None)):
            with self.subTest(stdout=stdout), tempfile.TemporaryDirectory() as destination:
                with mock.patch.dict(os.environ, {"MUU_INSTALL_TEST_EVIDENCE_DIR": destination}):
                    with Installation() as fixture:
                        events = b'{"tool":"mv","phase":"replace"}\n{"partial"'
                        fixture.events_file.write_bytes(events)
                        error = subprocess.TimeoutExpired(["fixture-installer"], 30,
                                                          output=stdout, stderr=stderr)
                        with mock.patch.object(subprocess, "run", side_effect=error) as run:
                            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                                fixture.run()
                        self.assertIs(raised.exception, error)
                        self.assertEqual(run.call_args.kwargs["timeout"], 30)
                        fixture_root = fixture.root
                    self.assertFalse(fixture_root.exists())
                    saved = list(Path(destination).iterdir())
                    self.assertEqual(len(saved), 1)
                    expected_out = stdout.encode() if isinstance(stdout, str) else stdout
                    self.assertEqual((saved[0] / "stdout.partial").read_bytes(), expected_out)
                    self.assertEqual((saved[0] / "stderr.partial").read_bytes(), stderr or b"")
                    self.assertEqual((saved[0] / "events.jsonl").read_bytes(), events)
                    facts = json.loads((saved[0] / "timeout.json").read_text())
                    self.assertEqual(facts["timeout_seconds"], 30)
                    self.assertEqual(facts["command"], error.cmd)

    # The fakes themselves must fail on a missing or incomplete bundle (96),
    # so a test cannot pass because the installer verified an empty path.
    def test_verification_tools_reject_a_bundle_before_copying(self):
        with Installation() as fixture:
            missing = fixture.root / "missing/MuuIME.app"
            incomplete = fixture.root / "incomplete/MuuIME.app"
            Installation.write_bundle(incomplete, fixture.new_bytes)
            (incomplete / "Contents/MacOS/MuuIME").unlink()
            for app, expected in ((fixture.source, 0), (missing, 96), (incomplete, 96)):
                commands = [
                    ["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app)],
                    ["xcrun", "swift", "audit-installed-mode-icons.swift", "verify-artifact", str(app),
                     "a" * 40, "42", "standard", fixture.digest, str(fixture.signature_file)],
                    ["xcrun", "swift", "audit-installed-mode-icons.swift", "signature", str(app)],
                ]
                for command in commands:
                    with self.subTest(app=app.name, command=command[:4], expected=expected):
                        result = subprocess.run(command, env=fixture.environment, capture_output=True,
                                                text=True, timeout=30)
                        self.assertEqual(result.returncode, expected, result.stderr)

    # Contract: strict verification, artifact authority and the signature
    # JSON run at source, stage and installed, in that order, with the bytes
    # unchanged, then register -> migrate-two-mode -> verify-two-mode. This
    # replaces the "no re-sign", per-phase SHA/signature-JSON and two-mode
    # migration spellings that were checked in installer and runbook text.
    def test_reviewed_artifact_is_verified_at_every_copy_without_resigning(self):
        with Installation() as fixture:
            result = fixture.run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(fixture.target_bytes, fixture.new_bytes)
            self.assertEqual((fixture.source / "Contents/MacOS/MuuIME").read_bytes(), fixture.new_bytes)
            for tool, action in (("codesign", None), ("xcrun", "verify-artifact"), ("xcrun", "signature")):
                events = [e for e in fixture.events if e["tool"] == tool and e.get("action") == action]
                self.assertEqual([e["phase"] for e in events], ["source", "stage", "installed"])
            tis = [e["action"] for e in fixture.events if e["tool"] == "xcrun" and Path(e["args"][1]).name == "tis-control.swift"]
            self.assertEqual(tis, ["register", "migrate-two-mode", "verify-two-mode"])

    # A dogfood artifact installs when the operator names dogfood, with the
    # same hash and signature checks. Named as standard, it is refused and the
    # installed app keeps its old bytes.
    def test_explicit_dogfood_artifact_can_be_installed_without_relaxing_authority(self):
        with Installation(variant="dogfood") as fixture:
            result = fixture.run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(fixture.target_bytes, fixture.new_bytes)
        with Installation(variant="dogfood") as fixture:
            result = fixture.run(**{"--expected-variant": "standard"})
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(fixture.target_bytes, fixture.old_bytes)

    # Negatives: each of the six authority arguments omitted, and an
    # unreviewed variant. None may reach any external command or touch the
    # installed app. Replaces the required-argument spellings and "installer
    # must not infer a standard or diagnostic artifact path".
    def test_explicit_review_authority_is_required_before_integration(self):
        for key in ("--source-app", "--expected-commit", "--expected-build", "--expected-variant",
                    "--expected-executable-sha256-file", "--expected-codesign-json"):
            with self.subTest(omitted=key), Installation() as fixture:
                self.assertNotEqual(fixture.run(omit=key).returncode, 0)
                self.assertEqual(fixture.events, [])
                self.assertEqual(fixture.target_bytes, fixture.old_bytes)
        with Installation() as fixture:
            self.assertNotEqual(fixture.run(**{"--expected-variant": "unreviewed"}).returncode, 0)
            self.assertEqual(fixture.events, [])

    # Negatives: a signature, authority or signature-JSON mismatch at
    # source or stage, a copy failure, a changed stage and a changed source.
    # Each must keep the old bytes and never register. Replaces the stage
    # hash/cmp spellings and the hash- and codesign-omission fixtures.
    def test_source_or_staging_rejections_preserve_the_previous_app(self):
        faults = [{key: phase} for key in ("signature_failure", "authority_failure", "signature_changed")
                  for phase in ("source", "stage")]
        faults += [{"copy_failure": True}, {"stage_hash_changed": True}, {"source_hash_changed": True}]
        for fault in faults:
            with self.subTest(fault=fault), Installation(**fault) as fixture:
                if fault.get("source_hash_changed"):
                    (fixture.source / "Contents/MacOS/MuuIME").write_bytes(b"unreviewed-source")
                self.assertNotEqual(fixture.run().returncode, 0)
                self.assertEqual(fixture.target_bytes, fixture.old_bytes)
                self.assertFalse(any(e.get("action") == "register" for e in fixture.events))

    # The same mismatches after the move must stop registration. That the
    # previous app comes back is asserted by the *_restores_previous_app
    # tests below; this test covers only that nothing is registered.
    def test_installed_authority_rejections_stop_before_registration(self):
        faults = [{key: "installed"} for key in ("signature_failure", "authority_failure", "signature_changed")]
        faults.append({"installed_hash_changed": True})
        for fault in faults:
            with self.subTest(fault=fault), Installation(**fault) as fixture:
                self.assertNotEqual(fixture.run().returncode, 0)
                self.assertFalse(any(e.get("action") == "register" for e in fixture.events))

    # Contract: a transient register failure is retried until success (3
    # tries here). Persistent register, migrate or verify failure stops after
    # 10 tries with one fake sleep between them, never looping indefinitely.
    def test_public_registration_retries_have_a_bound(self):
        for fault, expected_count, succeeds in (({"registration_failures": 2}, 3, True),
                                                ({"registration_failures": 100}, 10, False),
                                                ({"tis_failure": "migrate-two-mode"}, 10, False),
                                                ({"tis_failure": "verify-two-mode"}, 10, False)):
            with self.subTest(fault=fault), Installation(**fault) as fixture:
                result = fixture.run()
                self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                self.assertEqual(sum(e.get("action") == "register" for e in fixture.events), expected_count)
                self.assertEqual(sum(e["tool"] == "sleep" for e in fixture.events), expected_count - 1)
                self.assertEqual(fixture.target_bytes, fixture.new_bytes)

    # Failure-recovery contracts, one fault per test (they were added first as
    # strict expected failures). Each fault is injected at a named mv phase
    # (backup, replace, quarantine, restore) or in cleanup's rm.
    #
    # Backup mv fails: the previous app never moved, so it is still in place.
    def test_backup_move_failure_preserves_previous_app(self):
        with Installation(move_failure="backup") as fixture:
            self.assertNotEqual(fixture.run().returncode, 0)
            self.assertEqual(fixture.target_bytes, fixture.old_bytes)

    # The rename succeeded but mv reports failure: the target must hold either
    # the previous app or the reviewed new bytes, never a partial bundle.
    def test_error_after_replacement_rename_keeps_a_valid_app(self):
        with Installation(replacement_error_after_rename=True) as fixture:
            self.assertNotEqual(fixture.run().returncode, 0)
            self.assertIn(fixture.target_bytes, (fixture.old_bytes, fixture.new_bytes))

    # Only cleanup's rm fails: the install itself stays valid and the failure
    # is still reported.
    def test_cleanup_failure_keeps_a_valid_app(self):
        with Installation(cleanup_failure=True) as fixture:
            self.assertNotEqual(fixture.run().returncode, 0)
            self.assertIn(fixture.target_bytes, (fixture.old_bytes, fixture.new_bytes))

    # Negative for the original bug: before the fix the EXIT trap deleted the
    # stage holding the only previous app when this mv failed.
    def test_replacement_move_failure_restores_previous_app(self):
        with Installation(move_failure="replace") as fixture:
            self.assertNotEqual(fixture.run().returncode, 0)
            self.assertTrue(fixture.target.exists(), "replacement failure lost the previous app")
            self.assertEqual(fixture.target_bytes, fixture.old_bytes)

    # Even the recovery rename fails: an intact previous app must remain
    # somewhere in the private root, and stderr must name that path.
    def test_failed_restore_keeps_recoverable_previous_app(self):
        with Installation(move_failure="replace", restore_failure=True) as fixture:
            result = fixture.run()
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(fixture.recoverable_previous_apps, "failed restore lost the recovery copy")
            self.assertIn(str(fixture.recoverable_previous_apps[0]), result.stderr)

    # Moving the rejected new copy aside fails: the previous app still survives
    # in the stage, at the path reported on stderr.
    def test_failed_quarantine_keeps_recoverable_previous_app(self):
        with Installation(installed_hash_changed=True, move_failure="quarantine") as fixture:
            result = fixture.run()
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(fixture.recoverable_previous_apps, "failed quarantine lost the recovery copy")
            self.assertIn(str(fixture.recoverable_previous_apps[0]), result.stderr)

    # Each post-placement check (signature, authority, signature JSON, hash)
    # must restore the previous bytes. These fixtures start with a previous
    # app; the fresh-install tests below cover the case with nothing to restore.
    def assert_previous_app_restored(self, **fault):
        with Installation(**fault) as fixture:
            self.assertNotEqual(fixture.run().returncode, 0)
            self.assertTrue(fixture.target.exists(), "verification failure lost the previous app")
            self.assertEqual(fixture.target_bytes, fixture.old_bytes)

    def test_installed_signature_failure_restores_previous_app(self):
        self.assert_previous_app_restored(signature_failure="installed")

    def test_installed_authority_failure_restores_previous_app(self):
        self.assert_previous_app_restored(authority_failure="installed")

    def test_installed_signature_mismatch_restores_previous_app(self):
        self.assert_previous_app_restored(signature_changed="installed")

    def test_installed_hash_mismatch_restores_previous_app(self):
        self.assert_previous_app_restored(installed_hash_changed=True)

    # With no previous app, a failed post-placement check must leave the
    # target absent (not even a link) and nothing registered. The only moves
    # are placement of the replacement and its quarantine.
    def test_fresh_install_rejections_leave_no_unverified_target(self):
        faults = ({"signature_failure": "installed"}, {"authority_failure": "installed"},
                  {"signature_changed": "installed"}, {"installed_hash_changed": True})
        for fault in faults:
            with self.subTest(fault=fault), Installation(has_previous_app=False, **fault) as fixture:
                result = fixture.run()
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertFalse(fixture.target.exists() or fixture.target.is_symlink(), result.stderr)
                self.assertFalse(any(e.get("action") == "register" for e in fixture.events))
                self.assertEqual([e["phase"] for e in fixture.events if e["tool"] == "mv"],
                                 ["replace", "quarantine"])

    # Positive control for the fresh-install path.
    def test_fresh_install_success_leaves_the_reviewed_app(self):
        with Installation(has_previous_app=False) as fixture:
            result = fixture.run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(fixture.target_bytes, fixture.new_bytes)

    # After verification, a TIS failure keeps the new app even on a fresh
    # install; quarantine is only for unverified copies.
    def test_fresh_install_tis_failure_keeps_the_verified_app(self):
        with Installation(has_previous_app=False, registration_failures=100) as fixture:
            result = fixture.run()
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(fixture.target_bytes, fixture.new_bytes)
            self.assertEqual(sum(e.get("action") == "register" for e in fixture.events), 10)
            self.assertFalse(any(e.get("phase") == "quarantine" for e in fixture.events))

    # A link at the target (relative, absolute or dangling) is refused before
    # any tool runs, and the link and the real older app are left exactly as
    # found. move_failure would expose the old loss if the check were bypassed.
    def test_target_symlinks_are_rejected_before_any_mutation(self):
        for form in ("relative", "absolute", "dangling"):
            with self.subTest(form=form), Installation(move_failure="replace") as fixture:
                older = fixture.target.with_name("Older.app")
                fixture.target.rename(older)
                destination = {"relative": older.name, "absolute": str(older),
                               "dangling": "Missing.app"}[form]
                fixture.target.symlink_to(destination)
                result = fixture.run()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("symbolic link", result.stderr)
                self.assertTrue(fixture.target.is_symlink())
                self.assertEqual(os.readlink(fixture.target), destination)
                self.assertEqual((older / "Contents/MacOS/MuuIME").read_bytes(), fixture.old_bytes)
                self.assertEqual(fixture.events, [])
                self.assertEqual(set(p.name for p in fixture.target.parent.iterdir()),
                                 {"MuuIME.app", "Older.app"})

    # The same for a linked Input Methods, Library or home directory: no event,
    # no stage directory, and the app behind the link unchanged.
    def test_install_path_symlinks_are_rejected_before_any_mutation(self):
        for level in ("Input Methods", "Library", "home"):
            for form in ("relative", "absolute", "dangling"):
                with self.subTest(level=level, form=form), Installation() as fixture:
                    ancestor = {"Input Methods": fixture.target.parent,
                                "Library": fixture.home / "Library", "home": fixture.home}[level]
                    real = ancestor.with_name(ancestor.name + "-real")
                    preserved = real / fixture.target.relative_to(ancestor)
                    ancestor.rename(real)
                    destination = {"relative": real.name, "absolute": str(real),
                                   "dangling": "Missing-directory"}[form]
                    ancestor.symlink_to(destination)
                    result = fixture.run()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("symbolic link", result.stderr)
                    self.assertTrue(ancestor.is_symlink())
                    self.assertEqual(os.readlink(ancestor), destination)
                    self.assertEqual((preserved / "Contents/MacOS/MuuIME").read_bytes(), fixture.old_bytes)
                    self.assertEqual(fixture.events, [])
                    self.assertFalse(list(fixture.root.rglob(".muu-ime-install.*")))


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fixture-command"]:
        raise SystemExit(fixture_command(sys.argv[2], sys.argv[3:]))
    unittest.main()
