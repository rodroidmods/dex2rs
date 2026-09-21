#!/usr/bin/env python3
"""Every exception landing pad must take (and clear) the pending exception first.

A converted method that has a `try/catch` gets one negative dispatcher state per
handler. The first thing such an arm must do is `d2c::get_pending_exception` --
`ExceptionOccurred` + `ExceptionClear` -- because *every* JNI call except a
handful fails while an exception is pending. Get that wrong and the handler runs
with the original exception still set: `resolve_class`/`resolve_method` return
failure, the generated `break 'd2c` unwinds to Java, and any retry the app does
repeats the same failing call forever. On a device that is "the job never
finishes", with no crash and no ANR to point at -- the same class of bug as a
dispatcher arm that cannot leave its state, one level down.

Runs over the generated `src/methods/` (or a single `methods.rs`).

Usage:
    python3 tools/exception_check.py target/dist-prod/src/methods
    python3 tools/exception_check.py --selftest
"""

import argparse
import glob
import os
import re
import sys

# `-3 => {` -- a negative state is a landing pad, as in cfg_diff.py.
PAD_RE = re.compile(r"\n            (-?\d+) => \{")
# `pub unsafe extern "system" fn Name(` -- one entry point.
FN_RE = re.compile(r'^pub unsafe extern "system" fn ([A-Za-z0-9_$]+)\(', re.M)
TAKE = "get_pending_exception"


def read_generated(path: str) -> str:
    if os.path.isdir(path):
        return "".join(
            open(p, encoding="utf-8").read()
            for p in sorted(glob.glob(os.path.join(path, "*.rs")))
        )
    return open(path, encoding="utf-8").read()


def bodies(src: str):
    """[(name, body)] for every generated entry point."""
    marks = [(m.start(), m.end(), m.group(1)) for m in FN_RE.finditer(src)]
    out = []
    for i, (_, end, name) in enumerate(marks):
        stop = marks[i + 1][0] if i + 1 < len(marks) else len(src)
        out.append((name, src[end:stop]))
    return out


def first_statement(arm_body: str) -> str:
    for line in arm_body.splitlines():
        if line.strip():
            return line.strip()
    return ""


def check(src: str):
    """[(method, state, first statement)] for every pad that does not take first."""
    problems = []
    methods = pads = 0
    for name, body in bodies(src):
        if "match __state" not in body:
            continue
        methods += 1
        for arm in PAD_RE.finditer(body):
            state = int(arm.group(1))
            if state >= 0:
                continue
            pads += 1
            first = first_statement(body[arm.end() :])
            if TAKE not in first:
                problems.append((name, state, first[:100]))
    return methods, pads, problems


SELFTEST_GOOD = '''pub unsafe extern "system" fn Foo_bar(env: d2c::Env, thiz: d2c::jni_sys::jobject) -> d2c::jni_sys::jobject {
    let mut __state: i32 = 0;
    'd2c: loop {
        match __state {
            0 => {
                __state = -1; continue 'd2c;
            }
            -1 => {
                exception = d2c::get_pending_exception(env);
                __state = 2; continue 'd2c;
            }
            2 => {
                return core::ptr::null_mut();
            }
        }
    }
}
'''

SELFTEST_BAD = SELFTEST_GOOD.replace(
    "                exception = d2c::get_pending_exception(env);\n",
    "                exception = d2c::local_ref(env, v1);\n",
)


def selftest() -> int:
    """A correct pad must pass and a pad that skips the take must be reported."""
    failures = []
    _, pads, problems = check(SELFTEST_GOOD)
    if pads != 1 or problems:
        failures.append(f"a correct landing pad was reported: pads={pads} {problems}")
    _, pads, problems = check(SELFTEST_BAD)
    if pads != 1 or not problems:
        failures.append("a landing pad that does not take the exception was not reported")
    if failures:
        print("exception_check selftest: FAIL")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("exception_check selftest: OK (a pad that skips the take is caught)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("methods", nargs="?", help="generated src/methods/ or methods.rs")
    ap.add_argument("--max-report", type=int, default=15)
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if not args.methods:
        ap.error("a generated methods path is required (or use --selftest)")

    methods, pads, problems = check(read_generated(args.methods))
    print(f"methods with landing pads : {methods}")
    print(f"landing pads              : {pads}")
    print(f"not taking the exception  : {len(problems)}")
    for name, state, first in problems[: args.max_report]:
        print(f"\n  {name} at state {state}")
        print(f"    first statement: {first}")
    # A pass over nothing is the failure mode every one of these checks has had:
    # the method count came from `methods.rs`, which holds no methods any more,
    # and the tool reported a clean run of zero arms.
    if pads == 0:
        print(
            "\nFAIL: no landing pads found -- the path given holds no dispatcher "
            "arms, so this is not a pass"
        )
        return 1
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
