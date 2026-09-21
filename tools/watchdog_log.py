#!/usr/bin/env python3
"""Turn the watchdog's log lines into a verdict, against the generated source.

The runtime names a `(method, state)` when a converted method stops making
progress, which is the only way a freeze inside converted code can be seen on a
device (no crash, no ANR, no tombstone). The line on its own is still a
`(method, state)` pair, and the state has to be looked up in the generated Rust
to mean anything -- that lookup is this tool, so nobody has to read a dispatcher
by hand again.

Usage:
    adb logcat -d | python3 tools/watchdog_log.py target/dist-plain
    python3 tools/watchdog_log.py target/dist-plain logcat.txt

`SOURCE_DIR` is the generated project (`--source-dir` of the conversion) or its
`src/` or `src/methods/`. Report lines are matched anywhere in the input, so the
whole logcat can be piped in unfiltered.

For each distinct report it prints the arm that state owns and classifies it:

    IMPOSSIBLE   the arm is a bare `return`, so the dispatcher cannot run it
                 again. The build predates the `d2c::watch()` guard, which hands
                 the slot back when a method returns: the slot was left at the
                 state the method *ended* in. Not evidence of a freeze.
    STALE        the log carries `FROZEN`, which only builds from before the
                 sampler stopped guessing emit. It once meant "spinning", but a
                 sampler cannot tell that from a method the app calls over and
                 over whose slow state is always the same, and on a real build it
                 named a state whose arm is a bare `return`. Re-capture with a
                 current build.
    UNREACHABLE  the method named is not in the tree (wrong tree, or the report
                 came from a different APK).
    candidate    a state that really can be parked or spun in; the arm's Java
                 calls are listed, since one of them is where the thread is.

The two line kinds a current build emits are `PARKED` (the sampler: this frame is
inside that method and has made no transition for N seconds, so it is waiting in a
call entered from that block) and `FREEZE` (the method's own guard: that arm
re-enters itself without leaving, which is a conversion bug). Builds from before
the rename logged the sampler line as `STUCK`; it is read as the same kind.

Only `FREEZE` is a bug report -- it is logged at `error`. `PARKED` is logged at
`debug` and is a stopwatch, *not* a bug report: a method waiting on the app's own
native library is parked in exactly that state for as long as the native side
takes, and from inside the runtime a 90-second metadata pass is indistinguishable
from a call that never returns. So for each `PARKED` series this prints which of
the two it is, from the series rather than from any one line: the seconds reset to
5 whenever the frame transitions, so a stage that is working climbs *and resets*,
while a frame that is hung climbs and never resets. Even that is not on its own a
verdict -- a build in the wild climbed to 27 s in a native stage and then finished
-- so a series that ends at its peak is only called a hang when nothing else
reports after it.

`--selftest` runs the classifier against a fixture tree and checks it tells the
cases apart, which is the property that makes the rest of the output worth
reading.
"""

import glob
import os
import re
import sys
import tempfile

REPORT = re.compile(r"dex2rs: (FROZEN|PARKED|STUCK) (\d+)s in (\S+) at state (-?\d+)")
FREEZE = re.compile(r"dex2rs: FREEZE in (\S+) at state (-?\d+)")
ARM = re.compile(r"\n            (-?\d+) => \{")
FN = re.compile(r'^pub unsafe extern "system" fn ([A-Za-z0-9_$]+)\(', re.M)
# Logcat prefixes every line with `MM-DD HH:MM:SS.mmm`, which compares
# lexicographically in order.
TS = re.compile(r"^(\d\d-\d\d \d\d:\d\d:\d\d\.\d+)")

