#!/usr/bin/env python3
"""Find paths that leave a generated method while still holding a monitor.

A converted method is a dispatcher: `__state = n; continue 'd2c` jumps to block
`n`, and the two ways out of the method are `return;` and `break 'd2c` (the
`EX_UnwindBlock`, which returns to Java with whatever exception is pending).

Inside a `synchronized` block the compiler emits a `monitor-enter` that only a
`monitor-exit` releases, and it is *path* dependent. If any path reaches an exit
without the matching exit, that monitor stays held for the lifetime of the
process: every other thread that needs it blocks forever, and the app stops
making progress in whatever it was doing. That is a hang, and it is intermittent
by nature -- it needs that particular path to be taken.

This is the failure mode the other checks cannot see. `cfg_diff` compares the
branch/exit skeleton but documents that it cuts exception edges off both sides,
and the arm check only asks whether each arm sets `__state`. Neither would notice
a monitor that is entered on one path and released on another.

Usage:
    python3 tools/monitor_check.py src/methods/
"""

import argparse
import glob
import os
import re
import sys

FN_RE = re.compile(r'^pub unsafe extern "system" fn (\w+)\(', re.M)
ARM_RE = re.compile(r"^\s*(-?\d+) => \{$", re.M)
ENTER_RE = re.compile(r"d2c::monitor_enter\(env, (\w+)\)")
EXIT_RE = re.compile(r"d2c::monitor_exit\(env, (\w+)\)")
GOTO_RE = re.compile(r"__state = (-?\d+); continue 'd2c;")
BREAK_RE = re.compile(r"break 'd2c;")
RETURN_RE = re.compile(r"(?m)^[ \t]*return;[ \t]*$")


def split_arms(body: str):
    """state number -> the text of its arm, in source order."""
    hits = [(m.start(), int(m.group(1))) for m in ARM_RE.finditer(body)]
    arms = {}
    for i, (start, state) in enumerate(hits):
        end = hits[i + 1][0] if i + 1 < len(hits) else len(body)
        arms[state] = body[start:end]
    return arms


def events(arm: str):
    """The ordered (kind, operand) events inside one arm.

    Collected per pattern and sorted by position rather than matched with one
    alternation: the four shapes are anchored differently and `(?m)` cannot be
    turned on part-way through a pattern.
    """
    found = []
    for match in ENTER_RE.finditer(arm):
        found.append((match.start(), "enter", match.group(1)))
    for match in EXIT_RE.finditer(arm):
        found.append((match.start(), "exit", match.group(1)))
    for match in GOTO_RE.finditer(arm):
        found.append((match.start(), "goto", int(match.group(1))))
    for match in BREAK_RE.finditer(arm):
        found.append((match.start(), "break", None))
    for match in RETURN_RE.finditer(arm):
        found.append((match.start(), "return", None))
    found.sort(key=lambda item: item[0])
    return [(kind, operand) for _pos, kind, operand in found]


def check_method(name: str, body: str):
    """Every exit reachable with a monitor held, as (state, monitor, exit kind)."""
    arms = split_arms(body)
    if not arms:
        return []
    problems = []
    # A state can be reached with different monitors held, so keep the set of
    # held-sets per state. Bounded: monitor nesting in real code is shallow.
    seen = {}
    queue = [(0, frozenset())]
    while queue:
        state, held = queue.pop()
        if state not in arms:
            continue
        if seen.get(state, set()) and held in seen[state]:
            continue
        seen.setdefault(state, set()).add(held)
        for kind, operand in events(arms[state]):
            if kind == "enter":
                held = held | {operand}
            elif kind == "exit":
                held = held - {operand}
            elif kind == "goto":
                queue.append((operand, held))
                break
            elif kind in ("break", "return"):
                if held:
                    problems.append((state, sorted(held), kind))
                break
    return problems


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("sources", help="generated src/methods/ directory or a methods.rs")
    ap.add_argument("--max-report", type=int, default=15)
    args = ap.parse_args()

    if os.path.isdir(args.sources):
        paths = sorted(glob.glob(os.path.join(args.sources, "*.rs")))
    else:
        paths = [args.sources]
    if not paths:
        print(f"FAIL: nothing to read in {args.sources}", file=sys.stderr)
        return 2

    methods = 0
    with_monitor = 0
    bad = []
    for path in paths:
        text = open(path, "r", encoding="utf-8", errors="replace").read()
        marks = [(m.start(), m.group(1)) for m in FN_RE.finditer(text)]
        for i, (start, name) in enumerate(marks):
            end = marks[i + 1][0] if i + 1 < len(marks) else len(text)
            body = text[start:end]
            methods += 1
            if "monitor_enter" not in body:
                continue
            with_monitor += 1
            for state, held, kind in check_method(name, body):
                bad.append((name, state, held, kind))

    print(f"methods                : {methods}")
    print(f"methods with a monitor : {with_monitor}")
    print(f"exits holding a monitor: {len(bad)}")
    for name, state, held, kind in bad[: args.max_report]:
        print(f"\n  {name}")
        print(f"    state {state}, {kind} with {', '.join(held)} still held")
    if len(bad) > args.max_report:
        print(f"\n  ... and {len(bad) - args.max_report} more")

    # Reporting "0" for a tree that has no monitors at all would be a pass the
    # tool did not earn, which is the one thing it must never do.
    if with_monitor == 0:
        print(
            "\nFAIL: no method in this tree enters a monitor, so nothing was "
            "checked -- the sources are probably not the generated ones"
        )
        return 1
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
