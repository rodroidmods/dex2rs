#!/usr/bin/env python3
"""Reference output of the original (Python) d2c compiler.

Two modes:

* fixture mode (default): assemble `dex2rs-core/tests/fixtures/Hello.smali`
  with the bundled `APKEditor.jar` and print the generated C++ for every method
  — the Rust port has to stay semantically identical to it.
* `--apk APP.apk --filter filter.txt [--limit N]` : run the same compiler over a
  real APK with the same filter file and print one line per compiled method
  (`class->name(proto)` + generated C++ length) followed by a summary. Feed the
  same filter to `dex2rs` and compare: the method set has to match and the
  per-method sizes should track each other.

Usage: python3 tools/golden_cpp.py [--obfuscate] [--dynamic-register]
       python3 tools/golden_cpp.py --apk app.apk --filter filter.txt [--limit 50]
"""
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
D2C = os.path.join(ROOT, "d2c")
FIXTURE = os.path.join(ROOT, "dex2rs-core", "tests", "fixtures", "Hello.smali")
APKEDITOR = os.path.join(D2C, "tools", "APKEditor.jar")

sys.path.insert(0, D2C)


def arg_value(flag, default=None):
    if flag in sys.argv:
        return sys.argv[sys.argv.index(flag) + 1]
    return default


def assemble() -> bytes:
    """Fixture smali -> dex bytes, using the smali assembler inside APKEditor.jar."""
    tmp = tempfile.mkdtemp(prefix="golden-")
    try:
        pkg = os.path.join(tmp, "com", "example")
        os.makedirs(pkg)
        shutil.copy(FIXTURE, os.path.join(pkg, "Hello.smali"))
        dex = os.path.join(tmp, "classes.dex")
        subprocess.check_call(
            ["java", "-cp", APKEDITOR, "org.jf.smali.Main", "a", "-a", "30", "-o", dex, tmp],
            stdout=subprocess.DEVNULL,
        )
        with open(dex, "rb") as fp:
            return fp.read()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def compile_methods(dex_bytes, obfus, dynamic, filter_path=None, dex_name="classes.dex"):
    """(triple, generated source) for every method the filter selects."""
    from androguard.core.analysis import analysis
    from androguard.core.bytecodes.dvm import DalvikVMFormat
    from dex2c.compiler import Dex2C

    dex = DalvikVMFormat(dex_bytes)
    vmx = analysis.Analysis()
    vmx.add(dex)

    selected = None
    if filter_path is not None:
        import dcc

        method_filter = dcc.MethodFilter(filter_path, dex, False, False)
        selected = {str(m.get_triple()) for m in dex.get_methods() if method_filter.should_compile(m)}

    compiler = Dex2C(dex, vmx, obfus, dynamic)

    out = []
    for method in dex.get_methods():
        triple = method.get_triple()
        if selected is not None and str(triple) not in selected:
            continue
        try:
            source, prototype = compiler.get_source_method(method)
        except Exception as error:  # noqa: BLE001 - the original logs and skips
            out.append((triple, None, f"ERROR: {error}"))
            continue
        if source:
            out.append((triple, source, None))
    return out


def main() -> int:
    obfus = "--obfuscate" in sys.argv
    dynamic = "--dynamic-register" in sys.argv
    apk = arg_value("--apk")
    filter_path = arg_value("--filter")
    limit = int(arg_value("--limit", "0"))

    if apk:
        with zipfile.ZipFile(apk) as zf:
            dex_names = sorted(
                name for name in zf.namelist() if name.startswith("classes") and name.endswith(".dex")
            )
            total = 0
            compiled = 0
            errors = 0
            for name in dex_names:
                results = compile_methods(zf.read(name), obfus, dynamic, filter_path, name)
                for triple, source, error in results:
                    full = "".join(triple)
                    if error:
                        errors += 1
                        print(f"ERROR\t{full}\t{error}")
                        continue
                    if limit and compiled >= limit:
                        continue
                    compiled += 1
                    total += len(source)
                    print(f"OK\t{full}\t{len(source)}")
            print(f"# dex files: {len(dex_names)}")
            print(f"# compiled: {compiled} methods, {total} bytes ({total / max(compiled, 1):.0f}/method)")
            print(f"# errors: {errors}")
        return 0

    from androguard.core.analysis import analysis  # noqa: F401  (checked in compile_methods)
    from androguard.core.bytecodes.dvm import DalvikVMFormat
    from dex2c.compiler import Dex2C

    dex = DalvikVMFormat(assemble())
    vmx = analysis.Analysis()
    vmx.add(dex)
    compiler = Dex2C(dex, vmx, obfus, dynamic)

    for method in dex.get_methods():
        source, prototype = compiler.get_source_method(method)
        name = "".join(method.get_triple())
        print(f"===== {name} =====")
        print(source if source is not None else "<no code>")
        print(f"----- prototype: {prototype} -----")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
