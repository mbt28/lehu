#!/usr/bin/env python3
"""Writes THIRD-PARTY.md: every crate compiled into the lehu binary, the
licence LEHU takes it under, its copyright notices, and the licence texts,
so that a binary can carry what those licences ask for.

    scripts/third-party.py            # rewrites THIRD-PARTY.md
    scripts/third-party.py --check    # exits 1 if THIRD-PARTY.md is stale

Reads Cargo.lock through `cargo metadata` for each Linux target LEHU is
built for, follows the normal dependencies (not build, not dev) from the
workspace members, leaves out procedural macros (they run at compile time
and are not in the binary), and reads each crate's licence files from the
registry or vendor directory cargo resolved it to. The output depends only
on Cargo.lock and the crates' own files, so it is the same on any machine
with the same lock; needs the crates downloaded (`cargo fetch`).
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD-PARTY.md"
FONT_LICENCE = ROOT / "crates/lehu-ui/ui/fonts/OFL.txt"

# The targets LEHU is built for: the F1C200s board, the Raspberry Pi, a PC.
TARGETS = [
    "armv5te-unknown-linux-gnueabi",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
]

# When a crate offers a choice, the first of these it offers is taken.
PREFERRED = [
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "0BSD",
    "Unlicense",
    "Unicode-3.0",
    "Apache-2.0 WITH LLVM-exception",
    "MPL-2.0",
    "GPL-3.0-only",
    "GPL-3.0-or-later",
]

# Short licences, as published by their stewards; the crate's own copyright
# notice (in the table) goes above them.
TEXTS = {
    "MIT": """\
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
""",
    "BSD-2-Clause": """\
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
""",
    "BSD-3-Clause": """\
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
""",
    "ISC": """\
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
""",
    "0BSD": """\
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
""",
    "Zlib": """\
This software is provided 'as-is', without any express or implied
warranty. In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.
""",
    "Unlicense": """\
This is free and unencumbered software released into the public domain.

Anyone is free to copy, modify, publish, use, compile, sell, or
distribute this software, either in source code form or as a compiled
binary, for any purpose, commercial or non-commercial, and by any
means.

In jurisdictions that recognize copyright laws, the author or authors
of this software dedicate any and all copyright interest in the
software to the public domain. We make this dedication for the benefit
of the public at large and to the detriment of our heirs and
successors. We intend this dedication to be an overt act of
relinquishment in perpetuity of all present and future rights to this
software under copyright law.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.

For more information, please refer to <https://unlicense.org>
""",
}

# Long licences are taken from a crate that ships them, recognised by a
# phrase of their first lines.
MARKERS = {
    "Apache-2.0": "Apache License",
    "Apache-2.0 WITH LLVM-exception": "LLVM Exceptions to the Apache 2.0 License",
    "Unicode-3.0": "UNICODE LICENSE V3",
    "MPL-2.0": "Mozilla Public License Version 2.0",
}

GPL_NOTE = (
    "The GNU General Public License version 3 is the file LICENSE at the root of\n"
    "LEHU's source tree (on Debian systems also /usr/share/common-licenses/GPL-3).\n"
)

SYSTEM_LIBRARIES = """\
These are the system's own, found at run time and not shipped by LEHU; their
licences come with the system's packages.

