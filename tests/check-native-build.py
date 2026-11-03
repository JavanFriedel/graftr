#!/usr/bin/env python3
"""Explicit clean-build check; Python orchestrates but is absent from the build's PATH."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile


PROJECT = Path(__file__).resolve().parents[1]


def main():
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    if not cargo or not rustc:
        raise RuntimeError("Put Cargo and rustc on PATH before running this check")
    with tempfile.TemporaryDirectory(prefix="graftr-build-no-bun-") as directory:
        root = Path(directory)
        project = root / "checkout"
        project.mkdir(parents=True)
        shutil.copytree(PROJECT / "src", project / "src")
        shutil.copytree(PROJECT / "completions", project / "completions")
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copyfile(PROJECT / name, project / name)
        for name in ("help-catalog.json", "build.sh"):
            shutil.copyfile(PROJECT / name, project / name)
        plugin = project / "plugins/herdr"
        plugin.mkdir(parents=True)
        shutil.copyfile(PROJECT / "plugins/herdr/herdr-plugin.toml", plugin / "herdr-plugin.toml")
        commands = root / "commands"
        commands.mkdir()
        for name in ("sh", "realpath", "dirname", "sed", "mkdir", "mktemp", "cp", "chmod", "mv", "rm",
                     "cc", "gcc", "ld", "ar", "as", "ranlib", "strip", "cargo", "rustc"):
            executable = shutil.which(name)
            if not executable:
                raise RuntimeError(f"Missing build tool: {name}")
            (commands / name).symlink_to(executable)
        path = str(commands)
        for name in ("bun", "node", "python", "python3", "jq"):
            if shutil.which(name, path=path):
                raise RuntimeError(f"Unexpected build runtime: {name}")
        env = {**os.environ, "PATH": path, "CARGO_NET_OFFLINE": "true", "CARGO_TARGET_DIR": str(root / "fresh cache"),
               "RUSTC": rustc}
        for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
            env.pop(name, None)
        subprocess.run(["sh", str(project / "build.sh")], cwd=project, env=env, check=True, timeout=180)
        executable = project / "dist/graftr"
        if executable.read_bytes()[:4] != b"\x7fELF":
            raise RuntimeError("Build did not publish an ELF executable")
        subprocess.run([str(executable), "--help"], env=env, stdout=subprocess.DEVNULL, check=True, timeout=10)
        print("Clean native build/install/help passed: no TypeScript source, Bun, Node, Python or jq on build PATH.")


if __name__ == "__main__":
    main()
