#!/usr/bin/env python3
"""`--obfuscate` must change the string call and nothing else.

The obfuscated path rewrites every `const-string`:

    v5 = d2c::new_string_utf(env, b"packageName");
    v5 = d2c::new_string_utf_obf_seeded(env, b"8\\x9dD...", 0x20bcc98e);

every class/field/method name the reflection path passes:

    if d2c::resolve_class(env, &mut cls1, b"b0/c") { ... }
    if d2c::resolve_class(env, &mut cls1, d2c::Name::obf(b"7\\x1b\\xf2...", 0x31d47f22)) { ... }

and, with `--dynamic-register`, drops the `#[unsafe(no_mangle)]` export (the
library registers its entry points instead). Those are the *only* three
differences the option is allowed to make: a string constant, a name, and the
export attribute. It must not touch a branch, a state, an arm or a call -- if it
did, an obfuscated build would run different control flow from a plain one, and
the plain one is the build that has been tested on a device.

A name site is compared by *decrypting* it (the generator's keystream is ported
below, and `codegen`/the runtime are pinned to each other by
`dex2rs-core/tests/compile.rs`), so the two sides have to be identical text
rather than merely exempted from the comparison. A `resolve_*` that still carries
a plain name in the obfuscated tree is refused outright: that is the leak this
whole path exists to prevent.

Nothing used to check that. This does: it canonicalises both string forms to one
token, drops the export attribute, and requires the two trees to be *identical*,
file for file, line for line. Two failures are refused rather than passed:

  * no `_obf_seeded` call in the "obfuscated" tree -- then the comparison is
    plain against plain and proves nothing;
  * no file compared at all.

Usage:
    python3 tools/obf_equiv.py PLAIN_SRC OBF_SRC
    python3 tools/obf_equiv.py target/dist-prod/src target/dist-obf-dyn/src
    python3 tools/obf_equiv.py --selftest
"""

import argparse
import glob
import os
import re
import sys

# One obfuscated call site: the ciphertext literal and the seed that travels with
# it. Paired against the plain literal at the same call site on the other side.
OBF_SITE_RE = re.compile(
    r'd2c::new_string_utf_obf_seeded\(env, b"((?:[^"\\]|\\.)*)", (0x[0-9a-fA-F]+)\)'
)
PLAIN_SITE_RE = re.compile(r'd2c::new_string_utf\(env, b"((?:[^"\\]|\\.)*)"\)')

# One obfuscated name site: the ciphertext of a class/field/method name plus the
# seed that travels with it. The same expression matches the whole call, so it is
# also what canonicalise replaces; the two capturing groups are the ciphertext
# and the seed.
NAME_OBF_SITE_RE = re.compile(
    r'd2c::Name::obf\(b"((?:[^"\\]|\\.)*)", (0x[0-9a-fA-F]+)\)'
)
NAME_OBF_CALL_RE = NAME_OBF_SITE_RE
# The calls whose arguments are names: a `resolve_*`, a `check-cast` (which only
# needs the name for the exception message) and the exception-handler match.
NAME_CALL_RE = re.compile(
    r'd2c::(?:resolve_(?:class|method|field)|check_cast|is_instance_of_name)\('
)
# A `resolve_*` in the *obfuscated* tree that still passes a plain name. Every
# one of them has to go through `Name::obf`, so the literal has to be reached
# without passing `Name::obf(` -- a ciphertext is itself a `b"..."`, and matching
# on the literal alone would report every correctly obfuscated call as a leak.
# The statements are one per line, so the walk stays inside one.
PLAIN_RESOLVE_NAME_RE = re.compile(
    r'd2c::resolve_(?:class|method|field)\(env,(?:(?!Name::obf\()[^)\n])*b"'
)
LITERAL_RE = re.compile(r'b"((?:[^"\\]|\\.)*)"')

