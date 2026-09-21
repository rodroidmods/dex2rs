#!/usr/bin/env python3
"""Verify the generated dispatcher cannot spin.

Each generated method is a state machine:

    LOOP: loop {
        match __state {
            3 => {
                ...instructions...
                __state = 7; continue 'd2c;   <- must run on every path
            }
            ...
        }
    }

If an arm can reach its closing brace without setting `__state` (or returning /
breaking out), the `loop` re-enters the same arm and the method spins forever.
That is exactly the bug that hung `smali/n;->invoke`: a block whose last
instruction was `packed-switch` was treated as a terminator, so the DEX
fall-through was never emitted and the same two instructions ran forever.

The check works on the real brace structure: for every arm of every
`match __state`, the last top-level statement must be a terminator, or an
`if/else` whose *both* branches terminate. A trailing `if cond { ... }` with no
`else` is a fall-through and gets reported.

Usage: python3 tools/arm_check.py <generated-src-dir-or-file> [...]
"""

import os
import re
import sys

TERMINATORS = (
    re.compile(r"^__state\s*=\s*-?\d+\s*;\s*continue\b"),
    re.compile(r"^return(\s|;|$)"),
    re.compile(r"^break(\s|;|$)"),
    re.compile(r"^d2c::abort\("),
    re.compile(r"^std::process::abort\("),
)

ARM_RE = re.compile(r"^(\s*)(-?\d+)\s*=>\s*\x01")


def mask(original: str) -> str:
    """Blank out string/char literal contents and Rust lifetimes, so brace and
    keyword scanning is not confused by `'d2c` lifetimes or string data."""
    out = []
    j = 0
    n = len(original)
    while j < n:
        c = original[j]
        if c == "/" and j + 1 < n and original[j + 1] == "/":
            out.append(" " * (n - j))
            break
        if c == '"':
            out.append(" ")
            j += 1
            while j < n:
                if original[j] == "\\":
                    out.append("  ")
                    j += 2
                    continue
                if original[j] == '"':
                    out.append(" ")
                    j += 1
                    break
                out.append(" ")
                j += 1
            continue
        if c == "'":
            # char literal: 'x' or '\n'
            m = re.match(r"'(?:\\.|[^'\\])'", original[j:])
            if m:
                out.append(" " * len(m.group(0)))
                j += len(m.group(0))
                continue
            # lifetime ('d2c): no braces inside, just drop it
            m = re.match(r"'[A-Za-z_][A-Za-z0-9_]*", original[j:])
            if m:
                out.append(" " * len(m.group(0)))
                j += len(m.group(0))
                continue
            out.append(" ")
            j += 1
            continue
        if c == "{":
            out.append("\x01")
        elif c == "}":
            out.append("\x02")
        else:
            out.append(c)
        j += 1
    return "".join(out)


def close_brace(masked, start_idx, start_col):
    """Index/col of the `}` matching the `{` at (start_idx, start_col)."""
    depth = 0
    for i in range(start_idx, len(masked)):
        line = masked[i]
        j = start_col if i == start_idx else 0
        while j < len(line):
            if line[j] == "\x01":
                depth += 1
            elif line[j] == "\x02":
                depth -= 1
                if depth == 0:
                    return i, j
            j += 1
    return None, None


def top_level_statements(masked):
    """Split a body into top-level statements as (first_line, last_line)."""
    stmts = []
    start = None
    depth = 0
    for i, line in enumerate(masked):
        if start is None:
            if not line.strip():
                continue
            start = i
        depth += line.count("\x01") - line.count("\x02")
        finished = depth <= 0 and line.rstrip().endswith((";", "\x02"))
        if finished:
            stmts.append((start, i))
            start = None
            depth = 0
    if start is not None:
        stmts.append((start, len(masked) - 1))
    return stmts


def body_terminates(original, masked) -> bool:
    stmts = top_level_statements(masked)
    if not stmts:
        return False
    a, b = stmts[-1]
    text = " ".join(
        masked[k].strip() for k in range(a, b + 1) if masked[k].strip()
    )
    # Use the original for keyword matching so `break 'd2c;` still matches.
    raw_text = " ".join(
        original[k].strip() for k in range(a, b + 1) if original[k].strip()
    )
    if any(t.match(text.strip()) for t in TERMINATORS):
        return True
    if any(t.match(raw_text.strip()) for t in TERMINATORS):
        return True

    head = masked[a].strip()
    if head.startswith("if ") and head.endswith("\x01"):
        # locate the `} else {` that closes this if's then-branch
        else_idx = None
        for k in range(a + 1, b + 1):
            if re.match(r"^\x02\s*else\s*\x01$", masked[k].strip()):
                else_idx = k
                break
        if else_idx is None:
            return False
        then_body = masked[a + 1 : else_idx]
        then_orig = original[a + 1 : else_idx]
        close = b
        while close > else_idx and masked[close].strip() in ("\x02", ""):
            close -= 1
        else_body = masked[else_idx + 1 : close + 1]
        else_orig = original[else_idx + 1 : close + 1]
        return body_terminates(then_orig, then_body) and body_terminates(
            else_orig, else_body
        )
    return False


