#!/usr/bin/env python3
"""Package a checked native build and dependency notices; never overwrite release assets."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def release_files(root, binary, metadata, rust_docs):
    files = {"graftr": binary.read_bytes(), "LICENSE": (root / "LICENSE").read_bytes(),
             "README.md": (root / "README.md").read_bytes(),
             "completions/graftr.bash": (root / "completions/graftr.bash").read_bytes(),
             "plugins/herdr/herdr-plugin.toml": (root / "plugins/herdr/herdr-plugin.toml").read_bytes()}
    elf = files["graftr"]
    if len(elf) < 20 or elf[:6] != b"\x7fELF\x02\x01" or elf[18:20] != b"\x3e\x00":
        raise ValueError("Release requires a native Linux x86_64 ELF executable")
    notices = []
    for package in metadata["packages"]:
        if package["id"] == metadata["resolve"]["root"]:
            continue
        name = f'{package["name"]}-{package["version"]}'
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", name) or not package.get("license"):
            raise ValueError("Dependency lacks a safe name or license declaration")
        directory = Path(package["manifest_path"]).parent.resolve()
        licenses = {p for p in directory.iterdir() if p.is_file()
                    and p.name.lower().startswith(("license", "licence", "copying", "copyright"))}
        if package.get("license_file"):
            licenses.add(directory / package["license_file"])
        if not licenses:
            raise ValueError(f"No dependency license text found: {name}")
        for path in sorted(licenses):
            if path.is_symlink() or not path.resolve().is_relative_to(directory) or not path.is_file():
                raise ValueError(f"Unsafe dependency license path: {name}")
            relative = path.relative_to(directory).as_posix()
            files[f"licenses/crates/{name}/{relative}"] = path.read_bytes()
        notices.append({"name": package["name"], "version": package["version"], "license": package["license"]})
    copyright_file = rust_docs / "COPYRIGHT-library.html"
    if not copyright_file.is_file() or not (rust_docs / "licenses").is_dir():
        raise ValueError("Rust library notices are missing; install the matching rust-docs component")
    files["licenses/rust/COPYRIGHT-library.html"] = copyright_file.read_bytes()
    rust_licenses = sorted((rust_docs / "licenses").glob("*.txt"))
    if not rust_licenses:
        raise ValueError("Rust license texts are missing")
    for path in rust_licenses:
        files[f"licenses/rust/{path.name}"] = path.read_bytes()
    files["THIRD_PARTY_NOTICES.json"] = (json.dumps(notices, indent=2, sort_keys=True) + "\n").encode()
    return files


def package(root, binary, metadata, rust_docs, output, tag, target, revision, timestamp):
    project = next(p for p in metadata["packages"] if p["id"] == metadata["resolve"]["root"])
    if tag != f'v{project["version"]}' or not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Tag must match the exact Cargo package version")
    if target != TARGET or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Unsupported release target or source revision")
    files = release_files(root, binary, metadata, rust_docs)
    receipt = {"schemaVersion": 1, "version": project["version"], "target": target, "sourceRevision": revision,
               "files": {name: digest(data) for name, data in sorted(files.items())}}
    files["RELEASE.json"] = (json.dumps(receipt, indent=2, sort_keys=True) + "\n").encode()
    name = f"graftr-{tag}-{target}"
    output.mkdir(parents=True, exist_ok=True)
    archive, checksums = output / f"{name}.tar.gz", output / "SHA256SUMS"
    if archive.exists() or checksums.exists():
        raise ValueError("Refusing to overwrite existing release assets")
    with tempfile.TemporaryDirectory(prefix=".release-", dir=output) as temporary:
        staged = Path(temporary) / archive.name
        with staged.open("wb") as stream, gzip.GzipFile(filename="", mode="wb", fileobj=stream, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                for relative, data in sorted(files.items()):
                    member = tarfile.TarInfo(f"{name}/{relative}")
                    member.size, member.mtime = len(data), timestamp
                    member.mode = 0o755 if relative == "graftr" else 0o644
                    tar.addfile(member, io.BytesIO(data))
        sums = Path(temporary) / "SHA256SUMS"
        sums.write_text(f"{digest(staged.read_bytes())}  {archive.name}\n")
        os.link(staged, archive)
        try:
            os.link(sums, checksums)
        except BaseException:
            archive.unlink()
            raise
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--target", default=TARGET)
    parser.add_argument("--output", type=Path, default=ROOT / "dist/release")
    args = parser.parse_args()
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("Release packaging requires a clean committed checkout")
    subprocess.run(["sh", str(ROOT / "build.sh")], cwd=ROOT, check=True)
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT))
    sysroot = Path(subprocess.check_output([os.environ.get("RUSTC", "rustc"), "--print", "sysroot"], text=True).strip())
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    timestamp = int(subprocess.check_output(["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT, text=True))
    print(package(ROOT, ROOT / "dist/graftr", metadata, sysroot / "share/doc/rust", args.output,
                  args.tag, args.target, revision, timestamp))


if __name__ == "__main__":
    main()