# One `RegisterNatives` table entry, in both spellings.
#
# `RegisterNatives` builds the call from the signature, and the signature names the
# parameter and return classes, so an `--obfuscate` build encrypts the method name
# and the signature too (`native_method_obf`, decoded by the runtime at
# registration time). That is a third place the two trees differ, and it has to be
# canonicalised to its plaintext rather than exempted, or a keystream disagreement
# for those strings would go unnoticed. Both forms are rewritten to the same
# `E(name,sig,SYM)` so the plain tree's `c".."` and the obfuscated tree's decrypted
# `b".."` compare equal.
PLAIN_ENTRY_RE = re.compile(
    r'd2c::native_method\(\s*c"((?:[^"\\]|\\.)*)",\s*c"((?:[^"\\]|\\.)*)",\s*'
    r'methods::(\S+)\s+as\s+\*const\s*\(\),?\s*\)'
)
OBF_ENTRY_RE = re.compile(
    r'd2c::native_method_obf\(\s*'
    r'd2c::Name::obf\(b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\),\s*'
    r'd2c::Name::obf\(b"((?:[^"\\]|\\.)*)",\s*(0x[0-9a-fA-F]+)\),\s*'
    r'methods::(\S+)\s+as\s+\*const\s*\(\),?\s*\)',
    re.S,
)


def plain_entries(text: str):
    """`(name, signature, symbol)` for a plain tree's `RegisterNatives` table."""
    return [
        (unescape(m.group(1)), unescape(m.group(2)), m.group(3))
        for m in PLAIN_ENTRY_RE.finditer(text)
    ]


def obf_entries(text: str):
    """The same tuples, for an `--obfuscate` tree's table."""
    return [
        (
            decrypt(m.group(1), int(m.group(2), 16)),
            decrypt(m.group(3), int(m.group(4), 16)),
            m.group(5),
        )
        for m in OBF_ENTRY_RE.finditer(text)
    ]


# The obfuscated call: a ciphertext literal plus its seed.
OBF_CALL_RE = re.compile(r'd2c::new_string_utf_obf_seeded\(env, b"(?:[^"\\]|\\.)*", 0x[0-9a-fA-F]+\)')
# The plain call, with the string it was generated from.
PLAIN_CALL_RE = re.compile(r'd2c::new_string_utf\(env, b"(?:[^"\\]|\\.)*"\)')
# The export attribute: present under static registration, gone under dynamic.
NO_MANGLE_LINE = "#[unsafe(no_mangle)]"
# The trace build (the one used to name a hang on a device) inserts a logging
# call per instruction. It is still an --obfuscate build and is still expected to
# have the same control flow, so the logging is dropped rather than exempted.
TRACE_LINE_RE = re.compile(r'^\s*d2c::(?:trace|enter|leave)\((?:b)?"(?:[^"\\]|\\.)*"\);$')


def escape_literal(raw: bytes) -> str:
    r"""The interior of the `b"..."` literal Rust's `codegen::bytes_literal` writes.

    Has to match it byte for byte, because a decrypted name is put back into the
    source text and the two trees are then required to be identical: `bytes_literal`
    escapes `"` and `\\` as themselves and everything outside printable ASCII as
    `\xNN`, so this has to do the same rather than pick its own escaping.
    """
    out = []
    for byte in raw:
        if byte == 0x22:
            out.append('\\"')
        elif byte == 0x5C:
            out.append("\\\\")
        elif 0x20 <= byte <= 0x7E:
            out.append(chr(byte))
        else:
            out.append(f"\\x{byte:02x}")
    return "".join(out)


def name_sites(text: str):
    """`(ciphertext, seed)` for every `d2c::Name::obf(...)` site, in order."""
    return [
        (cipher, int(seed, 16)) for cipher, seed in NAME_OBF_SITE_RE.findall(text)
    ]


def plain_name_literals(text: str):
    """The plain name literals a tree passes, in order (one call per line)."""
    out = []
    for line in text.splitlines():
        if NAME_CALL_RE.search(line):
            out.extend(LITERAL_RE.findall(line))
    return out