# The runtime calls a generated arm can make that run *Java* code, or take a
# lock, and so can be where a thread ends up parked. Everything else in an arm is
# a JNI accessor that returns in microseconds.
JAVA_CALL = re.compile(
    r"d2c::(call_[a-z_0-9]*|invoke_polymorphic|monitor_enter|alloc_object|"
    r"new_string_utf[a-z_0-9]*|throw_exception|method_handle|method_type)\("
)
JNI_CALL = re.compile(r"d2c::([a-z_0-9]+)\(")
# `resolve_method(env, &mut clsN, &mut mthN, static, b"owner", b"name", ...)`
# names what a `jmethodID` in this arm points at, so a call site can be read.
RESOLVED = re.compile(
    r"resolve_(?:method|static_method|instance_method|polymorphic)\(env, &mut ([a-z_0-9]+), "
    r"&mut ([a-z_0-9]+), \w+, b\"([^\"]+)\", b\"([^\"]+)\""
)
# A call site, and which `jmethodID` it goes through.
CALLED = re.compile(r"d2c::(call_[a-z_0-9]*)\(env, [^,]+, [^,]+, ([a-z_0-9]+), ([a-z_0-9]+),")

# Calls into anything that is not the JDK/Android itself: app code, and the
# app's own `native*` JNI methods, which is where a build step's time goes and
# where a park is by far the most likely to be.
APP_OWNER = re.compile(r"^(?!(java|javax|android|androidx|kotlin|dalvik)/)")


def app_calls(text):
    """`owner.name` for every call in this arm into non-platform code."""
    resolved = {mth: (owner, name) for _, mth, owner, name in RESOLVED.findall(text)}
    out = []
    for _, _, mth in CALLED.findall(text):
        target = resolved.get(mth)
        if target and APP_OWNER.match(target[0]) and target not in out:
            out.append(target)
    return out


def methods_dir(directory):
    """Accept the project root, its `src/`, or the `methods/` directory."""
    for candidate in (directory, os.path.join(directory, "methods"),
                      os.path.join(directory, "src", "methods")):
        if os.path.isdir(candidate) and glob.glob(os.path.join(candidate, "*.rs")):
            return candidate
    return directory


def load_methods(directory):
    """Map JNI entry-point name -> body text, from the generated tree."""
    directory = methods_dir(directory)
    blob = ""
    for path in sorted(glob.glob(os.path.join(directory, "*.rs"))):
        blob += open(path, encoding="utf-8", errors="replace").read()
    marks = [(m.end(), m.group(1)) for m in FN.finditer(blob)]
    bodies = {}
    for i, (end, name) in enumerate(marks):
        stop = marks[i + 1][0] if i + 1 < len(marks) else len(blob)
        bodies[name] = blob[end:stop]
    return bodies


def arm_of(body, want):
    for match in ARM.finditer(body):
        if int(match.group(1)) != want:
            continue
        depth, i = 1, match.end()
        while i < len(body) and depth:
            if body[i] == "{":
                depth += 1
            elif body[i] == "}":
                depth -= 1
            i += 1
        return body[match.end():i - 1]
    return None


def statements(text):
    return [line.strip() for line in text.splitlines() if line.strip()]


def reports(lines):
    """Distinct (kind, method, state) reports, with how often/long they fired.

    `first`/`last`/`secs` are the reported seconds of the first, most recent and
    longest line of the series and `last_ts` its logcat timestamp, in log order --
    which is what [`climb_note`] reads to tell a park from a stage that is
    working.
    """
    seen = {}
    for line in lines:
        ts = TS.match(line)
        ts = ts.group(1) if ts else ""
        match = REPORT.search(line)
        if match:
            kind, secs, name, state = match.groups()
            # `STUCK` is what builds before the rename emitted, and the runtime
            # then also wrote a stronger `"...is the hang"` tail that a real
            # capture disproved; only the kind matters here.
            kind = "PARKED" if kind == "STUCK" else kind
            secs = int(secs)
            key = (kind, name, int(state))
            entry = seen.setdefault(
                key, {"count": 0, "secs": 0, "first": secs, "last": secs, "last_ts": ts}
            )
            entry["count"] += 1
            entry["secs"] = max(entry["secs"], secs)
            entry["last"] = secs
            if ts:
                entry["last_ts"] = ts
            continue
        match = FREEZE.search(line)
        if match:
            name, state = match.groups()
            key = ("FREEZE", name, int(state))
            entry = seen.setdefault(
                key, {"count": 0, "secs": 0, "first": 0, "last": 0, "last_ts": ts}
            )
            entry["count"] += 1
            if ts:
                entry["last_ts"] = ts
    return seen


