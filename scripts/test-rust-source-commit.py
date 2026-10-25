#!/usr/bin/env python3
"""Link real fixture Rust archives; a wrong library must fail the app gate."""

import os
import plistlib
import subprocess
import tempfile
from pathlib import Path

from rust_toolchain import tool_command

REPO = Path(__file__).resolve().parent.parent
CHECKER = REPO / "scripts/verify-rust-source-commit.py"
EXPECTED = "1" * 40


# Builds three real arm64 executables from a tiny no_std staticlib carrying
# the expected, another, or "unknown" source marker, all with the expected
# commit in Info.plist. Only the expected marker may pass. A fake lipo then
# shows the arm64-only rule rejecting any other architecture list.
def main():
    rustc, rust_environment = tool_command("rustc")
    with tempfile.TemporaryDirectory(prefix="muu-rust-source-test-") as directory:
        root = Path(directory)
        main_c = root / "main.c"
        main_c.write_text("extern const char *muu_core_version(void); int main(void) { return muu_core_version()[0] == 0; }\n")
        for identifier in [EXPECTED, "0" * 40, "unknown"]:
            app = root / (identifier + ".app")
            binary = app / "Contents/MacOS/fixture"
            binary.parent.mkdir(parents=True)
            with (app / "Contents/Info.plist").open("wb") as output:
                plistlib.dump({"CFBundleExecutable": "fixture", "MuuSourceCommit": EXPECTED}, output)
            source = root / "fixture.rs"
            source.write_text(
                '#![no_std]\n'
                '#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }\n'
                f'static ID: &[u8] = b"MUU_RUST_SOURCE_COMMIT={identifier}\\0";\n'
                '#[unsafe(no_mangle)] pub extern "C" fn muu_core_version() -> *const u8 { core::hint::black_box(ID); b"fixture\\0".as_ptr() }\n'
            )
            library = root / "arm64.a"
            subprocess.run(rustc + ["--crate-type=staticlib", "--edition=2024", "--target", "aarch64-apple-darwin",
                            "-Cpanic=abort", "-Copt-level=2", str(source), "-o", str(library)], env=rust_environment, check=True)
            subprocess.run(["xcrun", "clang", "-arch", "arm64", "-mmacosx-version-min=15.0",
                            str(main_c), str(library), "-o", str(binary)], check=True)
            result = subprocess.run(["python3", str(CHECKER), str(app), EXPECTED], capture_output=True, text=True)
            assert (result.returncode == 0) == (identifier == EXPECTED), (identifier, result.stdout, result.stderr)
            if identifier == EXPECTED:
                # No Intel build is needed to retain the architecture gate:
                # a valid arm64 executable must still reject any other lipo inventory.
                tools = root / "tools"
                tools.mkdir()
                fake = tools / "lipo"
                for arches in ["x86_64", "arm64 x86_64", ""]:
                    fake.write_text("#!/bin/sh\nprintf '%s\\n' '" + arches + "'\n")
                    fake.chmod(0o755)
                    wrong_arch = subprocess.run(["python3", str(CHECKER), str(app), EXPECTED],
                        capture_output=True, text=True, env=os.environ | {"PATH": str(tools) + os.pathsep + os.environ["PATH"]})
                    assert wrong_arch.returncode != 0 and "arm64 only" in wrong_arch.stderr
    print("Rust source commit checks passed (arm64 only, wrong/unknown archive rejected)")


if __name__ == "__main__":
    main()