def canonicalise(text: str, obfuscated: bool) -> str:
    """One form for both sides: string and name calls become their plaintext.

    Exports and traces are dropped. A name is *decrypted* rather than exempted,
    so what is left has to equal the plain tree's text exactly -- which also
    means a generator/runtime keystream disagreement for names shows up here.
    """
    pattern = OBF_CALL_RE if obfuscated else PLAIN_CALL_RE
    text = pattern.sub("S", text)
    if obfuscated:
        text = NAME_OBF_CALL_RE.sub(
            lambda m: 'b"' + escape_literal(decrypt(m.group(1), int(m.group(2), 16))) + '"',
            text,
        )
    return "\n".join(
        line
        for line in text.splitlines()
        if NO_MANGLE_LINE not in line and not TRACE_LINE_RE.match(line)
    )


def methods_dir(src: str) -> str:
    """Accept the project dir, its `src/`, or `src/methods/` itself."""
    for candidate in (
        os.path.join(src, "methods"),
        os.path.join(src, "src", "methods"),
        src,
    ):
        if os.path.isdir(candidate) and glob.glob(os.path.join(candidate, "*.rs")):
            return candidate
    return os.path.join(src, "methods")


def read_tree(src: str, obfuscated: bool):
    directory = methods_dir(src)
    return {
        os.path.basename(path): canonicalise(
            open(path, encoding="utf-8").read(), obfuscated
        )
        for path in sorted(glob.glob(os.path.join(directory, "*.rs")))
    }


def raw_text(src: str) -> str:
    return "".join(
        open(path, encoding="utf-8").read()
        for path in sorted(glob.glob(os.path.join(methods_dir(src), "*.rs")))
    )


def unescape(literal: str) -> bytes:
    r"""The bytes a Rust byte-string interior stands for (`\xNN`, `\\`, ...)."""
    out = bytearray()
    i = 0
    while i < len(literal):
        char = literal[i]
        if char != "\\":
            out += char.encode("utf-8")
            i += 1
            continue
        code = literal[i + 1]
        if code == "x":
            out.append(int(literal[i + 2 : i + 4], 16))
            i += 4
        elif code in "nrt0":
            out.append({"n": 10, "r": 13, "t": 9, "0": 0}[code])
            i += 2
        else:
            out += code.encode("utf-8")
            i += 2
    return bytes(out)


def keystream(seed: int, length: int) -> bytes:
    """A port of `dex2rs_runtime::keystream` (xorshift32, four bytes a round).

    This lives in a different crate from the generator that uses it, and nothing
    in the type system ties the two together: if they disagree, every obfuscated
    string in the app comes out as garbage — wrong class names, wrong SQL, wrong
    keys — while the build stays green. So the check re-derives the keystream from
    the emitted seed and compares the result against the plaintext the plain
    build passes at the same call site.
    """
    mask = 0xFFFFFFFF
    x = seed & mask
    out = bytearray(length)
    i = 0
    while i < length:
        x = (x ^ ((x << 13) & mask)) & mask
        x = (x ^ (x >> 17)) & mask
        x = (x ^ ((x << 5) & mask)) & mask
        for byte in range(4):
            if i + byte < length:
                out[i + byte] = (x >> (byte * 8)) & 0xFF
        i += 4
    return bytes(out)


def decrypt(cipher_literal: str, seed: int) -> bytes:
    cipher = unescape(cipher_literal)
    return bytes(c ^ k for c, k in zip(cipher, keystream(seed, len(cipher))))


