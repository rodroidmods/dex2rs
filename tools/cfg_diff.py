#!/usr/bin/env python3
"""Differential control-flow check: Rust port vs the reference Python d2c.

Every bug found so far on a real device had the same shape -- a block that left
its state through a *different edge* than the reference:

  * `packed-switch` with no matching case: the reference falls through to the
    next C label, the port's dispatcher arm ended and re-ran itself -> infinite
    loop (grey screen / APP_SCOUT_HANG).
  * a `throw` in the method's last block: the reference falls out of the block
    into `EX_UnwindBlock` and returns, the port re-ran the arm -> infinite loop.

The trace-level diff (`tools/compare_golden.py`) compares the *decoded*
instruction stream. It cannot see a missing edge, because the decoded stream is
identical in both cases -- the edge is what the emitter does with it. This tool
compares the edges.

Both sides are read as a graph of blocks and normalised the same way before the
comparison, so that a difference in *block splitting* is not reported as a bug:

  1. blocks a compiler splits only to hoist a trailing `return` (the port always
     does this) are contracted, as are chains of single-successor blocks. What
     remains is the decision skeleton: only blocks that branch or leave the
     method are compared.
  2. the reference's landing pads (`EX_LandingPad_N:` / `EX_UnwindBlock:`) are
     cut off with the exception edges that target them; the port's equivalents
     are its negative dispatcher states. Exception edges are per-instruction and
     belong to the surrounding try/catch, so they are not compared.

What is compared, per method, is therefore the full branch/exit skeleton: how
many decisions there are, and where each one goes. A missing edge -- the bug
class above -- shows up immediately.

Usage:
    python3 tools/cfg_diff.py GOLDEN_DIR METHODS_RS
    python3 tools/cfg_diff.py /tmp/goldenref/jni/nc target/guard/src/methods.rs
"""

import argparse
import glob
import os
import re
import sys

# `L13:` at the start of a line (the reference's block labels).
REF_LABEL_RE = re.compile(r"^L(\d+):", re.M)
# `goto L13;` -- never `goto EX_HANDLE;`, which is an exception edge.
REF_GOTO_RE = re.compile(r"\bgoto L(\d+);")
REF_RETURN_RE = re.compile(r"^\s*return\b", re.M)
# Everything from the first landing pad on is exception handling, not a block.
REF_PAD_RE = re.compile(r"^EX_(?:LandingPad_\d+|UnwindBlock):", re.M)
# `0 => {` -- one dispatcher arm, indented under `match __state {`.
PORT_ARM_RE = re.compile(r"\n            (-?\d+) => \{")
# `__state = 7; continue 'd2c;`
PORT_JUMP_RE = re.compile(r"__state = (-?\d+); continue 'd2c;")
PORT_FN_RE = re.compile(r'^pub unsafe extern "system" fn ([A-Za-z0-9_$]+)\(', re.M)

# Literals and comments, in the generated Rust. Needed before anything counts
# braces, because `--obfuscate` emits ciphertext bytes *raw* when they are
# printable -- and a ciphertext byte can be `{` or `}` (0x7b/0x7d, one in 128
# each). Counting those as code truncates the dispatcher arm and the tool then
# reports control-flow differences that do not exist: on a real app that was 41
# phantom mismatches out of 3727 methods, every one of them an obfuscated build.
# A checker that invents bugs is worse than no checker, so the literals are
# blanked (length-preserving, so every other regex still sees the same offsets).
LITERAL_RE = re.compile(
    r'(?:b?)"(?:\\.|[^"\\])*"'  # "..." and b"..."
    r"|'(?:\\.|[^'\\])'"  # a real char literal; `'d2c` (a lifetime) is not one
    r"|//[^\n]*"  # line comment
    r"|/\*(?:.|\n)*?\*/",  # block comment
    re.S,
)


