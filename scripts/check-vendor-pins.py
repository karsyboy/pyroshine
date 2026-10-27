#!/usr/bin/env python3
"""Check that generated dependency bases agree with the manifests that pin them."""
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def check_pyrowave(base):
    """PyroWave is a native library: Rust provenance and Nix name the base."""
    rust = (ROOT / "moonshine-core/src/session/stream/video/pyrowave.rs").read_text()
    nix = (ROOT / "nix/pyrowave.nix").read_text()
    pins = {
        "pyrowave.rs SOURCE_REVISION": re.search(r'SOURCE_REVISION: &str = "([0-9a-f]{40})"', rust),
        "nix/pyrowave.nix upstreamRevision": re.search(r'upstreamRevision = "([0-9a-f]{40})"', nix),
    }
    for name, match in pins.items():
        if not match or match.group(1) != base["commit"]:
            raise ValueError(f"pyrowave: {name} must equal the UPSTREAM commit")
    if 'SOURCE_URL: &str = "https://github.com/Themaister/pyrowave"' not in rust:
        raise ValueError("pyrowave: SOURCE_URL must name the upstream repository")


def check(dependency):
    base = dict(line.split(" ", 1) for line in
                (ROOT / f"vendor/{dependency}-patches/UPSTREAM").read_text().splitlines())
    if dependency == "pyrowave":
        check_pyrowave(base)
        return
    core = tomllib.loads((ROOT / "moonshine-core/Cargo.toml").read_text())["dependencies"][dependency]
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
    repository = f"https://github.com/{'hgaiser' if dependency == 'pixelforge' else 'games-on-whales'}/{dependency}"
    package = "inputtino-sys" if dependency == "inputtino" else dependency
    path = "vendor/inputtino/bindings/rust/inputtino-sys" if dependency == "inputtino" else "vendor/pixelforge"
    if core["git"] != repository or workspace["patch"][repository][package]["path"] != path:
        raise ValueError(f"{dependency}: unexpected Cargo source or patch path")
    local = next(p for p in lock if p["name"] == package)
    if "source" in local:
        raise ValueError(f"{package}: lockfile must use the local path patch")
    manifest = tomllib.loads((ROOT / path / "Cargo.toml").read_text())["package"]
    if local["version"] != manifest["version"]:
        raise ValueError(f"{package}: manifest and lockfile versions differ")
    if dependency == "inputtino":
        public = next(p for p in lock if p["name"] == "inputtino")
        expected = f"git+{repository}?rev={base['commit']}#{base['commit']}"
        if core.get("rev") != base["commit"] or public.get("source") != expected:
            raise ValueError("inputtino: public Rust crate and native base must pin the same revision")
    elif core.get("tag") != base["describe"] or base["describe"] != f"v{manifest['version']}":
        raise ValueError("pixelforge: upstream tag and vendored version must agree with UPSTREAM")


if __name__ == "__main__":
    try:
        check(sys.argv[1])
    except (KeyError, ValueError, StopIteration) as error:
        sys.exit(f"error: {error}")