def decode_check(plain_src: str, obf_src: str):
    """(string sites, name sites, problems): every obfuscated constant and name
    must decode to the plaintext the plain build uses at the same call site.

    Call sites are paired by index within each module. That is sound because the
    structural comparison above has already established the two trees are
    identical once both forms are canonicalised, so the sites are in the same
    order; a changed count shows up here as a problem rather than as a silent
    mispairing.
    """
    problems = []
    sites = 0
    names = 0
    plain_dir, obf_dir = methods_dir(plain_src), methods_dir(obf_src)
    for path in sorted(glob.glob(os.path.join(obf_dir, "*.rs"))):
        name = os.path.basename(path)
        plain_path = os.path.join(plain_dir, name)
        if not os.path.exists(plain_path):
            continue
        obf_text = open(path, encoding="utf-8").read()
        plain_text = open(plain_path, encoding="utf-8").read()
        obf_sites = OBF_SITE_RE.findall(obf_text)
        plain_sites = PLAIN_SITE_RE.findall(plain_text)
        if len(obf_sites) != len(plain_sites):
            problems.append(
                f"{name}: {len(obf_sites)} obfuscated call sites but "
                f"{len(plain_sites)} plain ones, so they cannot be paired"
            )
            continue
        for (cipher, seed_text), plaintext in zip(obf_sites, plain_sites):
            sites += 1
            want = unescape(plaintext) + b"\x00"
            got = decrypt(cipher, int(seed_text, 16))
            if got != want:
                problems.append(
                    f"{name}: {plaintext[:40]!r} decodes to {got[:48]!r}, "
                    f"not {want[:48]!r}"
                )

        # Names are paired the same way, and by the same argument: the tree now
        # holds ciphertext where the plain one holds `b"..."` at the same call
        # sites. The plaintext comes back one byte longer only for strings (the
        # NUL for `NewStringUTF`), which is why a name has nothing to strip.
        obf_names = name_sites(obf_text)
        plain_names = plain_name_literals(plain_text)
        if len(obf_names) != len(plain_names):
            problems.append(
                f"{name}: {len(obf_names)} obfuscated name sites but "
                f"{len(plain_names)} plain ones, so they cannot be paired"
            )
            continue
        for (cipher, seed), plaintext in zip(obf_names, plain_names):
            names += 1
            want = unescape(plaintext)
            got = decrypt(cipher, seed)
            if got != want:
                problems.append(
                    f"{name}: name {plaintext[:40]!r} decodes to {got[:48]!r}, "
                    f"not {want[:48]!r}"
                )
    return sites, names, problems


def register_file(root: str) -> str | None:
    """`dynamic_register.rs`, accepting the project dir or its `src/`.

    The two callers hand over different ones: `read_tree` is given `src/`, and
    `entry_check` is called from `main` with whatever the user passed. Both shapes
    are in use (`check_all.sh` passes `src/`), so accept either rather than rely on
    the caller to normalise.
    """
    for candidate in (
        os.path.join(root, "dynamic_register.rs"),
        os.path.join(root, "src", "dynamic_register.rs"),
    ):
        if os.path.exists(candidate):
            return candidate
    return None


def entry_check(plain_src: str, obf_src: str):
    """`(entries, problems)`: the two `RegisterNatives` tables have to agree.

    The comparison above is per-module under `src/methods/`, and the register table
    is `src/dynamic_register.rs`, so it is checked here instead -- by decoding both
    tables to `(name, signature, symbol)` tuples and requiring the sequences to be
    equal. Comparing tuples rather than text is deliberate: the two spellings differ
    in the function name, the argument type and the failure message for reasons that
    are all intentional, and normalising those away in a text diff would mean
    writing off real differences along with them.

    This is the third place the pass rewrites, and the one that would otherwise go
    unverified: `register_check.py` proves each table is internally consistent, but
    not that the obfuscated one registers what the plain one registers. A static
    build has no table, which is a skip rather than a failure.
    """
    plain_path, obf_path = register_file(plain_src), register_file(obf_src)
    if plain_path is None or obf_path is None:
        return 0, []
    plain = plain_entries(open(plain_path, encoding="utf-8").read())
    obf = obf_entries(open(obf_path, encoding="utf-8").read())
    if not plain and not obf:
        return 0, []

    problems = []
    if len(plain) != len(obf):
        problems.append(
            f"{len(plain)} plain register entries but {len(obf)} obfuscated ones"
        )
        return 0, problems
    for index, (want, got) in enumerate(zip(plain, obf)):
        if want != got:
            problems.append(
                f"entry {index}: plain registers {want[0]!r}{want[1]!r} -> {want[2]}, "
                f"obfuscated {got[0]!r}{got[1]!r} -> {got[2]}"
            )
    return len(plain), problems


def emit_site(plaintext: bytes, seed: int) -> str:
    """The call the generator emits for one constant — built the way it builds it.

    Used by the selftest so the fixtures are self-consistent: a fixture whose
    ciphertext does not actually decode to its plaintext would make the whole
    check look broken (which is how this function came to exist).
    """
    body = plaintext + b"\x00"
    keys = keystream(seed, len(body))
    cipher = bytes(b ^ k for b, k in zip(body, keys))
    return (
        f'd2c::new_string_utf_obf_seeded(env, b"{escape_literal(cipher)}", 0x{seed:x})'
    )


