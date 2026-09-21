#!/usr/bin/env python3
"""Per-method comparison of the Rust port against the reference Python d2c.

Both compilers can emit a trace of the decoded Dalvik instruction stream (the
original `writer.py` `LOGD(...)` line, which the port writes as `d2c::trace(...)`
under `--trace`). The instruction stream is exactly the compiler's view of the
method budget: block order, branch targets, opcode decoding and operands. If the
two agree for every method, the decoded programs agree.

Usage:
    python3 tools/compare_golden.py GOLDEN_DIR/methods.cpp DIR_WITH_CPP --rs METHODS_RS

With no `--rs`, the golden side is read from `jni/nc/*.cpp` and the port side
from a `src/methods.rs`.
"""

import argparse
import glob
import os
import re
import sys

LOGD_RE = re.compile(r'LOGD\("([^"]*)"\);')
TRACE_RE = re.compile(r'd2c::trace\(b"([^"]*)"\);')
FN_RE = re.compile(r'^pub unsafe extern "system" fn ([A-Za-z0-9_$]+)\(')


def unescape(text: str) -> bytes:
    """Decode C/Rust string-literal escapes into bytes."""
    out = bytearray()
    i = 0
    while i < len(text):
        ch = text[i]
        if ch != "\\":
            out.extend(ch.encode("latin-1", "replace"))
            i += 1
            continue
        i += 1
        if i >= len(text):
            break
        esc = text[i]
        if esc == "x":
            out.append(int(text[i + 1 : i + 3], 16))
            i += 3
        elif esc == "n":
            out.append(0x0A)
            i += 1
        elif esc == "t":
            out.append(0x09)
            i += 1
        elif esc == "r":
            out.append(0x0D)
            i += 1
        elif esc == "0":
            out.append(0)
            i += 1
        else:
            out.extend(esc.encode("latin-1", "replace"))
            i += 1
    return bytes(out)


def dehex_once(data: bytes) -> bytes:
    """Undo a `hex_escape_string` pass: every byte was rendered as `\\xNN`."""
    if b"\\x" not in data:
        return data
    out = bytearray()
    i = 0
    while i < len(data):
        if data[i : i + 2] == b"\\x" and i + 3 < len(data) + 1:
            try:
                out.append(int(data[i + 2 : i + 4], 16))
            except ValueError:
                out.append(data[i])
                i += 1
                continue
            i += 4
        else:
            out.append(data[i])
            i += 1
    return bytes(out)


def golden_traces(nc_dir: str) -> dict:
    """symbol -> list of trace lines, from the reference C++ output."""
    # `index.tsv` maps file -> method for a reference tree that covers the long
    # JNI names too: those reach 505 characters, past the 255-byte filesystem
    # limit, so the file cannot be named after the method it holds. Without it
    # the file name is the symbol, which is how the original tool wrote them.
    names = {}
    index_path = os.path.join(nc_dir, "index.tsv")
    if os.path.exists(index_path):
        with open(index_path, "r", encoding="utf-8") as fp:
            for line in fp:
                name, _, method = line.rstrip("\n").partition("\t")
                if name and method:
                    names[name + ".cpp"] = method

    methods = {}
    for path in sorted(glob.glob(os.path.join(nc_dir, "*.cpp"))):
        base = os.path.basename(path)
        symbol = names.get(base, base[: -len(".cpp")])
        if not symbol.startswith("Java_"):
            continue  # Dex2C.cpp and friends
        with open(path, "r", encoding="utf-8", errors="surrogateescape") as fp:
            text = fp.read()
        # Python writes `offset:name <hex(output)>`; undo the hex pass to get the
        # same shape the port's trace has after its own unescaping.
        traces = []
        for raw in LOGD_RE.findall(text):
            decoded = unescape(raw)
            head, _, tail = decoded.partition(b" ")
            traces.append(head + b" " + dehex_once(tail) if tail else head)
        methods[symbol] = traces
    return methods


def rust_traces(path: str) -> dict:
    """symbol -> list of trace lines, from the generated project.

    Takes the `src/methods/` directory the generator writes (one module per
    top-level class) as well as a single `methods.rs`: the entry points moved out
    of that file, so reading only it would compare the reference against nothing.
    """
    if os.path.isdir(path):
        sources = [
            open(p, "r", encoding="utf-8", errors="surrogateescape")
            for p in sorted(glob.glob(os.path.join(path, "*.rs")))
        ]
    else:
        sources = [open(path, "r", encoding="utf-8", errors="surrogateescape")]
    methods = {}
    symbol = None
    buf = []
    for fp in sources:
        for line in fp:
            match = FN_RE.match(line)
            if match:
                if symbol is not None:
                    methods[symbol] = list(buf)
                symbol = match.group(1)
                buf = []
                continue
            if symbol is None:
                continue
            if line.rstrip("\n") == "}":
                methods[symbol] = list(buf)
                symbol = None
                buf = []
                continue
            for raw in TRACE_RE.findall(line):
                buf.append(dehex_once(unescape(raw)))
    return methods


