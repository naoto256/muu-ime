#!/usr/bin/env python3
"""Run build tools from the repository's pinned Rust toolchain."""

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def tool_command(tool, arguments=()):
    if tool not in ("cargo", "rustc", "rustdoc"):
        raise ValueError(f"unsupported Rust tool: {tool}")
    version = (ROOT / "rust-toolchain").read_text(encoding="utf-8").strip()
    names = ["cargo", "rustc", "rustdoc"]
    if tool == "cargo" and arguments:
        if arguments[0] == "clippy":
            names.append("cargo-clippy")
        elif arguments[0] == "fmt":
            names.extend(("cargo-fmt", "rustfmt"))
    tools = {}
    for name in names:
        path = Path(subprocess.check_output(
            ["rustup", "which", "--toolchain", version, name], text=True, cwd=ROOT
        ).strip())
        if not path.is_absolute() or not os.access(path, os.X_OK):
            raise ValueError(f"rustup did not resolve an executable absolute path: {name}")
        tools[name] = path
    directory = tools["cargo"].parent
    if any(path.parent != directory for path in tools.values()):
        raise ValueError("Rust tools must belong to the same toolchain directory")
    # Cargo discovers cargo-clippy/cargo-fmt through PATH even when Cargo itself
    # is invoked by absolute path. Compiler and rustdoc are pinned separately.
    # RUSTUP_TOOLCHAIN also overrides an ambient value, so any rustup proxy a
    # build script reaches resolves to the same pin.
    environment = os.environ | {
        "PATH": str(directory) + os.pathsep + os.environ.get("PATH", ""),
        "RUSTC": str(tools["rustc"]),
        "RUSTDOC": str(tools["rustdoc"]),
        "RUSTUP_TOOLCHAIN": version,
    }
    if "rustfmt" in tools:
        environment["RUSTFMT"] = str(tools["rustfmt"])
    return [str(tools[tool]), *arguments], environment


def versions():
    pinned = (ROOT / "rust-toolchain").read_text(encoding="utf-8").strip()
    for tool in ("rustc", "cargo", "rustdoc"):
        command, environment = tool_command(tool, ["--version"])
        output = subprocess.check_output(command, env=environment, text=True).strip()
        print(output, flush=True)
        if output.split()[:2] != [tool, pinned]:
            raise ValueError(f"expected {tool} {pinned}")
    for arguments in (["clippy", "--version"], ["fmt", "--", "--version"]):
        command, environment = tool_command("cargo", arguments)
        subprocess.run(command, env=environment, check=True)


def main():
    if sys.argv[1:] == ["--versions"]:
        versions()
        return
    if len(sys.argv) < 2:
        raise SystemExit("usage: rust_toolchain.py <cargo|rustc|rustdoc> <args...>")
    command, environment = tool_command(sys.argv[1], sys.argv[2:])
    os.execve(command[0], command, environment)


if __name__ == "__main__":
    main()