def verdict_is_bad(kind, verdict):
    """Which classifications mean "this line should not be here"?"""
    if verdict in ("IMPOSSIBLE", "UNREACHABLE"):
        return True
    return kind == "FROZEN" and verdict == "STALE"


def classify(bodies, kind, name, state):
    """(verdict, detail lines) for one report."""
    body = bodies.get(name)
    if body is None:
        return "UNREACHABLE", [
            "no generated method with this JNI name -- wrong tree, or the line",
            "came from a different APK than the one being looked at",
        ]
    text = arm_of(body, state)
    if text is None:
        return "UNREACHABLE", [f"the method has no state {state}"]
    if kind == "FROZEN":
        # First, because a pre-fix line's `(method, state)` pair is not evidence
        # of anything -- which is the whole point of the entry.
        return "STALE", [
            "`FROZEN` is a pre-fix line: the sampler used to report a thread",
            "whose `(method, state)` was the same at every reading while the",
            "dispatcher kept running, as a dispatcher spinning in that state.",
            "It cannot tell that from a method the app calls over and over",
            "whose slow state is always this one -- and on a real build it named",
            "states whose arm is a bare `return`. A spin is now named from inside",
            "the method, as `FREEZE in <method> at state <n>`.",
        ]
    code = statements(text)
    if len(code) == 1 and (code[0] == "return;" or code[0].startswith("return ")):
        return "IMPOSSIBLE", [
            f"state {state} is `{code[0]}` -- a bare return, so the dispatcher",
            "cannot run it again: the loop left the method. The slot was left",
            "behind by a method that had already returned, which is what the",
            "d2c::watch() guard fixes; the build that logged this predates it.",
        ]
    shown = code[:12]
    detail = [f"state {state} ({len(code)} statements"
              + (", first 12 shown):" if len(code) > len(shown) else "):")]
    detail += [f"    {line[:150]}" for line in shown]
    if len(code) > len(shown):
        detail.append(f"    ... {len(code) - len(shown)} more statements")
    own = app_calls(text)
    if own:
        detail.append(
            "calls this app's own code: "
            + ", ".join(f"{owner}.{name}" for owner, name in own)
            + " -- a park inside one of these is guest code doing its job or "
            "not returning"
        )
    java = sorted(set(JAVA_CALL.findall(text)))
    jni = sorted(set(JNI_CALL.findall(text)))
    if java:
        detail.append(
            "runs Java code or takes a lock: " + ", ".join(java)
            + " -- one of these is where the thread is"
        )
    elif kind == "PARKED":
        detail.append(
            "no Java call in this block (only "
            + (", ".join(jni) if jni else "straight-line code")
            + "), so a park here is not guest code running"
        )
    if kind == "FREEZE":
        detail.append(
            "reported by the in-method guard, not the sampler: this arm never "
            "set __state, which is a conversion bug"
        )
    if "break 'd2c;" in text:
        detail.append(
            "this block also leaves the method without handing the slot back "
            "(a `break 'd2c` on a pending exception), so a build without the "
            "d2c::watch() guard can leave this state in the slot the same way "
            "a bare return does: only a current build makes this line mean a park"
        )
    return "candidate", detail


