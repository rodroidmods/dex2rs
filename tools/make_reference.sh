#!/usr/bin/env bash
# Build the reference tree `cfg_diff`/`compare_golden.py` compare against.
#
# Those two tools are the ones that catch a method whose control flow differs
# from the reference compiler's -- the bug class that hangs the app on the device
# (a block leaving its state through the wrong edge spins the dispatcher
# forever). They need a reference tree, the tree was not in the repository, and
# so `check_all.sh` reported SKIP for months: every build that reached a device
# was shipped with the hang check switched off.
#
# The tree comes from the original `d2c` in this repository, with two edits that
# only exist to let it cover the methods it refuses:
#
#   * `dcc.py` skips any method whose JNI long name passes 220 characters. That
#     is a filename limit (a `.cpp` is named after the method and the filesystem
#     stops at 255 bytes), not a judgement about the method -- but it means the
#     `$default` bridges, the big Kotlin lambdas and the service entry points
#     have no reference at all. On the test app that is 118 of 3729 methods,
#     including the one the Build button calls.
#   * files are therefore named `m#####.cpp`, with `index.tsv` recording which
#     method each holds. Both comparison tools read that index when it is there.
#
# Nothing about the *emitted code* is changed, so the tree is still the reference
# compiler's output -- only its file names differ.
#
# Usage:
#   tools/make_reference.sh INPUT.apk [OUTPUT_DIR] [FILTER]
#   tools/make_reference.sh app-release.apk target/golden filter.txt
#   GOLDEN=target/golden/jni/nc tools/check_all.sh OUT.apk --golden target/golden
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
apk="${1:?usage: make_reference.sh INPUT.apk [OUTPUT_DIR] [FILTER]}"
out="${2:-$root/target/golden}"
filter="${3:-$root/filter.txt}"

if [ ! -d "$root/d2c" ]; then
    echo "make_reference: no d2c/ directory in $root" >&2
    exit 2
fi
[ -f "$apk" ] || { echo "make_reference: no such file: $apk" >&2; exit 2; }
[ -f "$filter" ] || { echo "make_reference: no such filter: $filter" >&2; exit 2; }

apk="$(cd "$(dirname "$apk")" && pwd)/$(basename "$apk")"
filter="$(cd "$(dirname "$filter")" && pwd)/$(basename "$filter")"
case "$out" in /*) ;; *) out="$root/$out" ;; esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -r "$root/d2c" "$work/d2c"
rm -rf "$work/d2c/__pycache__"

# Patch the reference copy, in the temp directory only.
python3 - "$work/d2c/dcc.py" <<'PY'
import sys

path = sys.argv[1]
src = open(path, encoding="utf-8").read()

skip = """                if len(jni_longname) > 220:
                    Logger.debug(
                        " Name to long %s(> 220) %s" % (jni_longname, full_name)
                    )
                    continue
"""
if skip in src:
    src = src.replace(skip, "")

write = """        for method_triple, code in self.compiled_methods.items():
            full_name = JniLongName(*method_triple)
            filepath = path.join(source_dir, full_name) + ".cpp"
            if path.exists(filepath):
                Logger.warning(" Overwrite file %s %s" % (filepath, method_triple))
            try:
                with open(filepath, "w", encoding="utf-8") as fp:
                    fp.write('#include "Dex2C.h"\\n' + code)
            except Exception as e:
                print(f"{str(e)}\\n")
"""
replacement = """        index = []
        for i, (method_triple, code) in enumerate(self.compiled_methods.items()):
            full_name = JniLongName(*method_triple)
            filepath = path.join(source_dir, "m%05d.cpp" % i)
            index.append("m%05d\\t%s" % (i, full_name))
            try:
                with open(filepath, "w", encoding="utf-8") as fp:
                    fp.write('#include "Dex2C.h"\\n' + code)
            except Exception as e:
                print(f"{str(e)}\\n")
        with open(path.join(source_dir, "index.tsv"), "w", encoding="utf-8") as fp:
            fp.write("\\n".join(index))
"""
if write not in src:
    raise SystemExit("make_reference: dcc.py does not look like the version this patches")
open(path, "w", encoding="utf-8").write(src.replace(write, replacement))
PY

echo "make_reference: converting $apk"
mkdir -p "$out"
rm -rf "$out"
( cd "$work/d2c" && python3 dcc.py -i "$apk" -b -f "$filter" \
    -e "$out" -z "$out/reference.zip" )

nc="$out/jni/nc"
if [ ! -f "$nc/index.tsv" ]; then
    echo "make_reference: the reference produced no index.tsv in $nc" >&2
    exit 1
fi
count="$(wc -l < "$nc/index.tsv")"
echo "make_reference: $count methods in $nc"
echo
echo "use it with:"
echo "  python3 tools/cfg_diff.py $nc <generated>/src/methods"
echo "  python3 tools/compare_golden.py $nc --rs <generated>/src/methods"
echo "  tools/check_all.sh OUT.apk --golden $out --source-dir <generated>"
