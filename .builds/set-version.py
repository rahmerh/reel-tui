#!/usr/bin/env python3
# Sets the reel-tui version in Cargo.toml and Cargo.lock to RELEASE_VERSION,
# the release tag, and checks cargo resolves the same version.
import json
import os
from pathlib import Path
import re
import subprocess

version = os.environ["RELEASE_VERSION"]
semver = re.compile(
    r"^(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)"
    r"(?:-(?:"
    r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*"
    r"))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
if not semver.fullmatch(version):
    raise SystemExit(
        f"Tag {version!r} is not an exact SemVer version such as 0.2.0"
    )

manifest_path = Path("Cargo.toml")
manifest = manifest_path.read_text()
manifest, manifest_updates = re.subn(
    r'(?m)^(version\s*=\s*")[^"]+("\s*)$',
    rf'\g<1>{version}\g<2>',
    manifest,
    count=1,
)
if manifest_updates != 1:
    raise SystemExit("Could not update the package version in Cargo.toml")
manifest_path.write_text(manifest)

lock_path = Path("Cargo.lock")
lock = lock_path.read_text()
lock, lock_updates = re.subn(
    r'(\[\[package\]\]\nname = "reel-tui"\nversion = ")[^"]+("\n)',
    rf'\g<1>{version}\g<2>',
    lock,
    count=1,
)
if lock_updates != 1:
    raise SystemExit("Could not update the reel-tui version in Cargo.lock")
lock_path.write_text(lock)

metadata = json.loads(
    subprocess.check_output(
        [
            "cargo",
            "metadata",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
        ],
        text=True,
    )
)
package = next(
    item for item in metadata["packages"] if item["name"] == "reel-tui"
)
if package["version"] != version:
    raise SystemExit(
        f"Cargo resolved version {package['version']}, expected {version}"
    )