def blank_literals(text: str) -> str:
    """Same text, same length, with literal/comment contents replaced by spaces.

    Delimiters are kept so a line that is nothing but a literal still reads as a
    line, and so the byte offsets the other regexes work on do not move.
    """

    def repl(m: re.Match) -> str:
        s = m.group(0)
        if s.startswith("/"):  # comment: none of it is code
            return " " * len(s)
        if len(s) <= 1:
            return " " * len(s)
        return s[0] + " " * (len(s) - 2) + s[-1]

    return LITERAL_RE.sub(repl, text)


def ref_blocks(text: str):
    """Ordered [(label, {successor labels}, exits)] for one reference method."""
    text = blank_literals(text)
    pad = REF_PAD_RE.search(text)
    if pad:
        text = text[: pad.start()]
    labels = [int(m.group(1)) for m in REF_LABEL_RE.finditer(text)]
    starts = [m.end() for m in REF_LABEL_RE.finditer(text)]
    blocks = []
    for i, label in enumerate(labels):
        start = starts[i]
        end = text.find(f"\nL{labels[i + 1]}:", start) if i + 1 < len(labels) else len(text)
        body = text[start:end]
        succ = {int(m.group(1)) for m in REF_GOTO_RE.finditer(body)}
        exits = bool(REF_RETURN_RE.search(body))
        # A block whose last statement is neither a `goto`, a `return` nor an
        # unwind drops through to the physically next label.
        tail = body.rstrip()
        unwinds = tail.endswith("D2C_GOTO_UNWINDBLOCK;")
        if unwinds:
            exits = True
        falls_through = not (
            unwinds
            or re.search(r"goto L\d+;$", tail)
            or (tail.endswith(";") and re.search(r"return[^;]*;$", tail))
        )
        if falls_through and i + 1 < len(labels):
            succ.add(labels[i + 1])
        blocks.append([label, succ, exits])
    return blocks


def port_arms(text: str):
    """Ordered [[state, {successor states}, exits]] for one port method."""
    text = blank_literals(text)
    i = text.find("match __state {")
    if i < 0:
        return None
    parts = PORT_ARM_RE.split(text[i:])
    arms = []
    for k in range(1, len(parts) - 1, 2):
        state = int(parts[k])
        if state < 0:
            continue  # exception landing pad: an edge, not a block
        code = parts[k + 1]
        depth = 1
        cut = len(code)
        for idx, ch in enumerate(code):
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    cut = idx
                    break
        body = code[:cut]
        # Negative targets are exception edges (a landing pad).
        succ = {int(m.group(1)) for m in PORT_JUMP_RE.finditer(body)}
        succ = {s for s in succ if s >= 0}
        # The arm leaves the method only if its *last* statement does. Every
        # `write_check_pending` guard in the middle ends with `} }`, so it cannot
        # be mistaken for an exit, whichever JNI call it wraps.
        last = [line.strip() for line in body.split("\n") if line.strip()][-1]
        exits = last.startswith("return") or "break 'd2c;" in last
        arms.append([state, succ, exits])
    return arms



def contract(blocks):
    """Splice out single-successor, non-exiting blocks until none are left.

    A compiler is free to split an edge into a chain of transparent blocks (the
    port always hoists a trailing `return` into its own block); this removes
    that freedom so only the decision skeleton is compared. A self-loop is never
    contracted -- it is a real loop and must stay visible.
    """
    by_id = {b[0]: b for b in blocks}
    order = [b[0] for b in blocks]
    changed = True
    while changed:
        changed = False
        for bid in list(order):
            if bid not in by_id:
                continue
            succ, exits = by_id[bid][1], by_id[bid][2]
            if exits or len(succ) != 1:
                continue
            target = next(iter(succ))
            if target == bid or target not in by_id:
                continue
            # Splice: every edge into `bid` now goes to `target`.
            for other in by_id.values():
                if bid in other[1]:
                    other[1].discard(bid)
                    other[1].add(target)
            del by_id[bid]
            order.remove(bid)
            changed = True
    return [by_id[bid] for bid in order if bid in by_id]


