#!/usr/bin/env bash
# Exercise the runtime's JNI helpers against a real (desktop) JVM.
#
# The helpers in `dex2rs-runtime/src/jni.rs` are what the generated code calls
# for every cast, instance check, resolve and exception, and the bugs that reach
# a device live there rather than in the emitter. `cargo test` cannot reach them
# because JNI needs a VM, so this builds the `jvm_probe` example as a cdylib,
# compiles `tools/jvm/Probe.java` and runs it.
#
# Usage: tools/host_jvm_check.sh
#
# Exit status is non-zero when a probe fails, so it can be used in CI.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if ! command -v javac >/dev/null || ! command -v java >/dev/null; then
    echo "host_jvm_check: no JDK on PATH; skipping" >&2
    exit 0
fi

echo "building the probe library"
cargo build -q -p dex2rs-runtime --example jvm_probe

library="$(find target/debug/examples -maxdepth 1 -name 'libjvm_probe.so' | head -1)"
if [ -z "$library" ]; then
    echo "host_jvm_check: cargo did not produce libjvm_probe.so" >&2
    exit 1
fi

out="target/jvm-probe"
mkdir -p "$out"
javac -d "$out" tools/jvm/Probe.java

echo "running the probes against $(java -version 2>&1 | head -1)"
java -Dprobe.lib="$root/$library" -cp "$out" Probe
