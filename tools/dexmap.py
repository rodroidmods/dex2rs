#!/usr/bin/env python3
"""Print a DEX file's map_list as (section, item count, bytes).

Sections in a dex map_list are laid out in increasing offset order, so the byte
size of each variable-sized section is the gap to the next section's offset.
Handy for finding *where* a rebuilt dex grew compared to the original:

    python3 tools/dexmap.py a.dex b.dex
"""
import struct
import sys

NAMES = {
    0x0000: "header_item",
    0x0001: "string_id_item",
    0x0002: "type_id_item",
    0x0003: "proto_id_item",
    0x0004: "field_id_item",
    0x0005: "method_id_item",
    0x0006: "class_def_item",
    0x0007: "call_site_id_item",
    0x0008: "method_handle_item",
    0x1000: "map_list",
    0x1001: "type_list",
    0x1002: "annotation_set_ref_list",
    0x1003: "annotation_set_item",
    0x2000: "class_data_item",
    0x2001: "code_item",
    0x2002: "string_data_item",
    0x2003: "debug_info_item",
    0x2004: "annotation_item",
    0x2005: "encoded_array_item",
    0x2006: "annotations_directory_item",
    0xF000: "hiddenapi_class_data_item",
}


def read_map(path):
    data = open(path, "rb").read()
    if not data.startswith(b"dex\n"):
        raise SystemExit(f"{path}: not a dex")
    (map_off,) = struct.unpack_from("<I", data, 52)
    (size,) = struct.unpack_from("<I", data, map_off)
    entries = []
    for i in range(size):
        t, _unused, count, off = struct.unpack_from("<HHII", data, map_off + 4 + i * 12)
        entries.append((t, count, off))
    entries.sort(key=lambda e: e[2])
    out = {}
    for i, (t, count, off) in enumerate(entries):
        end = entries[i + 1][2] if i + 1 < len(entries) else len(data)
        out[NAMES.get(t, f"type_{t:#x}")] = (count, end - off)
    return out, len(data)


def main():
    files = sys.argv[1:] or ["/tmp/orig/classes.dex", "/tmp/dexcheck/classes.dex"]
    tables = []
    for f in files:
        table, total = read_map(f)
        tables.append((f, table, total))
        print(f"{f}: {total:,} bytes")
    keys = sorted({k for _, t, _ in tables for k in t},
                  key=lambda k: -max(t.get(k, (0, 0))[1] for _, t, _ in tables))
    print()
    header = f"{'section':26s}" + "".join(f"{f.split('/')[-1]:>22s}" for f, _, _ in tables)
    print(header)
    print("-" * len(header))
    for k in keys:
        row = f"{k:26s}"
        for _, t, _ in tables:
            count, size = t.get(k, (0, 0))
            row += f"{count:>8,} {size:>12,}"
        print(row)


if __name__ == "__main__":
    main()