def climb_note(entry, is_latest):
    """Did a `PARKED` series keep climbing, or did it reset?

    One property that separates a hang from work finishing, and it belongs to the
    series rather than to any single line: the seconds restart at 5 on every state
    transition of the frame, so a stage that hands work to the app's own native
    library climbs and resets, while a frame that never returns climbs and never
    does. Climbing on its own is still not a verdict -- a real capture climbed to
    27 s in a native stage and finished -- so a series that ends at its peak is a
    hang only if nothing else reported after it (`is_latest`): a later line for
    another frame means the build moved on and the series ended with the stage.
    """
    first = entry.get("first", 0)
    last = entry.get("last", 0)
    top = entry["secs"]
    if entry["count"] < 2:
        return ["one line only: too early to tell a park from a stage that ended"]
    if last < top:
        return [
            f"climbed to {top}s then reset (last line {last}s): the frame made",
            "progress, so this is a stage that finished, not a hang",
        ]
    if not is_latest:
        return [
            f"climbed to {top}s and ended there, but a later line reports a",
            "different frame, so the build moved on: the series ended because",
            "the stage finished, not because it hung",
        ]
    return [
        f"climbed {first}s -> {top}s, the last line is the longest, and nothing",
        "else reports after it: the frame never transitioned again, which is",
        "the shape of a hang. The call listed above is what it is waiting in.",
    ]


def render(bodies, seen):
    order = sorted(seen.items(), key=lambda kv: (kv[0][1], kv[0][2]))
    latest = max((e.get("last_ts", "") for e in seen.values()), default="")
    lines = []
    bad = 0
    for (kind, name, state), entry in order:
        verdict, detail = classify(bodies, kind, name, state)
        times = f"{entry['count']} line(s)"
        if entry["secs"]:
            times += f", reported up to {entry['secs']}s"
        lines.append(f"[{verdict}] {kind} in {name} at state {state}  ({times})")
        lines += ["    " + line for line in detail]
        if kind == "PARKED":
            is_latest = bool(latest) and entry.get("last_ts", "") == latest
            lines += ["    " + line for line in climb_note(entry, is_latest)]
        lines.append("")
        if verdict_is_bad(kind, verdict):
            bad += 1
    return lines, bad


def main():
    args = [a for a in sys.argv[1:] if a != "--selftest"]
    if "--selftest" in sys.argv[1:]:
        return selftest()
    if not args:
        print(__doc__.strip())
        return 2
    directory = args[0]
    text = open(args[1], encoding="utf-8", errors="replace").read() if len(args) > 1 \
        else sys.stdin.read()
    if not os.path.isdir(directory):
        print(f"no such directory: {directory}", file=sys.stderr)
        return 2
    bodies = load_methods(directory)
    if not bodies:
        print(f"no generated methods under {directory}", file=sys.stderr)
        return 2
    seen = reports(text.splitlines())
    if not seen:
        print("no watchdog lines found (FROZEN/PARKED/FREEZE)")
        return 0
    lines, bad = render(bodies, seen)
    print("\n".join(lines))
    print(f"{len(seen)} distinct report(s); {len(seen) - bad} to look at, "
          f"{bad} that should not be in a log from this build")
    return 1 if bad else 0


FIXTURE = '''\
pub unsafe extern "system" fn Java_test_returns__(env: d2c::Env) -> d2c::jint {
    unsafe {
        use dex2rs_runtime as d2c;
        let d2c_watch = d2c::watch();
        let mut __state: i64 = 0;
        'd2c: loop {
            d2c::probe(b"Java_test_returns__", __state);
            match __state {
            0 => {
                __state = 1; continue 'd2c;
            }
            1 => {
                return v0;
            }
                _ => break 'd2c,
            }
        }
        return Default::default();
    }
}

pub unsafe extern "system" fn Java_test_parks__(env: d2c::Env) -> d2c::jint {
    unsafe {
        use dex2rs_runtime as d2c;
        let d2c_watch = d2c::watch();
        let mut __state: i64 = 0;
        'd2c: loop {
            d2c::probe(b"Java_test_parks__", __state);
            match __state {
            0 => {
                if d2c::resolve_method(env, &mut mth0, false, b"a/b", b"c", b"()V") { break 'd2c; }
                d2c::call_void_method(env, v0, mth0);
                __state = 1; continue 'd2c;
            }
            1 => {
                return v0;
            }
                _ => break 'd2c,
            }
        }
        return Default::default();
    }
}
'''


