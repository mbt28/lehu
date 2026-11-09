#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
# Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
# A Python port of LIVI's livi-core/src/theme_catalog.rs; changed for LEHU.
"""Prepare a CarPlay Ultra car-maker theme for LEHU, on a PC with internet.

The board has no internet. The phone fetches a theme's artwork itself; the
head unit only needs the theme's ids: the asset and its version (from Apple's
asset catalog), the SDK version, the theme's centre display id and, if wanted,
a cluster layout and palette (from the theme's package). This script reads
them and writes the file LEHU loads from its CarPlay state directory.

    scripts/ultra-theme.py list [-v]
        The themes in the catalog: asset id, version, the SDK version it
        needs (MinimumSDKVersion) and its size; -v for every attribute.

    scripts/ultra-theme.py make ASSET VERSION SDK [-o ultra-theme.txt]
        Downloads that theme's package once, reads its displays and layouts
        and writes the theme file. Copy it to the board as
        <state dir>/carplay/ultra-theme.txt (for example
        /var/lib/lehu/carplay/ultra-theme.txt), readable by LEHU's user.

Only the Python standard library. Apple's catalog server is signed by
Apple's own root, which is downloaded and checked against its published
SHA-256 before it is trusted.
"""

import argparse
import base64
import hashlib
import io
import json
import ssl
import sys
import urllib.request
import zipfile

CATALOG = "https://gdmf.apple.com/v2/assets"
ROOT_URL = "https://www.apple.com/appleca/AppleIncRootCertificate.cer"
ROOT_SHA256 = "b0b1730ecbc7ff4505142c49f1295e6eda6bcaed7e2c68c5be91b5a11001f024"
PACKAGE = "AssetData/CarPlayPhone.assets/package.json"
STYLES = "AssetData/CarPlayPhone.assets/theme.json"
ASK = {
    "AssetAudience": "0c88076f-c292-4dad-95e7-304db9d29d34",
    "AssetType": "com.apple.MobileAsset.CarDDPAssets",
    "ClientVersion": 2,
    "ProductType": "iPhone17,1",
    "BuildVersion": "24A446",
    "ProductVersion": "27.0",
    "HWModelStr": "D93AP",
    "DeviceName": "iPhone",
    "ClientData": {"AllowXmlFallback": False, "DeviceAccessClient": "softwareupdated"},
}


def apple_context():
    der = urllib.request.urlopen(ROOT_URL, timeout=30).read()
    if hashlib.sha256(der).hexdigest() != ROOT_SHA256:
        sys.exit("Apple's root certificate does not match its published hash")
    ctx = ssl.create_default_context()
    ctx.load_verify_locations(cadata=ssl.DER_cert_to_PEM_cert(der))
    return ctx


def catalog():
    req = urllib.request.Request(
        CATALOG,
        data=json.dumps(ASK).encode(),
        headers={"Content-Type": "application/json"},
    )
    jwt = urllib.request.urlopen(req, context=apple_context(), timeout=60).read().decode()
    payload = jwt.split(".")[1]
    payload += "=" * (-len(payload) % 4)
    return json.loads(base64.urlsafe_b64decode(payload)).get("Assets", [])


def shown(asset):
    skip = ("__BaseURL", "__RelativePath", "_DownloadSize", "_UnarchivedSize", "_Measurement")
    return {k: v for k, v in asset.items() if not k.startswith(skip)}


def cmd_list(args):
    for a in catalog():
        asset, version = a.get("ContentIdentifier"), a.get("_ContentVersion")
        sdk = a.get("MinimumSDKVersion", "?")
        size = a.get("_DownloadSize", 0) / 1e6
        print(f"{asset:24} version {version:<6} sdk {sdk:6} {size:6.1f} MB"
              f"   make {asset} {version} {sdk}")
        if args.verbose:
            for k, v in sorted(shown(a).items()):
                print(f"    {k}: {v}")


def plain(v):
    ok = v and len(v) <= 64 and all(c.isascii() and (c.isalnum() or c in "._-") for c in v)
    if not ok:
        sys.exit(f"{v!r} is not an id LEHU takes")
    return v


def cmd_make(args):
    found = [
        a for a in catalog()
        if a.get("ContentIdentifier") == args.asset
        and str(a.get("_ContentVersion")) == str(args.version)
    ]
    if not found:
        sys.exit(f"{args.asset}:{args.version} is not in the catalog (see `list`)")
    url = found[0]["__BaseURL"] + found[0]["__RelativePath"]
    print(f"downloading {url}", file=sys.stderr)
    package = zipfile.ZipFile(io.BytesIO(urllib.request.urlopen(url, timeout=600).read()))
    displays = json.loads(package.read(PACKAGE)).get("displays", [])
    try:
        styles = json.loads(package.read(STYLES))
    except KeyError:
        styles = {}
    center = next((d.get("id") for d in displays if d.get("type") == "centerConsole"), None)
    if not center:
        sys.exit("the package names no centre display")
    lines = [
        f"# CarPlay Ultra theme {args.asset}:{args.version}:{args.sdk}, from Apple's catalog",
        f"asset={plain(args.asset)}",
        f"version={int(args.version)}",
        f"sdk={plain(args.sdk)}",
        f"center={plain(center)}",
    ]
    cluster = next((d for d in displays if d.get("type") == "cluster"), {})
    layouts = [l for l in cluster.get("layouts", []) if l.get("id")]
    for l in layouts:
        print(f"layout {l['id']}: {l.get('displayName', '')}", file=sys.stderr)
    layout = next((l for l in layouts if l["id"] == args.layout), layouts[0] if layouts else None)
    if layout:
        palette = styles.get("defaultPaletteIDs", {}).get(layout.get("layoutStyleID", ""))
        lines.append(f"layout={plain(layout['id'])}")
        if palette:
            lines.append(f"palette={plain(palette)}")
    with open(args.output, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {args.output}", file=sys.stderr)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    ls = sub.add_parser("list")
    ls.add_argument("-v", "--verbose", action="store_true", help="every attribute")
    ls.set_defaults(run=cmd_list)
    m = sub.add_parser("make")
    m.add_argument("asset")
    m.add_argument("version")
    m.add_argument("sdk")
    m.add_argument("--layout", help="a cluster layout id (default: the first)")
    m.add_argument("-o", "--output", default="ultra-theme.txt")
    m.set_defaults(run=cmd_make)
    args = p.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
