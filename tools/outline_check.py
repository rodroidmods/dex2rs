#!/usr/bin/env python3
"""Measure what LLVM's machine outliner did to a built library.

The size story for `--outline-common` and `opt-level z` is "the outliner removed
duplicated code", which is only checkable by counting. Reading the numbers off
`readelf --debug-dump=frames` by hand is how this repository once published a
function count that was wrong by 8x, so the numbers come from here instead.

    python3 tools/outline_check.py libstub.so
    python3 tools/outline_check.py libstub.so --threshold 64 --top 12

Reports, for one `.so`:

  * functions and instructions in `.text` (function sizes from `.eh_frame` FDEs)
  * fragments at or under the threshold: how many, and what share of `.text`
  * the `bl` sites that call them -- the duplication the outliner collapsed
  * fragments nothing calls (dead fragments, if the count is not ~0)
  * the hottest fragments, disassembled, so the report says *what* was copied

A fragment count in the tens of thousands with a 5-8000 call count per fragment
is the outliner at work (expected at `opt-level z` and with `--outline-common`).
A low count means the build has outlining off (`opt-level 3`), and a large
"never called" count means the linker kept fragments nothing reaches.

Needs `readelf` and a disassembler (`llvm-objdump`, or `objdump`); it exits 1
with a message if either is missing rather than reporting nothing.
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict

FDE_RE = re.compile(r"FDE cie=\S+ pc=([0-9a-f]+)\.\.([0-9a-f]+)")
INS_RE = re.compile(r"^\s*([0-9a-f]+):\s+(?:[0-9a-f]{2,8}\s+)?(\S+)\s*(.*)$")
BRANCHES = {"bl", "b"}


def which(*names: str) -> str | None:
    for name in names:
        if (found := shutil.which(name)):
            return found
    return None


def run(cmd: list[str]) -> str:
    return subprocess.run(
        cmd, capture_output=True, text=True, errors="replace"
    ).stdout


def functions(readelf: str, so: str) -> list[tuple[int, int]]:
    """(start, size) for every function with unwind info, sorted by address."""
    frames = run([readelf, "-W", "--debug-dump=frames", so])
    out = []
    for line in frames.splitlines():
        if "FDE cie=" in line and (m := FDE_RE.search(line)):
            start, end = int(m.group(1), 16), int(m.group(2), 16)
            if end > start:
                out.append((start, end - start))
    return sorted(out)


def disassemble(dis: str, so: str) -> str:
    with tempfile.TemporaryFile(mode="w+") as tmp:
        subprocess.run([dis, "-d", so], stdout=tmp, stderr=subprocess.DEVNULL,
                       text=True)
        tmp.seek(0)
        return tmp.read()


def operand_target(operand: str) -> int | None:
    if (m := re.match(r"0x([0-9a-f]+)", operand.strip())):
        return int(m.group(1), 16)
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("library", help="a built libstub.so (or any ELF)")
    ap.add_argument("--threshold", type=int, default=32,
                    help="largest size in bytes still called a fragment (default 32)")
    ap.add_argument("--top", type=int, default=8,
                    help="how many hot fragments to disassemble (default 8)")
    args = ap.parse_args()

    if not (readelf := which("readelf", "llvm-readelf")):
        print("outline_check: readelf not found (binutils or llvm)", file=sys.stderr)
        return 1

    dis = which("llvm-objdump", "objdump")
    if not dis:
        print("outline_check: no disassembler found (llvm-objdump or objdump)",
              file=sys.stderr)
        return 1

    funcs = functions(readelf, args.library)
    if not funcs:
        print(f"outline_check: no .eh_frame FDEs in {args.library} "
              "(stripped of unwind info?)", file=sys.stderr)
        return 1

    starts = {start: size for start, size in funcs}
    fragments = {start for start, size in funcs if size <= args.threshold}
    asm = disassemble(dis, args.library)

    bounds = sorted(starts)
    total = frag_ins = 0
    calls: Counter[int] = Counter()
    bodies: dict[int, list[str]] = defaultdict(list)

    def owner(addr: int) -> tuple[int, int] | None:
        lo, hi, best = 0, len(bounds) - 1, None
        while lo <= hi:
            mid = (lo + hi) // 2
            if bounds[mid] <= addr:
                best = (bounds[mid], starts[bounds[mid]])
                lo = mid + 1
            else:
                hi = mid - 1
        return best

    for line in asm.splitlines():
        m = INS_RE.match(line)
        if not m:
            continue
        addr, mnemonic = int(m.group(1), 16), m.group(2)
        total += 1
        here = owner(addr)
        if here and here[0] in fragments:
            frag_ins += 1
            bodies[here[0]].append(f"{mnemonic} {m.group(3).split('<')[0].strip()}")
        if mnemonic == "bl" and (target := operand_target(m.group(3))) in fragments:
            calls[target] += 1

    dead = len(fragments) - len(calls)
    per_frag = sorted(calls.values())
    pct = frag_ins / total * 100 if total else 0.0
    share = len(fragments) / len(funcs) * 100 if funcs else 0.0

    print(f"{args.library}")
    print(f"  functions        {len(funcs):>10,}")
    print(f"  .text instructions {total:>9,}")
    print(f"  fragments (<={args.threshold}B) {len(fragments):>7,}  "
          f"({share:.1f}% of functions)")
    print(f"  instructions in them {frag_ins:>6,}  ({pct:.2f}% of .text)")
    print(f"  bl sites into them   {sum(calls.values()):>6,}")
    print(f"  never called         {dead:>6,}", end="")
    if dead > len(fragments) * 0.05 and fragments:
        print("  <- more than 5%: dead fragments, or a fragment set this "
              "threshold is wrong for")
    else:
        print()

    if per_frag:
        n = len(per_frag)
        print(f"  calls per fragment: min={per_frag[0]} p50={per_frag[n // 2]} "
              f"p90={per_frag[int(n * 0.9)]} max={per_frag[-1]}")

    if len(fragments) < 2:
        print("\n  No outlining: this is what `opt-level 3` without "
              "`--outline-common` looks like.")
        return 0

    if args.top:
        print(f"\n  {min(args.top, len(calls))} hottest fragments:")
        for target, count in calls.most_common(args.top):
            body = " ; ".join(bodies.get(target, [])[:6])
            print(f"    {starts[target]:>3}B  x{count:<6} {target:#x}: {body}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