REG_RE = re.compile(rb"\b[vp]\d+\b")

# `writer.py`'s IR keeps no `dvm_instr` on the moves it synthesises, so its
# `dump()` returns nothing for them; the port keeps the decoded opcode and
# traces it. Drop them from both sides so the comparison is about the real
# instruction stream.
# ...and `writer.py` writes a `goto` with no trace line either, while the port
# traces the fall-through edge it materialises as a state jump.
MOVE_RE = re.compile(rb"^(move(?!-result)|nop|goto)")


def opcode(line: bytes) -> bytes:
    """The opcode name of a normalized trace line."""
    return re.split(rb"[ ,]", line, maxsplit=1)[0]


def keep(line: bytes) -> bool:
    name = opcode(line)
    return bool(name) and not MOVE_RE.match(name) and not line.startswith(b"return")


def normalize(line: bytes) -> bytes:
    """`offset:name operands` -> `name operands` with registers erased.

    The reference prints byte offsets and absolute registers (`v0, v1`), the
    port prints instruction indices and parameter-relative registers
    (`{p0, p1}`) — both are presentation details, so they are stripped to leave
    the decoded opcode plus the members/constants it refers to.
    """
    _, _, rest = line.partition(b":")
    rest = REG_RE.sub(b"", rest)
    rest = re.sub(rb"[\s,]+(?=[,;)])", b"", rest)
    rest = re.sub(rb"\s+", b" ", rest)
    if rest.strip().startswith(b"return"):
        # `writer.py` writes the final `return` without a trace line, so the port
        # having one there is a presentation difference, not a program one.
        return b""
    rest = rest.replace(b"{", b" ").replace(b"}", b" ")
    rest = re.sub(rb"[\s,]+(?=[,;)])", b"", rest)
    rest = re.sub(rb"\s+", b" ", rest)
    return rest.strip()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("nc_dir", help="reference jni/nc directory with the per-method .cpp files")
    parser.add_argument(
        "--rs",
        help="generated src/methods/ directory or methods.rs (Rust port, built with --trace)",
    )
    parser.add_argument("--limit", type=int, default=10, help="how many diffs to print")
    parser.add_argument(
        "--bare",
        action="store_true",
        help="compare opcode names only (register/formatting noise removed)",
    )
    args = parser.parse_args()

    golden = golden_traces(args.nc_dir)
    rust = rust_traces(args.rs)
    print(f"reference methods: {len(golden)}")
    print(f"port methods     : {len(rust)}")

    only_golden = sorted(set(golden) - set(rust))
    only_rust = sorted(set(rust) - set(golden))
    if only_golden:
        print(f"only in reference ({len(only_golden)}): {only_golden[:5]}")
    if only_rust:
        print(f"only in port ({len(only_rust)}): {only_rust[:5]}")

    def reduce_side(lines):
        out = []
        for line in lines:
            norm = normalize(line)
            if not norm or not keep(norm):
                continue
            out.append(opcode(norm) if args.bare else norm)
        return out

    same = 0
    len_diffs = []
    op_diffs = []
    for symbol in sorted(set(golden) & set(rust)):
        a = reduce_side(golden[symbol])
        b = reduce_side(rust[symbol])
        if a == b:
            same += 1
        elif len(a) != len(b):
            len_diffs.append(symbol)
        else:
            op_diffs.append(symbol)

    total = len(set(golden) & set(rust))
    print(f"identical instruction streams: {same}/{total}")
    print(f"different lengths            : {len(len_diffs)}")
    print(f"same length, different ops   : {len(op_diffs)}")

    diffs = len_diffs + op_diffs
    if len_diffs:
        print(f"  length diffs sample: {len_diffs[:5]}")
    for symbol in diffs[: args.limit]:
        a, b = golden[symbol], rust[symbol]
        print(f"\n=== {symbol}")
        print(f"    reference {len(a)} ins, port {len(b)} ins")
        a = reduce_side(a)
        b = reduce_side(b)
        for index in range(max(len(a), len(b))):
            x = a[index] if index < len(a) else b"<missing>"
            y = b[index] if index < len(b) else b"<missing>"
            if x != y:
                mark = "  <- first difference"
                print(f"    [{index}] ref : {x!r}")
                print(f"    [{index}] port: {y!r}{mark}")
                short = min(len(a), index + 4)
                if short > index + 1:
                    print(f"    ref  next: {a[index + 1:short]}")
                    print(f"    port next: {b[index + 1:short]}")
                break
    return 0


if __name__ == "__main__":
    sys.exit(main())
