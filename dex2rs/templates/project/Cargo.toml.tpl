[workspace]

[package]
# The CLI rewrites `name`/`[lib] name` from `--lib-name`.
name = "stub"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "stub"
crate-type = ["cdylib"]
path = "src/lib.rs"

[dependencies]
# `runtime/` is a copy of the `dex2rs-runtime` crate, exactly like the original
# project shipped `jni/nc/*.cpp` inside the generated project folder.
dex2rs-runtime = { path = "runtime", default-features = false }
jni-sys = "0.4"

[profile.release]
# This line is rewritten by the CLI (`--opt-level`, default `z`). It moves the
# size of the converted library, and the split is not the one people expect.
# Measured on a real app (3 729 methods, `--obfuscate --dynamic-register`,
# arm64) -- all four rows from the same generated tree, arm64, API 26:
#
#     opt-level = 3                     12.37 MB libstub.so   11.22 MB .text
#     opt-level = 3, inliner disabled   11.02 MB libstub.so    9.90 MB .text
#     opt-level = 3, outliner forced on  8.81 MB libstub.so    6.76 MB .text
#     opt-level = "z"                    7.64 MB libstub.so    5.74 MB .text
#
# So inlining is *not* what makes `-O3` big: disabling it (`-C
# llvm-args=--inline-threshold=0`) removes 1.32 MB of 4.73 MB and barely moves
# the function count (4 431 -> 4 485). The rest is LLVM's **machine outliner**,
# which runs at `-Oz` and not at `-O3`, and which undoes exactly the duplication
# `-O3`'s inliner creates: 30 066 short runtime-helper sequences -- `Arc::clone`,
# vtable dispatch, discriminant tests -- that had been copied up to 8 291 times
# each, i.e. 447 761 `bl` sites. Function count 4 431 -> 36 594 at `-Oz`'s 36 471.
# `--outline-common` asks for that same outlining at `opt-level 3`. See that
# flag's help for the trade.
#
# `2` (12.67 MB) and `"s"` (13.20 MB) both measured *larger* than `3` here, so
# this is not a dial: it is `3` with or without outlining, or `z`.
#
# Neither value changes what is generated, only how it is compiled.
#
# `panic = "abort"`: a bug in the generated code must never unwind across a JNI
# frame.
#
# The generated code is one module per class (`src/methods.rs` re-exports
# them), so a one-class change only re-parses that file. Add `incremental = true`
# here while iterating: it takes re-checking a 3 729-method build from 34 s to
# 13 s. It is left off by default because incremental artefacts are a known
# source of stale-codegen bugs and this profile produces the shipped library --
# add it for a work-in-progress build, not for the one you sign.
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
strip = true
debug = false
overflow-checks = false