def compare_method(ref_text: str, port_text: str):
    ref = contract(ref_blocks(ref_text))
    arms = port_arms(port_text)
    if arms is None:
        return ["port method has no dispatcher"]
    arms = contract(arms)

    problems = []
    if len(arms) != len(ref):
        problems.append(
            f"decision-block count {len(arms)} != reference {len(ref)}"
            f" (port states {[a[0] for a in arms][:12]},"
            f" ref labels {[b[0] for b in ref][:12]})"
        )
        return problems

    # Both sides are compared in *contracted index* space, so that a state
    # number (or label) that got spliced out never appears in the diff.
    ref_index = {b[0]: i for i, b in enumerate(ref)}
    port_index = {a[0]: i for i, a in enumerate(arms)}
    for (label, ref_succ, _), (state, port_succ, _) in zip(ref, arms):
        want = {ref_index[s] for s in ref_succ if s in ref_index}
        got = {port_index[s] for s in port_succ if s in port_index}
        if want != got:
            problems.append(
                f"block {state}: edges {sorted(got)} != reference "
                f"{sorted(want)} (L{label} -> {sorted(ref_succ)})"
            )
            continue
        if False:
            pass
    return problems


def read_generated(path: str) -> str:
    """All generated entry points, in either layout.

    The generator writes one module per top-level class under `src/methods/`
    (and `methods.rs` as the root that re-exports them); older runs kept every
    entry point in `methods.rs` alone. Accepting a directory as well as a file
    keeps this comparison from silently looking at the one file that no longer
    holds any methods.
    """
    if os.path.isdir(path):
        return "".join(
            open(p, encoding="utf-8").read()
            for p in sorted(glob.glob(os.path.join(path, "*.rs")))
        )
    return open(path, encoding="utf-8").read()


SELFTEST_REF = """Code:
L0:
    if (v0) goto L2;
    goto L3;
L2:
    return NULL;
L3:
    return NULL;
"""

# The port side of the same method. Two things in it must not be read as code:
# a ciphertext literal with raw `{` / `}` in it (which the real generator emits,
# because a printable ciphertext byte is written raw), and a literal that spells
# a jump (`continue 'd2c;`) -- the phrase the edge check searches for. Reading
# either one as code is what produced 41 phantom mismatches on a real build.
SELFTEST_PORT = '''pub unsafe extern "system" fn Foo_bar(env: d2c::Env, thiz: d2c::jni_sys::jobject) -> d2c::jni_sys::jobject {
    let mut __state: i32 = 0;
    'd2c: loop {
        match __state {
            0 => {
                v1 = d2c::new_string_utf_obf_seeded(env, b"\\x7b}\\x02{", 0x1);
                v2 = d2c::new_string_utf(env, b"__state = 9; continue 'd2c;");
                if x { __state = 2; continue 'd2c; }
                __state = 3; continue 'd2c;
            }
            2 => {
                return core::ptr::null_mut();
            }
            3 => {
                return core::ptr::null_mut();
            }
            _ => {
                return core::ptr::null_mut();
            }
        }
    }
}
'''


