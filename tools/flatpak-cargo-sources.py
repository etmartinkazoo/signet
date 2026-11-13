#!/usr/bin/env python3
"""Write packaging/flatpak/cargo-sources.json from Cargo.lock, for flatpak-builder.

A Flatpak build has no network, so flatpak-builder downloads every crate Cargo.lock names
beforehand: each crate archive from static.crates.io, checked against the SHA-256 in Cargo.lock,
unpacked into cargo/vendor/<name>-<version> with the .cargo-checksum.json that Cargo's directory
sources need, plus a cargo/config.toml pointing crates.io at that folder. The manifest sets
CARGO_HOME to the module's cargo/ folder. Standard library only; no network request.

Usage: tools/flatpak-cargo-sources.py [--check]   (--check: fail if the file is out of date)
"""
import json
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
LOCK = ROOT / "Cargo.lock"
OUTPUT = ROOT / "packaging" / "flatpak" / "cargo-sources.json"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR = "cargo/vendor"
CARGO_CONFIG = f"""[source.vendored-sources]
directory = "{VENDOR}"

[source.crates-io]
replace-with = "vendored-sources"
"""


def crate_sources(lock: dict) -> list[dict]:
    sources: list[dict] = []
    for package in sorted(lock["package"], key=lambda p: (p["name"], p["version"])):
        origin = package.get("source")
        if origin is None:  # a workspace member: comes with the source tree
            continue
        if origin != CRATES_IO:
            raise SystemExit(f"{package['name']} {package['version']}: unsupported source {origin}")
        name, version, checksum = package["name"], package["version"], package["checksum"]
        dest = f"{VENDOR}/{name}-{version}"
        sources.append({
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
            "sha256": checksum,
            "dest": dest,
        })
        sources.append({
            "type": "inline",
            "contents": json.dumps({"package": checksum, "files": {}}),
            "dest": dest,
            "dest-filename": ".cargo-checksum.json",
        })
    sources.append({"type": "inline", "contents": CARGO_CONFIG, "dest": "cargo", "dest-filename": "config.toml"})
    return sources


def main() -> int:
    with LOCK.open("rb") as f:
        text = json.dumps(crate_sources(tomllib.load(f)), indent=4) + "\n"
    if "--check" in sys.argv[1:]:
        if not OUTPUT.is_file() or OUTPUT.read_text() != text:
            print(f"{OUTPUT.relative_to(ROOT)} is out of date: run tools/flatpak-cargo-sources.py")
            return 1
        return 0
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(text)
    crates = sum(1 for source in json.loads(text) if source["type"] == "archive")
    print(f"wrote {OUTPUT.relative_to(ROOT)}: {crates} crates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
