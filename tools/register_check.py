#!/usr/bin/env python3
"""Audit the `--dynamic-register` table against the generated entry points.

`RegisterNatives` tells ART how to call a native method: it passes the *method
name* and the *JNI signature*, and ART builds the call from that signature alone.
If the signature disagrees with the ABI of the function pointer we hand it --
a wrong argument count, a float where an int is expected, a wrong return type --
ART calls the function with the wrong calling convention and the app dies with a
SIGSEGV that has no Java stack. Nothing else in the pipeline checks this: the
static path (exported `Java_*` symbols) is matched by name, so a bad signature
could not exist there in the first place.

Inputs are the two generated files:

    src/dynamic_register.rs   the tables
    src/methods.rs            the entry points

For every `d2c::native_method(name, signature, methods::SYM as *const ())` the
checker decodes the JNI signature with `descriptor.pyjni`-style rules and asserts
that the emitted entry point takes exactly `env`, the receiver and one parameter
per signature slot, with the matching Rust type -- and the matching return type.

Usage:
    python3 tools/register_check.py --source-dir /tmp/dr/proj
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# The `--obfuscate` class form of `register_natives` is ciphertext, and `verify.py`
# already ports the runtime's keystream (see `obfuscated_names` there). Reusing it
# rather than writing a second decoder keeps the two from drifting.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import verify  # noqa: E402

# JNI descriptor type -> the Rust type codegen emits (see codegen::return_type).
DESC_TO_RUST = {
    "Z": "d2c::jboolean",
    "B": "d2c::jni_sys::jbyte",
    "C": "d2c::jni_sys::jchar",
    "S": "d2c::jni_sys::jshort",
    "I": "d2c::jni_sys::jint",
    "J": "d2c::jni_sys::jlong",
    "F": "d2c::jni_sys::jfloat",
    "D": "d2c::jni_sys::jdouble",
    "V": "()",
}

RECV = "d2c::jni_sys::jobject"
ENV = "d2c::Env"


def parse_descriptor(text: str, i: int) -> tuple[str, str, int]:
    """Return (kind, rust_type, next_index) for the descriptor at `text[i]`.

    kind is 'prim', 'obj' or 'array'.
    """
    char = text[i]
    if char in DESC_TO_RUST:
        return "prim", DESC_TO_RUST[char], i + 1
    if char == "[":
        _, _, j = parse_descriptor(text, i + 1)
        # An array is a reference in JNI.
        return "array", "d2c::jni_sys::jobject", j
    if char == "L":
        end = text.index(";", i)
        return "obj", "d2c::jni_sys::jobject", end + 1
    raise ValueError(f"bad descriptor at {i}: {text!r}")


def split_signature(sig: str) -> tuple[list[str], str]:
    """Split `(..)R` into (parameter rust types, return rust type)."""
    if not sig.startswith("("):
        raise ValueError(f"signature does not start with '(': {sig!r}")
    close = sig.index(")")
    params: list[str] = []
    i = 1
    while i < close:
        _, rust, i = parse_descriptor(sig, i)
        params.append(rust)
    ret_text = sig[close + 1 :]
    _, ret, i = parse_descriptor(ret_text, 0)
    if i != len(ret_text):
        raise ValueError(f"trailing text in return type: {ret_text!r}")
    return params, ret


# `d2c::native_method(\n  c"name",\n  c"sig",\n  methods::SYM as *const (),\n),`
ENTRY = re.compile(
    r'd2c::native_method\(\s*c"((?:[^"\\]|\\.)*)",\s*c"((?:[^"\\]|\\.)*)",\s*'
    r'methods::(\S+)\s+as\s+\*const\s*\(\)\s*,?\s*\)',
    re.S,
)

# `pub unsafe extern "system" fn NAME(env: X, thiz: Y, p0: T, ..) -> R {`
FUNC = re.compile(
    r'pub unsafe extern "system" fn (\S+)\s*\((.*?)\)\s*->\s*([^{]+?)\s*\{',
    re.S,
)

# `d2c::native_method_obf(Name::obf(b"...", 0x..), Name::obf(b"...", 0x..), methods::SYM as *const ())`
#
# The `--obfuscate` spelling of an entry. `RegisterNatives` builds the call from
# the signature, and that signature names the parameter and return classes, so
# both it and the method name are ciphertext in an obfuscated build. The check has
# to decode them, or an obfuscated project would look like it registered nothing.
OBF_ENTRY = re.compile(
    r'd2c::native_method_obf\(\s*'
    r'd2c::Name::obf\(\s*b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\s*\),\s*'
    r'd2c::Name::obf\(\s*b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\s*\),\s*'
    r'methods::(\S+)\s+as\s+\*const\s*\(\),?\s*\)',
    re.S,
)


# `// class/path` comment before each `let table_N`
CLASS_COMMENT = re.compile(r"// ([A-Za-z0-9_$/]+)\n\s*let table_(\d+)")

# `register_natives(env, b"class/path", &table_N)` -- or, under `--obfuscate`,
# `register_natives(env, d2c::Name::obf(b"...", 0x...), &table_N)`.
#
# `--obfuscate` encrypts the class a table registers against, for the same reason
# it encrypts every other reflected name: a `--dynamic-register` build would
# otherwise leave the app's class paths readable in the library. The regex has to
# accept both forms, because a check that only matched the plain one would report
# every table as unregistered on an obfuscated build -- a false failure that looks
# exactly like the bug it exists to find.
REGISTER_CALL = re.compile(
    r'register_natives(?:_obf)?\(env, (?:b"((?:[^"\\]|\\.)*)"'
    r'|d2c::Name::obf\(\s*b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\s*\))\s*,\s*&table_(\d+)\)'
)


def register_call_class(match) -> tuple[str, str]:
    """`(class_path, table)` for one [`REGISTER_CALL`] match.

    Decodes the `--obfuscate` form with `verify.py`'s keystream port, so the table
    can still be paired with the class its `// class/path` comment names. The
    plaintext form is taken as-is.
    """
    if match.group(1) is not None:
        return match.group(1), match.group(4)
    cipher = verify._byte_literal(match.group(2).encode())
    seed = int(match.group(3), 16)
    keys = verify._keystream(seed, len(cipher))
    return bytes(c ^ k for c, k in zip(cipher, keys)).decode("utf-8", "replace"), match.group(4)


def decoded_name(cipher: str, seed: str) -> str:
    """One `Name::obf(b"...", 0x..)` argument, back as plaintext."""
    raw = verify._byte_literal(cipher.encode())
    keys = verify._keystream(int(seed, 16), len(raw))
    return bytes(c ^ k for c, k in zip(raw, keys)).decode("utf-8", "replace")


def entries(register_rs: str):
    """`(name, signature, symbol)` for every table entry, in either form."""
    for m in ENTRY.finditer(register_rs):
        yield m.group(1), m.group(2), m.group(3)
    for m in OBF_ENTRY.finditer(register_rs):
        yield (
            decoded_name(m.group(1), m.group(2)),
            decoded_name(m.group(3), m.group(4)),
            m.group(5),
        )


def parse_entry_points(methods_rs: str) -> dict[str, tuple[list[tuple[str, str]], str]]:
    out: dict[str, tuple[list[tuple[str, str]], str]] = {}
    for m in FUNC.finditer(methods_rs):
        name, params, ret = m.group(1), m.group(2), m.group(3).strip()
        parsed = []
        for part in params.split(","):
            part = part.strip()
            if not part:
                continue
            pname, _, ptype = part.partition(":")
            parsed.append((pname.strip(), ptype.strip()))
        out[name] = (parsed, ret)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--source-dir", required=True, help="generated project dir")
    args = ap.parse_args()

    src = Path(args.source_dir) / "src"
    register_rs = (src / "dynamic_register.rs").read_text()
    # The entry points live one file per class under `methods/` (see
    # `write_methods` in src/scaffold.rs); older projects kept them in a single
    # `methods.rs`, so accept either.
    methods_rs = ""
    single = src / "methods.rs"
    if single.is_file():
        methods_rs += single.read_text()
    per_class = src / "methods"
    if per_class.is_dir():
        files = sorted(per_class.glob("*.rs"))
        for path in files:
            methods_rs += path.read_text()
        print(f"class modules              : {len(files)}")

    if "RegisterNatives" not in register_rs and "native_method" not in register_rs:
        print("this project was not generated with --dynamic-register")
        return 0

    funcs = parse_entry_points(methods_rs)
    print(f"entry points               : {len(funcs)}")

    failures: list[str] = []
    failures_call: list[str] = []
    checked = 0

    # class each table belongs to, so a failure names the class too
    table_class = {n: c for c, n in CLASS_COMMENT.findall(register_rs)}
    called = set()
    for match in REGISTER_CALL.finditer(register_rs):
        class_path, table = register_call_class(match)
        called.add(table)
        # The comment and the argument must name the same class: the comment is
        # what a reader believes, the argument is what the library actually
        # registers, and a mismatch is exactly what this check is for.
        named = table_class.get(table)
        if named is not None and named != class_path:
            failures_call.append(
                f"table_{table} registers {class_path} but its comment says {named}"
            )

    for name, sig, sym in entries(register_rs):
        checked += 1
        try:
            want_params, want_ret = split_signature(sig)
        except ValueError as exc:
            failures.append(f"sig {name}{sig}: {exc}")
            continue

        if sym not in funcs:
            failures.append(f"{sym}: registered but no such entry point")
            continue
        got_params, got_ret = funcs[sym]

        # env, receiver, then one slot per parameter
        if got_params and got_params[0] != ("env", ENV):
            failures.append(f"{sym}: first parameter is {got_params[0]}, expected (env: {ENV})")
        if len(got_params) < 2 or got_params[1] != ("thiz", RECV):
            failures.append(
                f"{sym}: second parameter is {got_params[1] if len(got_params) > 1 else None},"
                f" expected (thiz: {RECV})"
            )
        actual = [t for _, t in got_params[2:]]
        if len(actual) != len(want_params):
            failures.append(
                f"{sym}: {name}{sig} declares {len(want_params)} parameter(s),"
                f" entry point takes {len(actual)}"
            )
        else:
            for index, (got, want) in enumerate(zip(actual, want_params)):
                if got != want:
                    failures.append(
                        f"{sym}: parameter {index} of {name}{sig} is {want}, entry point takes {got}"
                    )
        if got_ret != want_ret:
            failures.append(f"{sym}: {name}{sig} returns {want_ret}, entry point returns {got_ret}")

    failures.extend(failures_call)

    # every table must actually be registered, and only real tables registered
    for table, cls in table_class.items():
        if table not in called:
            failures.append(f"table_{table} ({cls}) is never passed to register_natives")
    for table in called:
        if table not in table_class:
            failures.append(f"register_natives references unknown table_{table}")

    print(f"RegisterNatives entries    : {checked}")
    print(f"tables / classes           : {len(table_class)}")
    print(f"problems                   : {len(failures)}")
    for line in failures[:40]:
        print("  " + line)
    if len(failures) > 40:
        print(f"  ... and {len(failures) - 40} more")

    if not failures:
        print("\nOK: every registered signature matches its entry point's ABI")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
