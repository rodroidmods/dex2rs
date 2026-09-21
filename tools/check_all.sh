#!/usr/bin/env bash
# One command that runs every check the project has, and reports.
#
# Each tool under `tools/` covers a failure mode the others cannot see, and they
# were all written in response to a bug that reached a device:
#
#   verify.py         APK structure: dex parseable, dex version vs minSdk, every
#                     `native` method has a JNI export, loader present, zip
#                     alignment, signature schemes
#   register_check.py `--dynamic-register`: every RegisterNatives signature
#                     matches the ABI of the entry point it points at
#   arm_check.py      every dispatcher arm can leave its state (the spin/hang
#                     class of bug)
#   monitor_check.py  no path exits a method still holding a monitor (the class
#                     of bug that hangs every *other* thread)
#   exception_check.py every landing pad takes (and clears) the pending exception
#                     first, or every JNI call in the handler fails
#   cfg_diff.py       the branch/exit skeleton of every method equals the
#                     reference compiler's, when a reference tree is available
#                     (its own parser has a selftest, which always runs)
#   obf_equiv.py      an --obfuscate build differs from a plain one only in the
#                     string-constant call and the export attribute
#   watchdog_log.py   reads the log's FROZEN/PARKED/FREEZE lines back against the
#                     generated tree, so a state the dispatcher cannot loop in is
#                     never mistaken for a hang
#   host_jvm_check.sh the runtime helpers against a real (desktop) JVM: cast
#                     rules, exception dispatch, resolved call, obfuscated string
#                     round trip, RegisterNatives
#
# Usage:
#   tools/check_all.sh OUT.apk [--source-dir DIR] [--original INPUT.apk]
#                              [--golden DIR] [--skip-jvm] [--skip-tests]
#
# The source directory (the generated project holding `src/methods.rs`) is
# auto-detected from `--source-dir`, then `$DEX2RS_SOURCE_DIR`, then the newest
# `target/*/src` that looks generated. Checks whose inputs are missing are
# reported as SKIP rather than FAIL, so running this on a partial output is
# still meaningful.
#
# Exit status is non-zero if any check that ran failed.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

apk=""
source_dir=""
original=""
golden=""
plain_source_dir=""
obf_source_dir=""
run_jvm=1
run_tests=0

while [ $# -gt 0 ]; do
    case "$1" in
        --source-dir) source_dir="${2:-}"; shift 2 ;;
        --original) original="${2:-}"; shift 2 ;;
        --golden) golden="${2:-}"; shift 2 ;;
        --plain-source-dir) plain_source_dir="${2:-}"; shift 2 ;;
        --skip-jvm) run_jvm=0; shift ;;
        --skip-tests) run_tests=0; shift ;;
        --with-tests) run_tests=1; shift ;;
        -h|--help) sed -n '2,34p' "${BASH_SOURCE[0]}"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) apk="$1"; shift ;;
    esac
done

if [ -z "$apk" ]; then
    echo "usage: tools/check_all.sh OUT.apk [--source-dir DIR] [--original INPUT.apk]" >&2
    exit 2
fi
if [ ! -f "$apk" ]; then
    echo "check_all: no such APK: $apk" >&2
    exit 2
fi

# ---------------------------------------------------------------- discovery ---
# `methods.rs` is the file the generator always writes (even when the entry
# points themselves live under `methods/`), so its parent is the project's `src/`
# and the grandparent is what verify.py/register_check.py want.
find_source_dir() {
    if [ -n "$source_dir" ]; then
        echo "$source_dir"; return
    fi
    if [ -n "${DEX2RS_SOURCE_DIR:-}" ]; then
        echo "$DEX2RS_SOURCE_DIR"; return
    fi
    local newest=""
    newest="$(ls -dt target/*/src/methods.rs 2>/dev/null | head -1)"
    if [ -n "$newest" ]; then
        dirname "$(dirname "$newest")"
    fi
}
source_dir="$(find_source_dir)"
# Accept the generated *project* (a dir holding `Cargo.toml` and `src/`) or any
# path inside it that a caller happens to have to hand -- `.../src`, or
# `.../src/methods`. Everything below derives its paths from `source_dir` as the
# project root, so `--source-dir <project>/src` used to make every source check
# report SKIP ("no generated methods found") on a build whose sources were
# right there: the checks looked in `<project>/src/src/methods`.
if [ -n "$source_dir" ] && [ ! -f "$source_dir/Cargo.toml" ]; then
    for cand in "$source_dir/.." "$source_dir/../.."; do
        if [ -f "$cand/Cargo.toml" ] && [ -d "$cand/src" ]; then
            source_dir="$(cd "$cand" && pwd)"
            break
        fi
    done