FN_RE = re.compile(r'^\s*pub unsafe extern "system" fn (\w+)')
STATE_SET_RE = re.compile(r"__state\s*=\s*(-?\d+)\s*;")


def find_arms(lines):
    """Yield (method, state, body, masked_body) for every dispatcher arm."""
    masked = [mask(l) for l in lines]
    method = "?"
    i = 0
    while i < len(masked):
        fn = FN_RE.match(masked[i])
        if fn:
            method = fn.group(1)
        m = ARM_RE.match(masked[i])
        if not m:
            i += 1
            continue
        col = masked[i].index("\x01", m.end() - 1)
        end, _ = close_brace(masked, i, col)
        if end is None:
            i += 1
            continue
        yield method, int(m.group(2)), lines[i + 1 : end], masked[i + 1 : end]
        i += 1


def check_file(path):
    """Return (fall_through_arms, dangling_targets, arm_count) for one file.

    * fall-through arms: an arm that can reach its closing brace without
      setting `__state`, returning or breaking. Hitting one at runtime means the
      dispatcher's `loop` re-enters the same state forever.
    * dangling targets: an arm that jumps to a state no arm of the same method
      defines. The `_ =>` fallback catches it, but it means the emitter computed
      a wrong block number.
    * arm count: how many arms were actually inspected, so a run that found
      nothing to look at can be told apart from a clean one.
    """
    try:
        lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
    except OSError:
        return [], [], 0
    if not any("__state" in l for l in lines):
        return [], [], 0

    arms = list(find_arms(lines))
    defined = {}
    for method, state, _body, _mbody in arms:
        defined.setdefault(method, set()).add(state)

    fall_through = []
    dangling = []
    for method, state, body, mbody in arms:
        if not body_terminates(body, mbody):
            fall_through.append((method, state, body))
        known = defined.get(method, set())
        for target in STATE_SET_RE.findall("\n".join(mbody)):
            target = int(target)
            # Landing pads are numbered `-(i + 1)` and are emitted after the
            # blocks; they are in `defined` too, so any miss is a real bug.
            if target not in known:
                dangling.append((method, state, target))
    return fall_through, dangling, len(arms)


def main(argv):
    roots = argv[1:] or ["target/fixed2/src"]
    files = []
    for root in roots:
        if os.path.isfile(root):
            files.append(root)
        else:
            for dirpath, _, names in os.walk(root):
                files += [os.path.join(dirpath, n) for n in names if n.endswith(".rs")]

    bad = 0
    total_arms = 0
    for f in sorted(files):
        problems, dangling, arms = check_file(f)
        total_arms += arms
        if problems or dangling:
            bad += 1
        if problems:
            print(
                f"!! {f}: {len(problems)} arm(s) can fall through to the loop head "
                f"(the dispatcher would re-enter them forever)"
            )
            for method, state, body in problems[:5]:
                print(f"   {method}  state {state}, tail:")
                for ln in [l for l in body if l.strip()][-6:]:
                    print("     " + ln.rstrip())
        if dangling:
            print(f"!! {f}: {len(dangling)} jump(s) to a state that has no arm")
            for method, state, target in dangling[:5]:
                print(f"   {method}: state {state} jumps to {target}, which is not an arm")
    print(f"checked {len(files)} files, {total_arms} dispatcher arms, {bad} with problems")

    # Pointed at a path that no longer holds the generated code (the generator
    # moved the entry points out of `methods.rs` into `methods/`), this used to
    # report "checked 1 files, 0 with problems" -- indistinguishable from a clean
    # run of every arm we have. Finding nothing to check is a failure of the
    # check, not a pass.
    if total_arms == 0:
        print(
            "\nFAIL: no dispatcher arms found -- pass the generated `methods/` "
            "directory (or a module inside it), not the re-exporting `methods.rs`"
        )
        return 1
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