def emit_name_site(name: bytes, seed: int) -> str:
    """The `d2c::Name::obf` call the generator emits for one name."""
    keys = keystream(seed, len(name))
    cipher = bytes(b ^ k for b, k in zip(name, keys))
    return f'd2c::Name::obf(b"{escape_literal(cipher)}", 0x{seed:x})'


def emit_plain_entry(name: bytes, sig: bytes, sym: str) -> str:
    """One `d2c::native_method(...)` entry, the way the generator writes it."""
    return (
        "            d2c::native_method(\n"
        f'                c"{escape_literal(name)}",\n'
        f'                c"{escape_literal(sig)}",\n'
        f"                methods::{sym} as *const (),\n"
        "            ),\n"
    )


def emit_obf_entry(name: bytes, sig: bytes, sym: str) -> str:
    """The same entry under `--obfuscate`."""
    return (
        "            d2c::native_method_obf(\n"
        f"                {emit_name_site(name, 0x0BADC0DE)},\n"
        f"                {emit_name_site(sig, 0x0FEEFACE)},\n"
        f"                methods::{sym} as *const (),\n"
        "            ),\n"
    )


def selftest() -> int:
    """The comparison must catch a body that really changed, and refuse a bogus pair."""
    import tempfile

    plain = (
        "pub fn f() {\n"
        '    v = d2c::new_string_utf(env, b"hi");\n'
        '    if d2c::resolve_class(env, &mut cls0, b"b0/c") { break; }\n'
        "    __state = 2; continue 'd2c;\n}\n"
    )
    obf = (
        "pub fn f() {\n    v = "
        + emit_site(b"hi", 0x12345678)
        + ";\n    if d2c::resolve_class(env, &mut cls0, "
        + emit_name_site(b"b0/c", 0x31D47F22)
        + ") { break; }\n    __state = 2; continue 'd2c;\n}\n"
    )
    # Same method with a state changed: exactly the thing that must be reported.
    skewed = obf.replace("__state = 2", "__state = 3")
    # Same method with one name left in the clear: the refusal, not a difference.
    leaked = obf.replace(emit_name_site(b"b0/c", 0x31D47F22), 'b"b0/c"')
    assert leaked != obf, "the leak fixture did not change anything"

    # Same method with one ciphertext byte flipped: the decode check's own job.
    cipher, seed_text = OBF_SITE_RE.search(obf).group(1), OBF_SITE_RE.search(obf).group(2)
    flipped = obf.replace(
        f'0x{seed_text[2:]}', f'0x{seed_text[2:]}'
    ).replace(cipher, cipher[:-2] + ("41" if cipher[-2:] != "41" else "42"))

    failures = []
    if canonicalise(plain, False) != canonicalise(obf, True):
        failures.append("a string call or a name call was blamed as a difference")
    if decrypt(*name_sites(obf)[0]) != b"b0/c":
        failures.append("the keystream port does not reproduce the name fixture")
    if canonicalise(plain, False) == canonicalise(skewed, True):
        failures.append("a changed state was not blamed as a difference")
    if canonicalise("#[unsafe(no_mangle)]\npub fn f() {}", False) != canonicalise(
        "pub fn f() {}", False
    ):
        failures.append("the export attribute was blamed as a difference")

    # The register table: the plain spelling and the obfuscated one have to decode
    # to the same tuples, and neither may quietly drop an entry.
    entry_plain = emit_plain_entry(b"onCreate", b"(Landroid/os/Bundle;)V", "SYM_A")
    if "0xbadc0de" not in emit_obf_entry(b"onCreate", b"(Landroid/os/Bundle;)V", "SYM_A"):
        failures.append("the obfuscated entry fixture does not carry the seed it was built with")
    entry_obf = emit_obf_entry(b"onCreate", b"(Landroid/os/Bundle;)V", "SYM_A")
    want_entry = [(b"onCreate", b"(Landroid/os/Bundle;)V", "SYM_A")]
    if plain_entries(entry_plain) != want_entry:
        failures.append("the plain register-entry fixture does not parse to its own tuple")
    if obf_entries(entry_obf) != want_entry:
        failures.append(
            "the obfuscated register-entry fixture does not decode to the plain tuple"
        )
    if obf_entries(entry_obf.replace("0xbadc0de", "0xbadc0df")) == want_entry:
        failures.append("a tampered register-entry seed still decoded to the plain entry")

    # The keystream port must reproduce a known seed/plaintext pair, and a
    # corrupted ciphertext must not decode to the right plaintext.
    if decrypt(OBF_SITE_RE.search(obf).group(1), 0x12345678) != b"hi\x00":
        failures.append("the keystream port does not reproduce its own fixture")
    broken = OBF_SITE_RE.search(flipped)
    if broken and decrypt(broken.group(1), 0x12345678) == b"hi\x00":
        failures.append("a corrupted ciphertext decoded to the right plaintext")
    if not broken:
        failures.append("the fixture flip did not produce a call site")

    # End to end, on two real directories in a temp dir.
    with tempfile.TemporaryDirectory() as tmp:
        for name, body in (("plain", plain), ("obf", obf)):
            d = os.path.join(tmp, name, "src", "methods")
            os.makedirs(d)
            open(os.path.join(d, "A.rs"), "w").write(body)
        # A register table in both trees, so the entry comparison runs end to end.
        for side, body in (("plain", entry_plain), ("obf", entry_obf)):
            open(os.path.join(tmp, side, "src", "dynamic_register.rs"), "w").write(body)

        import subprocess

        cmd = [sys.executable, __file__]
        ok = subprocess.run(
            cmd + [os.path.join(tmp, "plain", "src"), os.path.join(tmp, "obf", "src")],
            capture_output=True,
        )
        if ok.returncode != 0:
            failures.append(f"matching trees reported as different: {ok.stdout.decode()}")
        if b"1 register entries are identical" not in ok.stdout:
            failures.append("the register table was not compared at all")

        # And a table that registers something else must fail, not pass quietly.
        open(os.path.join(tmp, "obf", "src", "dynamic_register.rs"), "w").write(
            entry_obf.replace("SYM_A", "SYM_B")
        )
        bad = subprocess.run(
            cmd + [os.path.join(tmp, "plain", "src"), os.path.join(tmp, "obf", "src")],
            capture_output=True,
        )
        if bad.returncode == 0:
            failures.append("a register table pointing at the wrong entry point passed")
        open(os.path.join(tmp, "obf", "src", "dynamic_register.rs"), "w").write(entry_obf)

        # And the decode check must actually fire when a constant is corrupted.
        open(os.path.join(tmp, "obf", "src", "methods", "A.rs"), "w").write(flipped)
        bad = subprocess.run(
            cmd + [os.path.join(tmp, "plain", "src"), os.path.join(tmp, "obf", "src")],
            capture_output=True,
        )
        if bad.returncode == 0:
            failures.append("a corrupted obfuscated constant passed the decode check")

        # A name left in the clear in the obfuscated tree is the leak this path
        # exists to prevent, and structural comparison alone cannot see it (the
        # plain side has the same literal).
        open(os.path.join(tmp, "obf", "src", "methods", "A.rs"), "w").write(leaked)
        bad = subprocess.run(
            cmd + [os.path.join(tmp, "plain", "src"), os.path.join(tmp, "obf", "src")],
            capture_output=True,
        )
        if bad.returncode == 0:
            failures.append("a plain name in the obfuscated tree passed the check")
        # Pointed at the same tree twice it must refuse: nothing was obfuscated.
        bad = subprocess.run(
            cmd + [os.path.join(tmp, "plain", "src"), os.path.join(tmp, "plain", "src")],
            capture_output=True,
        )
        if bad.returncode == 0:
            failures.append("a plain-vs-plain comparison was accepted as a pass")

    if failures:
        print("obf_equiv selftest: FAIL")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(
        "obf_equiv selftest: OK (a difference is caught, a decrypted name matches "
        "its plaintext, a plain name in the obfuscated tree is refused, a bogus "
        "pair is refused, and a register table that differs or is not compared "
        "at all is caught)"
    )
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("plain_src", nargs="?", help="generated project of a plain build")
    ap.add_argument("obf_src", nargs="?", help="generated project of an --obfuscate build")
    ap.add_argument("--max-report", type=int, default=10)
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if not args.plain_src or not args.obf_src:
        ap.error("plain_src and obf_src are required (or use --selftest)")

    plain = read_tree(args.plain_src, False)
    obf = read_tree(args.obf_src, True)
    if not plain or not obf:
        print("FAIL: no generated methods found in one of the trees")
        print(f"  plain: {methods_dir(args.plain_src)}")
        print(f"  obf  : {methods_dir(args.obf_src)}")
        return 1

    obf_text = raw_text(args.obf_src)
    obf_calls = len(OBF_CALL_RE.findall(obf_text))
    if obf_calls == 0:
        print(
            "FAIL: the second tree contains no `new_string_utf_obf_seeded` call, so "
            "it is not an --obfuscate build and the comparison proves nothing"
        )
        return 1

    # A `resolve_*` that still carries a plain name in the obfuscated tree is the
    # exact leak the reflection path is encrypted to prevent, and it would sail
    # through the comparison above (the plain side has one too).
    leaked = len(PLAIN_RESOLVE_NAME_RE.findall(obf_text))
    if leaked:
        print(
            f"FAIL: {leaked} resolve call(s) in the --obfuscate tree still pass a "
            "plain class/field/method name; the reflection path is not encrypted"
        )
        return 1

    only_plain = sorted(set(plain) - set(obf))
    only_obf = sorted(set(obf) - set(plain))
    differing = [name for name in plain if name in obf and plain[name] != obf[name]]

    print(f"plain modules  : {len(plain)}")
    print(f"obf modules    : {len(obf)}")
    print(f"obfuscated str : {obf_calls}")
    print(f"obfuscated nam : {len(NAME_OBF_SITE_RE.findall(obf_text))}")
    print(f"only in plain  : {len(only_plain)}")
    print(f"only in obf    : {len(only_obf)}")
    print(f"differing      : {len(differing)}")

    if only_plain or only_obf or differing:
        for name in (only_plain + only_obf)[: args.max_report]:
            print(f"\n  {name}: only in {'plain' if name in only_plain else 'obf'}")
        for name in differing[: args.max_report]:
            import difflib

            print(f"\n  {name}")
            for line in difflib.unified_diff(
                plain[name].splitlines(),
                obf[name].splitlines(),
                "plain",
                "obf",
                lineterm="",
                n=1,
            ):
                print(f"    {line[:160]}")
        if len(differing) > args.max_report:
            print(f"\n  ... and {len(differing) - args.max_report} more")
        return 1

    # Structure is identical, so now check the one rewrite that was allowed: every
    # obfuscated constant must decode, through the runtime's own keystream, to the
    # exact bytes the plain build hands to NewStringUTF at that call site.
    entries, entry_problems = entry_check(args.plain_src, args.obf_src)
    print(f"register entr  : {entries}" if entries else "register entries: none (static build)")
    if entry_problems:
        print("\nFAIL: the obfuscated RegisterNatives table is not the plain one")
        for problem in entry_problems[: args.max_report]:
            print(f"  - {problem}")
        return 1

    sites, names, problems = decode_check(args.plain_src, args.obf_src)
    print(f"decoded        : {sites}")
    print(f"decoded names  : {names}")
    print(f"decode wrong   : {len(problems)}")
    if sites == 0:
        print(
            "\nFAIL: no string call sites were paired, so the decode was not "
            "checked at all"
        )
        return 1
    if problems:
        print("\nFAIL: an obfuscated constant does not decode to the plain one")
        for problem in problems[: args.max_report]:
            print(f"  - {problem}")
        if len(problems) > args.max_report:
            print(f"  ... and {len(problems) - args.max_report} more")
        return 1
    print(
        f"\nOK: obfuscated build == plain build modulo the string call and the "
        f"name call, and all {sites} obfuscated constants and {names} names "
        "decode back to their plaintext"
        + (f", and the {entries} register entries are identical" if entries else "")
    )
    return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