def selftest() -> int:
    """Pin the parser: literals are not code, and a real missing edge is caught.

    Both halves matter. The first half is the bug this file just had -- an
    obfuscated literal containing `{`/`}` truncated an arm and invented a
    mismatch. The second half proves the fix did not make the tool blind: an arm
    that loses its second edge must still be reported, because that is the
    hang-on-device bug class this tool exists for.
    """
    failures = []

    ok = compare_method(SELFTEST_REF, SELFTEST_PORT)
    if ok:
        failures.append(f"literal-aware parse: expected no problems, got {ok}")

    # Same method, with the `__state = 3` edge deleted from arm 0. That is the
    # packed-switch/fall-through bug: one edge gone.
    broken = SELFTEST_PORT.replace("                __state = 3; continue 'd2c;\n", "")
    problems = compare_method(SELFTEST_REF, broken)
    if not problems:
        failures.append("missing edge: arm 0 lost an edge and it was not reported")

    # And the jump phrase inside a literal must not create an edge to state 9.
    arms = port_arms(SELFTEST_PORT)
    if arms is None or 9 in {s for a in arms for s in a[1]}:
        failures.append("literal-aware parse: a literal spelled a jump and it was taken")

    if failures:
        print("cfg_diff selftest: FAIL")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("cfg_diff selftest: OK (literals blanked, missing edge still caught)")
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("golden_dir", nargs="?", help="reference output dir (jni/nc/*.cpp)")
    ap.add_argument(
        "methods_rs",
        nargs="?",
        help="generated src/methods.rs or the src/methods/ directory",
    )
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--selftest", action="store_true", help="test this tool's parser")
    ap.add_argument("--max-report", type=int, default=10)
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if not args.golden_dir or not args.methods_rs:
        ap.error("golden_dir and methods_rs are required (or use --selftest)")

    golden = {}
    # A JNI long name reaches 505 characters on a real app, past the 255-byte
    # filesystem limit, so a reference tree that covers every method cannot name
    # its files after the method. `index.tsv` (file -> method) is how such a tree
    # says which method each file holds; without it the file name is the name.
    index_path = os.path.join(args.golden_dir, "index.tsv")
    names = {}
    if os.path.exists(index_path):
        with open(index_path, encoding="utf-8") as fp:
            for line in fp:
                name, _, method = line.rstrip("\n").partition("\t")
                if name and method:
                    names[name + ".cpp"] = method
    for path in glob.glob(os.path.join(args.golden_dir, "*.cpp")):
        base = os.path.basename(path)
        golden[names.get(base, base[:-4])] = open(path, encoding="utf-8").read()

    src = read_generated(args.methods_rs)
    marks = [(m.start(), m.group(1)) for m in PORT_FN_RE.finditer(src)]
    port = {}
    for i, (start, name) in enumerate(marks):
        end = marks[i + 1][0] if i + 1 < len(marks) else len(src)
        port[name] = src[start:end]

    checked = 0
    failed = []
    missing = []
    for name, port_text in port.items():
        ref_text = golden.get(name)
        if ref_text is None:
            missing.append(name)
            continue
        if "match __state" not in port_text:
            continue
        checked += 1
        problems = compare_method(ref_text, port_text)
        if problems:
            failed.append((name, problems))

    print(f"golden methods : {len(golden)}")
    print(f"port methods   : {len(port)}")
    print(f"compared       : {checked}")
    print(f"not in golden  : {len(missing)}")
    print(f"CFG mismatches : {len(failed)}")
    for name, problems in failed[: args.max_report]:
        print(f"\n  {name}")
        for p in problems[:6]:
            print(f"    - {p}")
    if len(failed) > args.max_report:
        print(f"\n  ... and {len(failed) - args.max_report} more")
    if args.verbose:
        # All of them, not the first few: these are precisely the methods whose
        # control flow nothing here can check, so when a hang turns up this list
        # is the first place to look.
        print(f"\n  no reference for {len(missing)} method(s):")
        for name in missing:
            print(f"    {name}")

    # A vacuous pass is worse than a failure: this tool's whole value is that a
    # wrong reference tree or a mis-parsed side makes it say so. Pointed at an
    # empty directory it used to print "CFG mismatches : 0" and exit 0, which
    # reads exactly like a clean comparison of every method.
    if not golden:
        print(
            f"\nFAIL: no reference methods in {args.golden_dir} "
            "(expected the reference tool's jni/nc/*.cpp)"
        )
        return 1
    if checked == 0:
        print(
            "\nFAIL: nothing could be compared -- no name matched between the "
            "reference and the generated methods.rs"
        )
        return 1
    if len(missing) > len(port) // 2:
        print(
            f"\nFAIL: {len(missing)} of {len(port)} generated methods have no "
            "reference counterpart, so the comparison is not meaningful"
        )
        return 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