fi
# Where the generated entry points actually are. The generator writes one module
# per top-level class under `src/methods/` and keeps `methods.rs` as the root
# that re-exports them, so `methods.rs` alone no longer contains any code. Checks
# that read only `methods.rs` would inspect nothing and pass -- `arm_check.py`
# now refuses to call that a pass, but point them at the right place anyway.
methods_target=""
if [ -n "$source_dir" ] && [ -d "$source_dir/src/methods" ]; then
    methods_target="$source_dir/src/methods"
elif [ -n "$source_dir" ] && [ -f "$source_dir/src/methods.rs" ]; then
    methods_target="$source_dir/src/methods.rs"
fi

# The library the conversion builds, from the generated manifest. verify.py can
# infer it from the APK, but only when the APK actually gained the new `.so`: on a
# `--no-build` run (or an app that ships its own JNI library) the guess lands on
# the app's own library, and the loader check then reports a missing
# `loadLibrary` for a library that was never meant to be loaded. The project
# knows the name, so pass it along.
lib_name=""
if [ -n "$source_dir" ] && [ -f "$source_dir/Cargo.toml" ]; then
    lib_name="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$source_dir/Cargo.toml" | head -1)"
fi

dynamic_register=0
if [ -n "$source_dir" ] && grep -q "d2c::native_method" "$source_dir/src/dynamic_register.rs" 2>/dev/null; then
    dynamic_register=1
fi

has_python=0
command -v python3 >/dev/null 2>&1 && has_python=1

# ------------------------------------------------------------------ running ---
results=()
failures=0

# A check prints its own detail; this records one line of the summary.
record() {
    local status="$1" name="$2" note="${3:-}"
    results+=("$(printf '%-6s %-22s %s' "$status" "$name" "$note")")
    [ "$status" = "FAIL" ] && failures=$((failures + 1))
    return 0
}

run_check() {
    local name="$1" out; shift
    out="$(mktemp)"
    if "$@" >"$out" 2>&1; then
        record OK "$name" "$(tail -1 "$out" | cut -c1-90)"
    else
        record FAIL "$name" "$(grep -m1 -E 'FAIL|problem|missing|error' "$out" | cut -c1-90)"
        echo
        echo "---- $name ----"
        tail -25 "$out"
    fi
    rm -f "$out"
}

echo "checking $apk"
[ -n "$source_dir" ] && echo "  source dir : $source_dir" || echo "  source dir : (not found; source checks will be skipped)"
if [ "$dynamic_register" = 1 ]; then
    echo "  registration: RegisterNatives (--dynamic-register)"
else
    echo "  registration: static JNI exports"
fi
echo

if [ "$has_python" = 1 ]; then
    verify_args=(python3 tools/verify.py "$apk")
    [ -n "$original" ] && verify_args+=(--original "$original")
    [ -n "$source_dir" ] && verify_args+=(--project "$source_dir")
    [ -n "$lib_name" ] && verify_args+=(--lib-name "$lib_name")
    [ "$dynamic_register" = 1 ] && verify_args+=(--dynamic-register)
    run_check "apk (verify.py)" "${verify_args[@]}"
else
    record SKIP "apk (verify.py)" "no python3 on PATH"
fi

if [ -n "$methods_target" ] && [ "$has_python" = 1 ]; then
    run_check "state machine (arm_check)" python3 tools/arm_check.py "$methods_target"
else
    record SKIP "state machine (arm_check)" "no generated methods found"
fi

if [ -n "$methods_target" ] && [ "$has_python" = 1 ]; then
    run_check "monitor balance" python3 tools/monitor_check.py "$methods_target"
else
    record SKIP "monitor balance" "no generated methods found"
fi

if [ -n "$methods_target" ] && [ "$has_python" = 1 ]; then
    run_check "exception handling" python3 tools/exception_check.py "$methods_target"
else
    record SKIP "exception handling" "no generated methods found"
fi

if [ -n "$source_dir" ] && [ "$has_python" = 1 ] && [ "$dynamic_register" = 1 ]; then
    run_check "register table" python3 tools/register_check.py --source-dir "$source_dir"
else
    record SKIP "register table" "not a dynamic-register build"
fi

if [ -z "$golden" ] && [ -d /tmp/goldenref/jni/nc ]; then
    golden=/tmp/goldenref
fi
# `tools/make_reference.sh` writes here. Without it the control-flow check is a
# SKIP, which is how every build reached a device with the hang check off.
if [ -z "$golden" ] && [ -d target/golden/jni/nc ]; then
    golden=target/golden
fi
if [ -z "$golden" ]; then
    # Name the remedy: this check was a silent SKIP for every build that reached a
    # device, which is how the hang class got shipped twice.
    record SKIP "control flow (cfg_diff)" "no reference tree; tools/make_reference.sh"
elif [ -z "$methods_target" ]; then
    record SKIP "control flow (cfg_diff)" "no generated methods found"
