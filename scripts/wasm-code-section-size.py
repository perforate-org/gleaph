#!/usr/bin/env python3
"""Print WebAssembly binary section sizes, one file per argument.

The Internet Computer enforces its install limit on the code section, which is
smaller than the whole module, so budget checks measure the code section
directly instead of the file size. See design/implementation-gaps.md
GAP-2026-08-23-002 for the standing measurements.

Usage: wasm-code-section-size.py MODULE.wasm [MODULE.wasm ...]
"""

import sys

SECTION_NAMES = {
    0: "custom",
    1: "type",
    2: "import",
    3: "function",
    4: "table",
    5: "memory",
    6: "global",
    7: "export",
    8: "start",
    9: "element",
    10: "code",
    11: "data",
    12: "datacount",
}


def read_uleb128(buf: bytes, pos: int) -> tuple[int, int]:
    result = shift = 0
    while True:
        byte = buf[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, pos
        shift += 7


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit("usage: wasm-code-section-size.py MODULE.wasm [...]")
    for path in sys.argv[1:]:
        with open(path, "rb") as handle:
            buf = handle.read()
        if buf[:8] != b"\x00asm\x01\x00\x00\x00":
            sys.exit(f"{path}: not a WebAssembly binary")
        print(f"{path}: total={len(buf)}")
        pos = 8
        while pos < len(buf):
            section_id, pos = read_uleb128(buf, pos)
            size, pos = read_uleb128(buf, pos)
            body_start = pos
            pos += size
            label = SECTION_NAMES.get(section_id, f"unknown{section_id}")
            detail = ""
            if section_id == 0 and size > 0:
                name_len, name_pos = read_uleb128(buf, body_start)
                name = buf[name_pos : name_pos + name_len]
                detail = f" name={name.decode('utf-8', 'replace')!r}"
            print(
                f"  section[{section_id}] {label}: {size}{detail}"
            )


if __name__ == "__main__":
    main()
