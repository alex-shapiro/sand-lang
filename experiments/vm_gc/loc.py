"""Counts code lines (not blank, not comment-only, not unit tests) per arm.

Usage: python3 loc.py
"""

import pathlib
import re


def code_lines(path):
    lines, depth, in_tests = 0, 0, False
    for raw in pathlib.Path(path).read_text().splitlines():
        line = raw.strip()
        if line.startswith("#[cfg(test)]"):
            in_tests, depth = True, 0
            continue
        if in_tests:
            depth += line.count("{") - line.count("}")
            if depth <= 0 and "}" in line:
                in_tests = False
            continue
        if line and not line.startswith("//"):
            lines += 1
    return lines


def total(files):
    return sum(code_lines(f) for f in files)


src = pathlib.Path("src")
groups = {
    "arm1 core (heap, vm, codec)": [
        "arm1/heap.rs",
        "arm1/vm.rs",
        "arm1/codec_objects.rs",
        "arm1/codec.rs",
        "arm1/mod.rs",
    ],
    "arm1 host (B6, B8)": ["arm1/host.rs"],
    "arm2 core (arm1 heap, vm, and codec objects + scope, codec)": ["arm1/heap.rs", "arm1/vm.rs", "arm1/codec_objects.rs"]
    + [
        str(p.relative_to(src))
        for p in sorted((src / "arm2").glob("*.rs"))
        if p.name != "host.rs"
    ],
    "arm2 host (B6, B8)": ["arm2/host.rs"],
    "arm3 core (value, heap, vm, codec)": [
        "arm3/value.rs",
        "arm3/heap.rs",
        "arm3/vm.rs",
        "arm3/codec.rs",
        "arm3/mod.rs",
    ],
    "arm3 host (B6, B8)": ["arm3/host.rs"],
    "shared format and S-expression codecs": [
        "shared/format.rs",
        "shared/json.rs",
        "shared/sexpr.rs",
    ],
}
for name, files in groups.items():
    files = [src / f for f in files]
    if all(f.exists() for f in files):
        print(f"{total(files):6} {name}")
        for f in files:
            print(f"         {code_lines(f):5} {f.relative_to(src)}")