elif [ "$has_python" != 1 ]; then
    record SKIP "control flow (cfg_diff)" "no python3 on PATH"
else
    run_check "control flow (cfg_diff)" python3 tools/cfg_diff.py "$golden/jni/nc" "$methods_target"
fi

# cfg_diff reads Rust source as text, so its own parser is load-bearing for a
# check that decides whether a build ships. It once counted `{`/`}` inside a
# ciphertext literal as code and reported 41 mismatches that did not exist; the
# selftest pins that, and it needs no reference tree.
if [ "$has_python" = 1 ]; then
    run_check "cfg parser (selftest)" python3 tools/cfg_diff.py --selftest
else
    record SKIP "cfg parser (selftest)" "no python3 on PATH"
fi

# A watchdog line is a `(method, state)` pair, and what it means is a property of
# the arm that owns that state -- a bare `return` cannot be looped in, so a line
# naming one is not a freeze at all. The classifier that does that lookup is
# itself checked against a fixture, the same way the log's own selftest is.
if [ "$has_python" = 1 ]; then
    run_check "watchdog log (selftest)" python3 tools/watchdog_log.py --selftest
else
    record SKIP "watchdog log (selftest)" "no python3 on PATH"
fi

# `--obfuscate` rewrites every `const-string` into a ciphertext literal plus a
# seed. That rewrite is only allowed to change the string call: everything else
# -- branches, states, arms, calls -- must be identical to the plain build, which
# is the one tested on a device. Find the *other* tree by looking for the marker,
# so this runs whichever kind of build is being checked.
# Which kind of a tree is this? `obf` is an --obfuscate build with the seeded
# ciphertext (what the option emits now); `old-obf` is the earlier generation,
# whose literals are the plaintext (it predates the seed). Only `obf` is compared
# against `plain`: comparing two obfuscation generations would report the change
# in the scheme itself as a difference in the code, which is not what this asks.
tree_kind() {
    if grep -rq "new_string_utf_obf_seeded" "$1/methods" 2>/dev/null; then
        echo obf
    elif grep -rq "new_string_utf_obf(" "$1/methods" 2>/dev/null; then
        echo old-obf
    else
        echo plain
    fi
}

# `--dynamic-register` or not, read off the generated tree.
#
# `obf_equiv` compares a table of `native_method` entries on one side against the
# other, so a plain tree from a *static* build cannot be paired with an obfuscated
# one from a dynamic build: one has a register table and the other has none. The
# two are the same conversion otherwise, which is why this is a pairing rule here
# rather than a failure in the comparison.
reg_mode() {
    if grep -q "d2c::native_method" "$1/dynamic_register.rs" 2>/dev/null; then
        echo dynamic
    else
        echo static
    fi
}

if [ -n "$source_dir" ]; then
    current_kind="$(tree_kind "$source_dir/src")"
    if [ "$current_kind" = "obf" ]; then
        obf_source_dir="${obf_source_dir:-$source_dir}"
        want=plain
    else
        plain_source_dir="${plain_source_dir:-$source_dir}"
        want=obf
    fi
    if { [ "$want" = "plain" ] && [ -z "$plain_source_dir" ]; } \
        || { [ "$want" = "obf" ] && [ -z "$obf_source_dir" ]; }; then
        for cand in $(ls -dt target/*/src 2>/dev/null); do
            [ -f "$cand/methods.rs" ] || continue
            [ "$(cd "$cand" && pwd)" = "$source_dir" ] && continue
            [ "$(tree_kind "$cand")" = "$want" ] || continue
            [ "$(reg_mode "$cand")" = "$(reg_mode "$source_dir/src")" ] || continue
            if [ "$want" = "obf" ]; then obf_source_dir="$cand"; else plain_source_dir="$cand"; fi
            break
        done
    fi
fi
if [ -n "$plain_source_dir" ] && [ -n "$obf_source_dir" ] && [ "$has_python" = 1 ]; then
    run_check "obfuscation equivalence" python3 tools/obf_equiv.py \
        "$plain_source_dir" "$obf_source_dir"
else
    record SKIP "obfuscation equivalence" "only one generated tree available"
fi

if [ "$run_jvm" = 1 ]; then
    if command -v javac >/dev/null 2>&1; then
        run_check "runtime vs real JVM" ./tools/host_jvm_check.sh
    else
        record SKIP "runtime vs real JVM" "no JDK on PATH"
    fi
else
    record SKIP "runtime vs real JVM" "--skip-jvm"
fi

if [ "$run_tests" = 1 ]; then
    run_check "cargo test" cargo test --workspace --quiet
fi

echo "==================== summary ===================="
for line in "${results[@]}"; do echo "$line"; done
echo "================================================="
if [ "$failures" -gt 0 ]; then
    echo "$failures check(s) FAILED"
    exit 1
fi
echo "all checks passed"
