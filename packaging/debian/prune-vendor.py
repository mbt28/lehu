#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Leave the vendored crates a Linux build never compiles as empty stubs.

    packaging/debian/prune-vendor.py SOURCE_TREE TARGET

SOURCE_TREE holds Cargo.lock and the vendored crates in cargo-vendor/
(cargo vendor --versioned-dirs). Cargo.lock lists every platform's crates,
Windows' among them; a crate outside TARGET's dependency graph keeps its
Cargo.toml, so the lock file still resolves, and loses its sources, which
no build here reads. Its checksum file then lists no files.
"""
import json
import pathlib
import shutil
import subprocess
import sys

tree = pathlib.Path(sys.argv[1]).resolve()
target = sys.argv[2]
vendor = tree / "cargo-vendor"

meta = json.loads(
    subprocess.run(
        [
            "cargo", "metadata", "--format-version", "1", "--locked", "--offline",
            "--filter-platform", target,
            "--config", 'source.crates-io.replace-with="vendored-sources"',
            "--config", f'source.vendored-sources.directory="{vendor}"',
        ],
        cwd=tree, check=True, capture_output=True, text=True,
    ).stdout
)
by_id = {p["id"]: p for p in meta["packages"]}
used = {
    f'{by_id[node["id"]]["name"]}-{by_id[node["id"]]["version"]}'
    for node in meta["resolve"]["nodes"]
}

stubbed = 0
for crate in sorted(vendor.iterdir()):
    if crate.name in used:
        continue
    for entry in crate.iterdir():
        if entry.name in ("Cargo.toml", ".cargo-checksum.json"):
            continue
        if entry.is_dir():
            shutil.rmtree(entry)
        else:
            entry.unlink()
    (crate / "src").mkdir()
    (crate / "src" / "lib.rs").write_text("")
    checksum = crate / ".cargo-checksum.json"
    sums = json.loads(checksum.read_text())
    sums["files"] = {}
    checksum.write_text(json.dumps(sums))
    stubbed += 1
print(f"prune-vendor: {stubbed} crates left as stubs for {target}")