def selftest():
    """The classifier has to tell an impossible report from a real one."""
    with tempfile.TemporaryDirectory() as tmp:
        os.makedirs(os.path.join(tmp, "methods"))
        with open(os.path.join(tmp, "methods", "test.rs"), "w") as handle:
            handle.write(FIXTURE)
        bodies = load_methods(tmp)
        assert len(bodies) == 2, bodies.keys()
        fake = [
            # A state whose arm returns cannot be parked in.
            "09-14 19:31:43.856 31741 31764 E dex2rs: dex2rs: PARKED 5s in "
            "Java_test_returns__ at state 1: no dispatcher progress",
            # A pre-fix line kind: reported, but not as something to chase.
            "09-14 19:31:43.856 31741 31764 E dex2rs: dex2rs: FROZEN 5s in "
            "Java_test_returns__ at state 0: the dispatcher is spinning",
            # The real thing: parked in a call from that block.
            "09-14 19:31:44.470 32663 32678 E dex2rs: dex2rs: PARKED 5s in "
            "Java_test_parks__ at state 0: no dispatcher progress",
            # The older spelling of the same kind, and it has to read as one.
            "09-14 19:31:44.700 32663 32678 E dex2rs: dex2rs: STUCK 5s in "
            "Java_test_parks__ at state 0: no dispatcher progress",
            # The in-method guard: a conversion bug.
            "09-14 19:31:44.900 32663 32678 E dex2rs: dex2rs: FREEZE in "
            "Java_test_parks__ at state 0: that block never leaves its state",
        ]
        seen = reports(fake)
        assert len(seen) == 4, seen
        lines, bad = render(bodies, seen)
        out = "\n".join(lines)
        assert "[IMPOSSIBLE]" in out, out
        assert "[STALE]" in out, out
        assert "call_void_method" in out, out
        assert bad == 2, (bad, out)
        # ... and it must not call a real one impossible or stale, or the check
        # is worse than nothing.
        verdict, _ = classify(bodies, "PARKED", "Java_test_parks__", 0)
        assert verdict == "candidate", verdict
        assert classify(bodies, "FREEZE", "Java_test_parks__", 0)[0] == "candidate"
        assert classify(bodies, "PARKED", "Java_nope__", 0)[0] == "UNREACHABLE"

        # The climb reading, both ways. It is tested here because it is now the
        # only thing that separates a hang from a stage that is working: the two
        # look identical line by line, and differ only in whether the seconds
        # ever reset.
        def series(*secs):
            return reports(
                [f"dex2rs: PARKED {s}s in Java_test_parks__ at state 0" for s in secs]
            )[("PARKED", "Java_test_parks__", 0)]

        working = " ".join(climb_note(series(5, 10, 15, 5, 10), True))
        assert "then reset" in working and "not a hang" in working, working
        hung = " ".join(climb_note(series(5, 10, 20, 45), True))
        assert "shape of a hang" in hung, hung
        # A series that ends at its peak is only a hang if nothing else
        # reported afterwards: on the device log that started all this, the
        # `mergeInstalled` series ended at 15 s and the next line was a
        # *different* stage starting, so the build was moving.
        moved_on = " ".join(
            climb_note({"count": 3, "first": 5, "last": 15, "secs": 15}, False)
        )
        assert "moved on" in moved_on and "not because it hung" in moved_on, moved_on
        # One line is not a series, and must not be called either way.
        assert "too early" in " ".join(climb_note(series(5), True))
    print("watchdog_log selftest: OK (a returned state is impossible, a pre-fix "
          "FROZEN line is stale, a park and an in-method spin are candidates, "
          "an unknown method is unreachable, the older STUCK spelling reads as "
          "PARKED, and a PARKED series that resets is told apart from one that "
          "never does)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