| Library | Licence | Use |
|---|---|---|
| ffmpeg (libavcodec, libavutil) | LGPL-2.1-or-later as LEHU's images build it | the hardware decoder through v4l2-request, the AAC decoder |
| alsa-lib | LGPL-2.1-or-later | sound |
| libopus | BSD-3-Clause | the microphone and wireless audio |
| OpenSSL 3 | Apache-2.0 | TLS to the phone, hashing |
| libdbus-1 | AFL-2.1 or GPL-2.0-or-later | BlueZ, opened with dlopen |
| glibc | LGPL-2.1-or-later | the C library |
"""


def cargo(*args):
    return subprocess.run(
        ["cargo", *args, "--offline", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout


TREE_LINE = re.compile(r"^(\S+) v(\S+?)(?: \(.*\))?$")


def linked_packages():
    """The packages whose code is in the binary, over all targets.
    `cargo tree -p lehu` resolves features as `cargo build -p lehu` does
    (`cargo metadata` would union every workspace member's); normal edges
    only, so no build or dev dependencies, and no procedural macros."""
    wanted = set()
    for target in TARGETS:
        out = cargo("tree", "-p", "lehu", "-e", "normal,no-proc-macro",
                    "--target", target, "--prefix", "none", "-f", "{p}")
        for line in out.splitlines():
            m = TREE_LINE.match(line.replace(" (*)", "").strip())
            if m:
                wanted.add((m.group(1), m.group(2)))
    meta = json.loads(cargo("metadata", "--format-version", "1"))
    members = set(meta["workspace_members"])
    found = [
        p for p in meta["packages"]
        if (p["name"], p["version"]) in wanted and p["id"] not in members
    ]
    return sorted(found, key=lambda p: (p["name"], p["version"]))


def choices(expression):
    """The licences a crate is taken under: one of each AND-ed group, the
    first PREFERRED one offered, else the first named."""
    expr = (expression or "").replace("/", " OR ")
    taken = []
    for group in split_top(expr, " AND "):
        group = group.strip()
        while group.startswith("(") and group.endswith(")"):
            group = group[1:-1].strip()
        options = [o.strip() for o in split_top(group, " OR ") if o.strip()]
        if not options:
            continue
        pick = next((p for p in PREFERRED if p in options), options[0])
        taken.append(pick)
    return taken or ["(none stated)"]


def split_top(text, sep):
    parts, depth, start = [], 0, 0
    i = 0
    while i < len(text):
        c = text[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif depth == 0 and text.startswith(sep, i):
            parts.append(text[start:i])
            i += len(sep)
            start = i
            continue
        i += 1
    parts.append(text[start:])
    return parts


NOTICE_FILES = re.compile(r"^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|AUTHORS)", re.I)
COPYRIGHT = re.compile(r"copyright|©", re.I)
# Lines of licence boilerplate that mention copyright without being a notice.
BOILERPLATE = re.compile(
    r"notice|holder|owner|law|\[yyyy\]|<year>|<copyright|interest|subject to|"
    r"copyright and|the copyright|copyrighted|copyrights?,|copyright\s*$|"
    r"derivative works|you must retain|copyright license|statement to your|"
    r"your own copyright",
    re.I,
)


def notices(pkg, taken):
    """The copyright lines of a crate's licence files (the file of the
    licence taken first), else its lib.rs header, else its authors."""
    crate = Path(pkg["manifest_path"]).parent
    tokens = [t.split("-")[0].lower() for t in taken]

    def order(path):
        name = path.name.lower()
        return (0 if any(t in name for t in tokens) else 1, name)

    lines = []
    files = sorted((p for p in crate.iterdir() if p.is_file() and NOTICE_FILES.match(p.name)), key=order)
    if pkg.get("license_file"):
        files.insert(0, crate / pkg["license_file"])
    # The library's root source, wherever Cargo.toml puts it: its header
    # often carries the notice when the licence files are templates.
    roots = [Path(t["src_path"]) for t in pkg["targets"] if "lib" in t["kind"]]
    files.extend(roots)
    for f in files:
        try:
            text = f.read_text(errors="replace")
        except OSError:
            continue
        if f in roots:
            text = "\n".join(text.splitlines()[:30])
        for line in text.splitlines():
            m = COPYRIGHT.search(line)
            if not m:
                continue
            line = line[m.start():].strip().rstrip("*/ ")
            if BOILERPLATE.search(line) or len(line) < 12:
                continue
            if line not in lines:
                lines.append(line)
        if lines:
            break
    if lines:
        return lines[:6]
    authors = [re.sub(r"\s*<[^>]*>", "", a).strip() for a in pkg.get("authors") or []]
    authors = [a for a in authors if a]
    if authors:
        return [f"the authors of {pkg['name']}: {', '.join(authors)}"]
    repo = pkg.get("repository") or pkg.get("homepage") or "crates.io"
    return [f"the authors of {pkg['name']}, see {repo}"]


def long_text(licence, packages):
    """The text of a long licence, from the first crate that ships it."""
    marker = MARKERS[licence]
    for pkg in packages:
        crate = Path(pkg["manifest_path"]).parent
        for f in sorted(crate.iterdir()):
            if not f.is_file() or not NOTICE_FILES.match(f.name):
                continue
            try:
                text = f.read_text(errors="replace")
            except OSError:
                continue
            head = "\n".join(text.splitlines()[:12])
            if marker in head or (licence == "Apache-2.0 WITH LLVM-exception" and marker in text):
                if licence == "Apache-2.0" and MARKERS["Apache-2.0 WITH LLVM-exception"] in text:
                    continue
                return text.rstrip() + "\n"
    return f"(the text of {licence} was not found among the crates; see https://spdx.org/licenses/{licence}.html)\n"


def own_file_text(pkg):
    crate = Path(pkg["manifest_path"]).parent
    for f in sorted(crate.iterdir()):
        if f.is_file() and NOTICE_FILES.match(f.name):
            try:
                return f.read_text(errors="replace").rstrip() + "\n"
            except OSError:
                pass
    return None


def render():
    packages = linked_packages()
    rows = []
    used = {}
    for pkg in packages:
        taken = choices(pkg.get("license"))
        for lic in taken:
            used.setdefault(lic, []).append(pkg)
        where = "vendored copy in vendor/" if pkg.get("source") is None else "crates.io"
        rows.append((pkg, taken, where))

    out = []
    w = out.append
    w("# Third-party software in the lehu binary\n")
    w("Written by `scripts/third-party.py` from Cargo.lock; do not edit by hand.\n")
    w(
        "LEHU itself is Copyright (C) 2026 Mehmet Tekbas, GPL-3.0-or-later (LICENSE;\n"
        "NOTICE names the projects its code descends from). The binary links Slint,\n"
        "which LEHU takes under the GNU General Public License version 3 only, so a\n"
        "binary as a whole is distributed under version 3 of that licence. Every other\n"
        "crate below is under a licence compatible with it; where a crate offers a\n"
        "choice, the one in the \"Taken under\" column is the one LEHU takes, and its\n"
        "text is in the section \"Licence texts\" with the crate's copyright notice\n"
        "above, in the table. Procedural macros and build dependencies run at compile\n"
        "time only and are not listed.\n"
    )
    w(f"Targets: {', '.join(TARGETS)}. Crates: {len(rows)}.\n")
    w("## Crates\n")
    w("| Crate | Version | Taken under | Offered | Copyright |")
    w("|---|---|---|---|---|")
    for pkg, taken, where in rows:
        offered = (pkg.get("license") or "(see licence file)").replace("|", "\\|")
        note = "; ".join(n.replace("|", "\\|") for n in notices(pkg, taken))
        name = pkg["name"] + ("" if where == "crates.io" else " (vendor/)")
        w(f"| {name} | {pkg['version']} | {', '.join(taken)} | {offered} | {note} |")
    w("")
    w("## Licence texts\n")
    for lic in sorted(used, key=lambda l: (PREFERRED.index(l) if l in PREFERRED else 99, l)):
        crates = ", ".join(sorted({p["name"] for p in used[lic]}))
        w(f"### {lic}\n")
        w(f"Crates: {crates}.\n")
        if lic.startswith("GPL-3.0"):
            w(GPL_NOTE)
        elif lic in TEXTS:
            w("```text")
            w(TEXTS[lic].rstrip())
            w("```")
        elif lic in MARKERS:
            w("```text")
            w(long_text(lic, used[lic]).rstrip())
            w("```")
        else:
            text = own_file_text(used[lic][0])
            w("```text")
            w((text or f"(no licence file found in {used[lic][0]['name']})").rstrip())
            w("```")
        w("")
    w("## Font\n")
    w(
        "The screens embed Open Sans Regular 3.003, Copyright 2020 The Open Sans\n"
        "Project Authors (https://github.com/googlefonts/opensans), under the SIL Open\n"
        "Font License 1.1; its glyphs are rasterised into the binary at build time.\n"
    )
    w("```text")
    w(FONT_LICENCE.read_text().rstrip())
    w("```\n")
    w("## System libraries\n")
    w(SYSTEM_LIBRARIES)
    return "\n".join(out)


def main():
    text = render()
    if "--check" in sys.argv[1:]:
        current = OUT.read_text() if OUT.exists() else ""
        if current != text:
            sys.stderr.write("THIRD-PARTY.md is stale: run scripts/third-party.py\n")
            return 1
        print("THIRD-PARTY.md is current")
        return 0
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
