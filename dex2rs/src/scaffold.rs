//! Generated project scaffolding.
//!
//! Mirrors dcc.py's `write_compiled_methods` / `write_dynamic_register` /
//! `archive_compiled_code`. What the original kept as a `project/` folder next to
//! the tool is embedded here instead: the CLI has to scaffold from a published
//! crate, where there is no checkout to copy from (and a nested package cannot
//! even travel in one -- see [`PROJECT_FILES`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use smali::android::zip::{ApkEntry, ApkFile};

use crate::filter::MethodTriple;

/// One compiled method: its generated Rust plus what `RegisterNatives` needs.
#[derive(Debug, Clone)]
pub struct GeneratedMethod {
    pub triple: MethodTriple,
    /// JNI long name, i.e. the `#[no_mangle]` symbol (`Java_com_example_Hello_add__II`).
    pub jni_name: String,
    /// The JNI prototype (`(II)I`) used as `RegisterNatives`' signature.
    pub prototype: String,
    pub source: String,
}

/// The generated project's own files, as `(path, contents)`.
///
/// Embedded, not read from a directory: `cargo install dex2rs` has no checkout to
/// read from, and a published crate cannot carry the template directory as a
/// package either -- `cargo package` leaves nested packages out of the tarball
/// (an explicit `include` does not override that). So the template manifest is
/// stored as `Cargo.toml.tpl`, a name that makes the directory *not* a package,
/// and written out as `Cargo.toml`.
const PROJECT_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../templates/project/Cargo.toml.tpl"),
    ),
    (
        "src/lib.rs",
        include_str!("../templates/project/src/lib.rs"),
    ),
    (
        "src/methods.rs",
        include_str!("../templates/project/src/methods.rs"),
    ),
    (
        "src/dynamic_register.rs",
        include_str!("../templates/project/src/dynamic_register.rs"),
    ),
];

/// Write the generated project's crate into `project_dir` and set its library
/// name.
pub fn create_project(project_dir: &Path, lib_name: &str) -> Result<()> {
    if project_dir.exists() {
        std::fs::remove_dir_all(project_dir)
            .with_context(|| format!("clearing {}", project_dir.display()))?;
    }
    for (path, contents) in PROJECT_FILES {
        write_file(&project_dir.join(path), contents)?;
    }
    vendor_runtime(project_dir)?;

    let manifest = project_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let text = text.replace("name = \"stub\"", &format!("name = \"{lib_name}\""));
    write_if_changed(&manifest, &text)?;
    Ok(())
}

/// Vendor the runtime crate into `project_dir/runtime` (`runtime/` is the
/// equivalent of the original project's `jni/nc/`).
///
/// The sources come from `dex2rs-runtime` itself, so the runtime a generated
/// project builds is the one this binary was compiled against -- byte for byte,
/// whether it came from a checkout or from crates.io, and with no `DEX2RS_RUNTIME`
/// to set and no layout to guess.
///
/// Public because it is also called when a `--source-dir` is reused: the runtime
/// is this tool's code, not the project's, so a reused tree that kept the copy
/// from whenever it was first scaffolded would silently build a `libstub.so` from
/// a stale runtime -- the generated `methods/` would be current while the JNI
/// helpers underneath them were not.
pub fn vendor_runtime(project_dir: &Path) -> Result<()> {
    let target = project_dir.join("runtime");
    for (path, contents) in dex2rs_runtime::vendored::SOURCES {
        write_file(&target.join(path), contents)?;
    }
    write_if_changed(
        &target.join("Cargo.toml"),
        dex2rs_runtime::vendored::STANDALONE_MANIFEST,
    )?;
    Ok(())
}

/// Write `contents` to `path`, creating the parent directories, and only when
/// the file differs -- see [`write_if_changed`] for why that matters.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_if_changed(path, contents)
}

/// Turn the runtime's `trace` feature on/off in a generated project.
///
/// `d2c::trace(...)` is a no-op unless the feature is enabled, so without this a
/// `--trace` build would produce the calls and log nothing on the device.
pub fn set_trace_feature(project_dir: &Path, enabled: bool) -> Result<()> {
    const PLAIN: &str = "dex2rs-runtime = { path = \"runtime\", default-features = false }";
    const TRACED: &str =
        "dex2rs-runtime = { path = \"runtime\", default-features = false, features = [\"trace\"] }";
    let path = project_dir.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let (from, to) = if enabled {
        (PLAIN, TRACED)
    } else {
        (TRACED, PLAIN)
    };
    if !text.contains(from) {
        return Ok(());
    }
    std::fs::write(&path, text.replace(from, to))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Rewrite the generated project's `opt-level`.
///
/// The template ships `opt-level = 3`, which is a *size* decision as much as a
/// speed one and is worth measuring before trusting: on the test app (3 729
/// methods, `--obfuscate --dynamic-register`, arm64) `opt-level = 3` produced a
/// 12.37 MB `libstub.so` with 11.22 MB of `.text`, and `opt-level = "z"` produced
/// 7.65 MB with 5.74 MB of `.text` — the difference is `-O3` inlining the runtime
/// helpers and the dispatcher prologue into every one of the 3 729 entry points.
/// `2` (12.67 MB) and `"s"` (13.20 MB) are both *larger* than `3` here, so this is
/// a two-way choice rather than a dial.
///
/// `panic = "abort"` already rules out unwinding; nothing else in the profile
/// changes, so the value only trades code size against dispatch speed.
pub fn set_opt_level(project_dir: &Path, level: &str) -> Result<()> {
    let path = project_dir.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = String::with_capacity(text.len());
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("opt-level") && line.contains('=') {
            let indent = &line[..line.len() - line.trim_start().len()];
            let quoted = level.chars().any(|c| !c.is_ascii_digit());
            let value = if quoted {
                format!("\"{level}\"")
            } else {
                level.to_string()
            };
            out.push_str(&format!("{indent}opt-level = {value}\n"));
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        bail!(
            "no `opt-level` line in {}, so --opt-level could not be applied",
            path.display()
        );
    }
    write_if_changed(&path, &out)?;
    Ok(())
}

/// Add or remove the machine-outliner flag in the generated project's
/// `.cargo/config.toml` (`--outline-common`).
///
/// This exists because `-Oz`'s size advantage is mostly *not* what the size
/// story usually credits: building `opt-level 3` with the inliner disabled
/// (`-C llvm-args=--inline-threshold=0`) removes only 1.32 MB of 12.37 MB and
/// barely moves the function count (4 431 -> 4 485). The other 4.46 MB of
/// `.text` is LLVM's machine outliner, which runs at `-Oz` and not at `-O3`, and
/// which undoes exactly the duplication `-O3`'s inliner creates: 30 066 short
/// runtime-helper sequences (`Arc::clone`, vtable dispatch, discriminant tests)
/// that had been copied up to 8 291 times each, 447 761 `bl` sites in total.
/// Forcing it on at `-O3` takes the reference app from 12.37 MB to **8.81 MB**
/// -- `.text` 11.22 MB to 6.76 MB, function count 4 431 to 36 594 -- while
/// everything outside the outlined fragments is still `-O3` code. `-Oz` remains
/// smaller at 7.64 MB. The price is one `bl` where the code was straight-line,
/// which is why this is opt-in.
///
/// The file lives in the project rather than in the `cargo ndk` environment so a
/// tree reused through `--source-dir`, and the `project-source.zip` archive,
/// rebuild by hand exactly what this run produced. When the flag is off the file
/// is removed, so a reused tree cannot keep outlining from an earlier run.
///
/// Scoped to Android targets rather than `[build]`: a `[build]` rustflags entry
/// applies to every crate cargo compiles in this project, host-side build scripts
/// and proc macros included, which is the last place an unusual LLVM backend flag
/// belongs. The outlining is wanted for the converted library, so it is asked for
/// on the target that library is built for.
pub fn set_outline_common(project_dir: &Path, outline: bool) -> Result<()> {
    let dir = project_dir.join(".cargo");
    let path = dir.join("config.toml");
    if !outline {
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            // Only succeeds if this file was the only thing in the directory.
            let _ = std::fs::remove_dir(&dir);
        }
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = "# Written by `dex2rs --outline-common`; delete this file to build \
                without it.\n\
                #\n\
                # The generated code repeats the same instruction sequences once per\n\
                # JNI entry point. `-O3` leaves them duplicated; `-Oz` runs LLVM's\n\
                # machine outliner, which factors them into shared copies, and that\n\
                # is most of why `-Oz` is smaller. This asks for the same outlining\n\
                # while everything else stays `-O3`: 12.37 MB -> 8.81 MB on the\n\
                # reference app, at the cost of a call where `-O3` inlined.\n\
                [target.'cfg(target_os = \"android\")']\n\
                rustflags = [\"-C\", \"llvm-args=--enable-machine-outliner=always\"]\n";
    write_if_changed(&path, text)?;
    Ok(())
}

const METHODS_HEADER: &str = "//! Generated by `dex2rs`. Do not edit.\n//!\n\
     //! One JNI entry point per compiled Dalvik method.\n\n\
     #![allow(clippy::all, unused_parens, non_snake_case)]\n\n\
     // The generated signatures refer to `d2c::...`, so the alias has to be\n\
     // in scope at module level (the C++ build had it in every file's\n\
     // includes).\n\
     use dex2rs_runtime as d2c;\n\n";

/// Write the generated entry points, one module per *top-level* class.
///
/// dcc.py wrote one `.cpp` per method; a single Rust module works, but the
/// converted code of a real app is the whole app plus its libraries — tens of
/// megabytes of one file. rustc parses and type-checks that file before it can
/// emit anything, so widening the filter past the app's own package meant a
/// build that peaked in the gigabytes and took minutes even for a one-method
/// change. One module per class keeps each file small enough to compile on its
/// own and lets an edit invalidate only the class it touched.
///
/// Anonymous, lambda and other compiler-generated classes are folded into the
/// module of the class that declares them (`Lcom/x/Foo$bar$2;` lives in
/// `Foo`'s module, not in a `Foo_bar_2` of its own). Kotlin and Java emit those
/// by the hundred for one source file — `Foo` plus `Foo$1`, `Foo$2`,
/// `Foo$when$0`, `Foo$$ExternalSyntheticLambda0` — and each one carries a
/// handful of methods, so giving them their own file multiplies the module
/// count for no compile-time benefit and scatters a class's logic across files
/// whose names are just mangled class paths. Grouping by the outer class keeps
/// the file name meaningful (`...Foo.rs`) and drops the numeric-disambiguation
/// suffix entirely for every class the sanitised path does not collide on.
///
/// `methods.rs` stays the root module, re-exporting each generated class, so
/// `lib.rs` and `dynamic_register.rs` (`methods::Java_...`) are unaffected and
/// the exported `Java_*` symbols do not change.
pub fn write_methods(
    project_dir: &Path,
    methods: &BTreeMap<MethodTriple, GeneratedMethod>,
) -> Result<()> {
    // Grouped by the declaring top-level class, in the map's own order so the
    // output is deterministic.
    let mut per_class: BTreeMap<String, Vec<&GeneratedMethod>> = BTreeMap::new();
    for method in methods.values() {
        per_class
            .entry(outer_class(&method.triple.0))
            .or_default()
            .push(method);
    }
    // The outer class's own methods open the file, then its nested classes in
    // class-path order. The methods arrive sorted by class path, and `$` sorts
    // before `;`, so without this a class's nested classes would come first.
    for (outer, class_methods) in per_class.iter_mut() {
        class_methods.sort_by(|a, b| {
            (a.triple.0 != *outer, &a.triple.0, &a.triple.1, &a.triple.2).cmp(&(
                b.triple.0 != *outer,
                &b.triple.0,
                &b.triple.1,
                &b.triple.2,
            ))
        });
    }

    let mut root = String::from(
        "//! Generated by `dex2rs`. Do not edit.\n//!\n\
         //! One JNI entry point per compiled Dalvik method, one module per\n\
         //! top-level class with its nested and synthetic classes folded in\n\
         //! (see `write_methods` in `src/scaffold.rs` for why).\n\n",
    );
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for (class, class_methods) in &per_class {
        let module = module_name(class, &mut used);
        root.push_str(&format!("pub mod {module};\npub use {module}::*;\n"));
        let mut body = String::from(METHODS_HEADER);
        for method in class_methods {
            body.push_str(&method.source);
            body.push('\n');
        }
        files.push((
            project_dir
                .join("src")
                .join("methods")
                .join(format!("{module}.rs")),
            body,
        ));
    }

    let dir = project_dir.join("src").join("methods");
    if !files.is_empty() {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Re-generating an identical file would bump its mtime and force cargo to
    // rebuild (and relink with LTO) the whole generated crate. Reusing a
    // `--source-dir` that was already built is the common case when iterating,
    // and with one file per class an unchanged class is now *also* skipped by
    // cargo itself.
    write_if_changed(&project_dir.join("src/methods.rs"), &root)?;
    let mut written: BTreeSet<PathBuf> = BTreeSet::new();
    for (path, body) in &files {
        write_if_changed(path, body)?;
        written.insert(path.clone());
    }
    // A `--source-dir` generated under a different grouping (or with a wider
    // filter) can leave module files behind that `methods.rs` no longer
    // declares. They are not compiled, but leaving them means the directory
    // stops describing the build, and `tools/register_check.py` would count
    // their entry points twice. Only files this run did not write are removed,
    // so an unchanged build still touches nothing.
    if dir.is_dir() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "rs") && !written.contains(&path) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing stale {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// The class whose module holds a class's generated code.
///
/// `$` introduces a nested, anonymous or synthetic class (`Lcom/x/Foo$1;`,
/// `Lcom/x/Foo$bar$2;`, `Lcom/x/Foo$$ExternalSyntheticLambda0;`); everything
/// from the first `$` on belongs to the class in front of it. A class path with
/// no `$` is its own outer class, so this is a no-op for ordinary classes.
///
/// The result must be byte-for-byte the outer class's *own* path, `;` included,
/// or a nested class would land in a different module than the class it came
/// from (`Foo$1;` and `Foo;` have to produce the same key). A `$` that starts
/// the class name rather than following one (`L$Foo;`, a top-level class called
/// `$Foo` in the default package) is left alone for the same reason.
fn outer_class(class: &str) -> String {
    let Some(at) = class.find('$') else {
        return class.to_string();
    };
    // The name part between the last `/` (or the `L` of the descriptor) and the
    // first `$` has to be non-empty for the `$` to mean nesting.
    let name_start = class.rfind('/').map_or(1, |slash| slash + 1);
    if at <= name_start {
        return class.to_string();
    }
    let mut outer = class[..at].to_string();
    outer.push(';');
    outer
}

/// `src/methods/<module>.rs` names: a class path is not a legal Rust identifier
/// (`/`, `$`, `;`), so it is sanitised, and a name already taken gets a numbered
/// suffix. Deterministic in the iteration order, so an unchanged build produces
/// the same file names and cargo can still reuse them.
///
/// Nested classes never reach here — they are folded into their outer class's
/// module first (see `outer_class`), so the numbered suffix is only reached when
/// two *different* top-level classes sanitise to the same name (`La/b;` and
/// `La_b;` both want `a_b`).
fn module_name(class: &str, used: &mut BTreeSet<String>) -> String {
    let stripped = class.trim_start_matches('L').trim_end_matches(';');
    let mut base: String = stripped
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
        base.insert(0, '_');
    }
    // `Self`/`super`/`crate` and the 2015+ keywords would all be rejected as a
    // module name.
    if RUST_KEYWORDS.contains(&base.as_str()) {
        base.push('_');
    }
    let mut name = base.clone();
    let mut n = 2;
    while !used.insert(name.clone()) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

/// Words that cannot be used as a module name.
const RUST_KEYWORDS: [&str; 15] = [
    "as", "break", "const", "crate", "else", "fn", "for", "if", "impl", "let", "loop", "mod",
    "self", "super", "type",
];

/// Write only when the content differs, so cargo's timestamp check is not
/// needlessly invalidated.
fn write_if_changed(path: &Path, body: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == body) {
        return Ok(());
    }
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Write `src/dynamic_register.rs`: the real `RegisterNatives` table with
/// `--dynamic-register`, otherwise the dummy that the C++ build also shipped.
pub fn write_dynamic_register(
    project_dir: &Path,
    methods: &BTreeMap<MethodTriple, GeneratedMethod>,
    dynamic_register: bool,
    obfuscate: bool,
) -> Result<()> {
    let path = project_dir.join("src/dynamic_register.rs");
    if !dynamic_register {
        std::fs::write(
            &path,
            "//! Static registration: the entry points are exported as `Java_*`\n\
             //! symbols and ART binds them by name (dcc.py's dummy\n\
             //! `DynamicRegister.cpp`).\n\n\
             use dex2rs_runtime as d2c;\n\n\
             pub unsafe fn register(_env: d2c::Env) -> Result<(), String> {\n    Ok(())\n}\n",
        )
        .with_context(|| format!("writing {}", path.display()))?;
        return Ok(());
    }

    if methods.is_empty() {
        bail!("no methods to register");
    }

    // `RegisterNatives` works per class, and dcc.py emitted the classes in
    // sorted order with their methods in the order they were discovered.
    let mut per_class: BTreeMap<String, Vec<&GeneratedMethod>> = BTreeMap::new();
    for method in methods.values() {
        let class_path = method
            .triple
            .0
            .trim_start_matches('L')
            .trim_end_matches(';');
        per_class
            .entry(class_path.to_string())
            .or_default()
            .push(method);
    }

    let mut out = String::from(
        "//! Generated by `dex2rs`: the `RegisterNatives` table for every converted\n\
         //! class (dcc.py's `DynamicRegister.cpp`).\n\n\
         use dex2rs_runtime as d2c;\n\n\
         use crate::methods;\n\n\
         pub unsafe fn register(env: d2c::Env) -> Result<(), String> {\n\
         \x20   unsafe {\n",
    );
    for (index, (class_path, class_methods)) in per_class.iter().enumerate() {
        // Under `--obfuscate` the table holds `ObfMethod`s, whose name and
        // signature are ciphertext the runtime decodes at registration time.
        // `RegisterNatives` needs the real JNI signature to build the call, and
        // that signature names the parameter and return classes -- on an
        // obfuscated app, the app's own (`(Lb0/d0;)V`). A table of C strings
        // therefore read back every class the conversion exists to hide.
        let entry_type = if obfuscate {
            "d2c::ObfMethod"
        } else {
            "d2c::jni_sys::JNINativeMethod"
        };
        out.push_str(&format!(
            "        // {class_path}\n        let table_{index}: [{entry_type}; {}] = [\n",
            class_methods.len()
        ));
        for method in class_methods {
            if obfuscate {
                out.push_str(&format!(
                    "            d2c::native_method_obf(\n                {},\n                {},\n                methods::{} as *const (),\n            ),\n",
                    obf_name_arg(method.triple.1.as_bytes())?,
                    obf_name_arg(method.prototype.as_bytes())?,
                    method.jni_name
                ));
            } else {
                out.push_str(&format!(
                    "            d2c::native_method(\n                c\"{}\",\n                c\"{}\",\n                methods::{} as *const (),\n            ),\n",
                    escape(method.triple.1.as_bytes()),
                    escape(method.prototype.as_bytes()),
                    method.jni_name
                ));
            }
        }
        out.push_str("        ];\n");
        // The class is registered by name, so under `--obfuscate` that name is
        // encrypted like every other reflected one: a `--dynamic-register` build
        // would otherwise leave the app's class paths readable in the library,
        // which is the one place the whole pass exists to keep them out of. The
        // failure message says which table instead of naming the class, for the
        // same reason (the comment above it is source-only).
        let class_arg = if obfuscate {
            obf_name_arg(class_path.as_bytes())?
        } else {
            format!("b\"{}\"", escape(class_path.as_bytes()))
        };
        let register_fn = if obfuscate {
            "register_natives_obf"
        } else {
            "register_natives"
        };
        let not_found = if obfuscate {
            format!("RegisterNatives: class {index} not found")
        } else {
            format!("Class not found: {class_path}")
        };
        out.push_str(&format!(
            "        if !d2c::{register_fn}(env, {class_arg}, &table_{index}) {{\n            return Err(\"{not_found}\".to_string());\n        }}\n\n"
        ));
    }
    out.push_str("        Ok(())\n    }\n}\n");

    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Escape byte strings for a Rust `b"..."` literal.
/// One `--obfuscate` name argument: `d2c::Name::obf(b"\x..", 0xSEED)`.
///
/// Shared by the `RegisterNatives` table's method names and signatures and by the
/// class it registers against, so all three are encrypted the same way.
///
/// Refuses a name the runtime cannot decode, for the same reason `codegen::name_arg`
/// does: the runtime decodes into a `MAX_NAME` buffer and reports failure rather
/// than truncating, and for a register table that failure is a class that silently
/// never registers. Signatures reach this too, and they are the longest strings in
/// the generated crate (468 bytes on the test app, against a 1024 byte bound).
fn obf_name_arg(name: &[u8]) -> Result<String> {
    anyhow::ensure!(
        name.len() < dex2rs_core::codegen::MAX_OBF_NAME,
        "a {} byte RegisterNatives name or signature exceeds the {} byte limit the \
         runtime decodes (raise MAX_NAME in dex2rs-runtime and MAX_OBF_NAME here): {}",
        name.len(),
        dex2rs_core::codegen::MAX_OBF_NAME,
        String::from_utf8_lossy(name)
    );
    let (seed, cipher) = dex2rs_core::codegen::obfuscate_bytes(name);
    Ok(format!(
        "d2c::Name::obf({}, {seed:#010x})",
        dex2rs_core::codegen::bytes_literal(&cipher)
    ))
}

fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Directories inside a generated project that hold build output rather than
/// source, and so never belong in the source archive.
///
/// `target/` is not a detail: a project that has been built once has a cargo
/// target directory measured in gigabytes, and packing it would copy all of it
/// into the archive (`ApkFile::from_directory` reads every file into memory
/// first, so it would also read every byte of it). `libs/` holds the compiled
/// libraries, which are the input to an APK, not source.
const ARCHIVE_SKIP: [&str; 3] = ["target", "libs", ".git"];

/// Where `build_project` records which sources produced the libraries in
/// `libs/`, for `--no-build` to check before packing them.
pub const BUILD_RECORD: &str = ".dex2rs-sources";

/// FNV-1a, over the paths and contents of every source file in the project.
///
/// A plain hash function rather than a dependency, and it only has to answer
/// "are these the same sources", not resist an adversary.
fn hash_bytes(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// A fingerprint of everything the generated library is compiled from: the
/// manifest (which carries `opt-level` and the outliner flag), the vendored
/// runtime, the generated methods, and `Cargo.lock`.
///
/// This exists because `--no-build` packs whatever is in `libs/`, and the one
/// thing that makes that safe or unsafe is whether those libraries came from
/// *these* sources. The dex marks methods `native` according to the filter that
/// just ran, while the library registers the methods from whenever it was
/// compiled: if they disagree, the build is clean and the device throws
/// `UnsatisfiedLinkError` (or runs a stale implementation) at call time. That is
/// not something to notice from a log, so the build records this value and
/// `--no-build` compares it.
///
/// Contents, not timestamps: the generator rewrites its own sources on every
/// run, so every mtime in the tree moves while the code stays the same.
pub fn sources_fingerprint(project_dir: &Path) -> Result<String> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let walk = walkdir::WalkDir::new(project_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_dir() && ARCHIVE_SKIP.contains(&name.as_ref()))
        });
    for entry in walk {
        let entry = entry.with_context(|| format!("walking {}", project_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(project_dir)
            .with_context(|| format!("relocating {}", entry.path().display()))?
            .to_string_lossy()
            .replace('\\', "/");
        files.push((relative, entry.path().to_path_buf()));
    }
    // The walk order is the filesystem's; sort so the fingerprint does not
    // depend on it.
    files.sort();
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for (relative, path) in files {
        hash_bytes(&mut state, relative.as_bytes());
        hash_bytes(&mut state, &[0]);
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        hash_bytes(&mut state, &data);
        hash_bytes(&mut state, &[0]);
    }
    Ok(format!("{state:016x}"))
}

/// Record which sources produced the libraries now sitting in `libs/`.
pub fn record_build(project_dir: &Path) -> Result<()> {
    let fingerprint = sources_fingerprint(project_dir)?;
    let path = project_dir.join("libs").join(BUILD_RECORD);
    std::fs::write(&path, fingerprint.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The fingerprint `record_build` wrote, if a build here produced it.
/// `None` means `libs/` holds libraries this tool did not build, so their
/// provenance cannot be checked.
pub fn recorded_build(project_dir: &Path) -> Option<String> {
    std::fs::read_to_string(project_dir.join("libs").join(BUILD_RECORD))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// dcc.py's `archive_compiled_code`: zip the generated project (`--no-build`).
///
/// Walks the project instead of calling `ApkFile::from_directory`, so skipped
/// directories are never opened. `from_directory` would also flatten the
/// executable bit off any script in the template, so the mode is carried over
/// explicitly.
pub fn archive_project(project_dir: &Path, archive: &Path) -> Result<()> {
    let mut packed = ApkFile::new();
    let walk = walkdir::WalkDir::new(project_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(entry.file_type().is_dir() && ARCHIVE_SKIP.contains(&name.as_ref()))
        });
    for entry in walk {
        let entry = entry.with_context(|| format!("walking {}", project_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(project_dir)
            .with_context(|| format!("relocating {}", entry.path().display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let data = std::fs::read(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        let mut item = ApkEntry::new(data);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = entry.metadata() {
                item = item.with_mode(Some(meta.permissions().mode() & 0o777));
            }
        }
        packed
            .replace_entry_with(relative, item)
            .with_context(|| format!("adding {} to the archive", entry.path().display()))?;
    }
    packed
        .write_to_file(archive)
        .with_context(|| format!("writing {}", archive.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source archive is a deliverable, so build output must not be in it.
    /// `target/` is the reason this is not cosmetic: `ApkFile::from_directory`
    /// reads every file it packs, and a cargo target directory is gigabytes.
    #[test]
    fn the_archive_leaves_out_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(project.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(project.join("libs/arm64-v8a")).unwrap();
        std::fs::write(project.join("src/lib.rs"), b"fn main() {}").unwrap();
        std::fs::write(project.join("target/debug/deps/artifact"), b"junk").unwrap();
        std::fs::write(project.join("libs/arm64-v8a/libstub.so"), b"elf").unwrap();

        let archive = project.join("src.zip");
        archive_project(project, &archive).unwrap();
        let packed = ApkFile::from_file(&archive).unwrap();
        let names: Vec<&str> = packed.entry_names().collect();
        assert!(
            names.contains(&"src/lib.rs"),
            "sources are packed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("target/")),
            "no target/: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("libs/")),
            "no libs/: {names:?}"
        );
    }

    /// The fingerprint answers "were the libraries built from these sources",
    /// so it must follow contents (the generator rewrites its own files every
    /// run, so timestamps all move while the code does not) and ignore the
    /// directories no build reads.
    #[test]
    fn the_fingerprint_follows_contents_not_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/lib.rs"), b"one").unwrap();

        let first = sources_fingerprint(project).unwrap();
        assert_eq!(first, sources_fingerprint(project).unwrap(), "stable");

        // A rewrite with identical bytes must not change it.
        std::fs::write(project.join("src/lib.rs"), b"one").unwrap();
        assert_eq!(first, sources_fingerprint(project).unwrap(), "same bytes");

        // A changed file must, and so must a new one.
        std::fs::write(project.join("src/lib.rs"), b"two").unwrap();
        let changed = sources_fingerprint(project).unwrap();
        assert_ne!(first, changed, "changed contents");
        std::fs::write(project.join("src/extra.rs"), b"").unwrap();
        assert_ne!(changed, sources_fingerprint(project).unwrap(), "new file");

        // Build output is not part of the sources.
        let before = sources_fingerprint(project).unwrap();
        std::fs::create_dir_all(project.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(project.join("libs/arm64-v8a")).unwrap();
        std::fs::write(project.join("target/debug/deps/artifact"), b"junk").unwrap();
        std::fs::write(project.join("libs/arm64-v8a/libstub.so"), b"elf").unwrap();
        assert_eq!(
            before,
            sources_fingerprint(project).unwrap(),
            "target/libs ignored"
        );
    }

    /// A library this tool built can be checked; one it did not says so.
    #[test]
    fn a_build_is_recorded_and_can_be_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/lib.rs"), b"one").unwrap();
        assert_eq!(recorded_build(project), None, "nothing built yet");

        std::fs::create_dir_all(project.join("libs/arm64-v8a")).unwrap();
        record_build(project).unwrap();
        assert_eq!(
            recorded_build(project).unwrap(),
            sources_fingerprint(project).unwrap()
        );

        // Changing a source after the build must show up as a mismatch, which
        // is the whole point: the dex would be patched for the new methods
        // while the library still implements the old ones.
        std::fs::write(project.join("src/lib.rs"), b"two").unwrap();
        assert_ne!(
            recorded_build(project).unwrap(),
            sources_fingerprint(project).unwrap()
        );
    }

    /// `--opt-level` rewrites the one line that decides the library's size, and
    /// spells it the way cargo wants (`3` unquoted, `"z"` quoted).
    #[test]
    fn opt_level_is_rewritten_in_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        let write = |body: &str| std::fs::write(&manifest, body).unwrap();
        let read = || std::fs::read_to_string(&manifest).unwrap();
        let template = "[profile.release]\nopt-level = 3\nlto = true\n";

        write(template);
        set_opt_level(dir.path(), "z").unwrap();
        assert_eq!(read(), "[profile.release]\nopt-level = \"z\"\nlto = true\n");

        // The other direction has to work too, or a project generated with one
        // value could never be switched back.
        set_opt_level(dir.path(), "3").unwrap();
        assert_eq!(read(), template);

        // Only the profile line is touched: the same text appears in the comment
        // block the template ships with.
        write("[profile.release]\n# opt-level = 3 is the fast one\nopt-level = 3\n");
        set_opt_level(dir.path(), "z").unwrap();
        assert_eq!(
            read(),
            "[profile.release]\n# opt-level = 3 is the fast one\nopt-level = \"z\"\n"
        );

        // A manifest with no such line is refused rather than silently ignoring
        // the flag, which would ship a build whose size nobody can explain.
        write("[profile.release]\nlto = true\n");
        assert!(set_opt_level(dir.path(), "z").is_err());
    }

    /// `--outline-common` writes the outliner flag where a by-hand rebuild will
    /// find it, leaves the file alone when it is already right (rewriting it
    /// would bump its mtime, invalidate cargo's fingerprint and rebuild the
    /// library for nothing), and removes it when the flag goes off so a tree
    /// reused through `--source-dir` stops outlining.
    #[test]
    fn outline_common_writes_and_removes_its_rustflags_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".cargo/config.toml");

        set_outline_common(dir.path(), true).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        // Scoped to Android targets: `[build]` would hand the backend flag to the
        // host crates of the dependency tree as well, which is not what it is for.
        assert!(text.contains("target_os"));
        assert!(!text.contains("[build]"));
        assert!(text.contains("llvm-args=--enable-machine-outliner=always"));

        let mtime = || std::fs::metadata(&config).unwrap().modified().unwrap();
        let before = mtime();
        set_outline_common(dir.path(), true).unwrap();
        assert_eq!(before, mtime(), "an unchanged file must not be rewritten");

        set_outline_common(dir.path(), false).unwrap();
        assert!(!config.exists());
        // Turning it off twice, or on a project that never had it, is not an
        // error -- `--source-dir` reuses trees of both kinds.
        set_outline_common(dir.path(), false).unwrap();
        set_outline_common(dir.path(), false).unwrap();

        // A second file in `.cargo/` must survive the cleanup.
        set_outline_common(dir.path(), true).unwrap();
        std::fs::write(dir.path().join(".cargo/keep"), "x").unwrap();
        set_outline_common(dir.path(), false).unwrap();
        assert!(dir.path().join(".cargo/keep").exists());
    }

    /// A class path is not a legal module name, and two different classes must
    /// not land in the same file.
    #[test]
    fn class_paths_become_distinct_module_names() {
        let mut used = BTreeSet::new();
        assert_eq!(
            module_name("Lcom/example/Foo;", &mut used),
            "com_example_Foo"
        );
        // Two different classes can sanitise to the same name (`/` and `_` both
        // become `_`), so the second one has to be renamed rather than
        // overwrite the first.
        assert_eq!(module_name("La/b;", &mut used), "a_b");
        assert_eq!(module_name("La_b;", &mut used), "a_b_2");
        assert_eq!(module_name("La_b;", &mut used), "a_b_3");
        // A leading digit and a keyword would both be rejected by rustc.
        assert_eq!(module_name("L0x;", &mut used), "_0x");
        assert_eq!(module_name("Lmod;", &mut used), "mod_");
        // Default-package classes are kept separate too.
        assert_eq!(module_name("LFoo;", &mut used), "Foo");
    }

    /// Every compiler-generated class shares the module of the class that
    /// declares it, so a lambda does not get a file whose name is a mangled
    /// class path.
    #[test]
    fn nested_classes_share_their_outer_class_module() {
        assert_eq!(outer_class("Lcom/example/Foo;"), "Lcom/example/Foo;");
        assert_eq!(outer_class("Lcom/example/Foo$1;"), "Lcom/example/Foo;");
        assert_eq!(outer_class("Lcom/example/Foo$bar$2;"), "Lcom/example/Foo;");
        assert_eq!(
            outer_class("Lcom/example/Foo$$ExternalSyntheticLambda0;"),
            "Lcom/example/Foo;"
        );
        assert_eq!(outer_class("La/b/c$d$e;"), "La/b/c;");
        // The folded path has to equal the outer class's own path exactly, or a
        // nested class lands in a second module next to its outer class.
        for nested in ["LFoo$1;", "LFoo$bar$2;", "LFoo$$Lambda$0;"] {
            assert_eq!(outer_class(nested), outer_class("LFoo;"));
        }
        // A `$` starting the class name is not nesting: this is a top-level
        // class called `$Foo` in the default package.
        assert_eq!(outer_class("L$Foo;"), "L$Foo;");
        assert_eq!(outer_class("L$a/b;"), "L$a/b;");
    }

    /// Folding nested classes must not change the code that is generated, only
    /// which file it lands in.
    #[test]
    fn nested_classes_are_grouped_with_their_outer_class() {
        let triples = [
            ("Lcom/x/Foo;", "a", "()V"),
            ("Lcom/x/Foo$1;", "b", "()V"),
            ("Lcom/x/Foo$when$0;", "c", "()V"),
            ("Lcom/x/Bar;", "d", "()V"),
        ];
        let mut methods = BTreeMap::new();
        for (class, name, desc) in triples {
            let triple = (class.to_string(), name.to_string(), desc.to_string());
            methods.insert(
                triple.clone(),
                GeneratedMethod {
                    triple,
                    jni_name: format!("Java_{name}"),
                    prototype: desc.to_string(),
                    source: format!("fn {name}() {{}}\n"),
                },
            );
        }
        let dir = std::env::temp_dir().join(format!("dex2rs-group-{}", std::process::id()));
        write_methods(&dir, &methods).unwrap();

        let root = std::fs::read_to_string(dir.join("src/methods.rs")).unwrap();
        assert_eq!(root.matches("pub mod ").count(), 2, "{root}");
        assert!(root.contains("pub mod com_x_Foo;"), "{root}");
        assert!(root.contains("pub mod com_x_Bar;"), "{root}");

        let foo = std::fs::read_to_string(dir.join("src/methods/com_x_Foo.rs")).unwrap();
        for name in ["fn a()", "fn b()", "fn c()"] {
            assert!(foo.contains(name), "{name} missing from {foo}");
        }
        // The outer class's own method opens the file, then its nested classes.
        let order: Vec<usize> = ["fn a()", "fn b()", "fn c()"]
            .iter()
            .map(|n| foo.find(n).unwrap())
            .collect();
        assert!(order[0] < order[1] && order[1] < order[2], "{order:?}");
        assert!(!foo.contains("fn d()"), "Bar leaked into Foo: {foo}");

        // Regenerating after a filter change drops the modules that are gone.
        let mut fewer = methods.clone();
        fewer.retain(|triple, _| triple.0 == "Lcom/x/Bar;");
        write_methods(&dir, &fewer).unwrap();
        assert!(!dir.join("src/methods/com_x_Foo.rs").exists());
        assert!(dir.join("src/methods/com_x_Bar.rs").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn byte_strings_are_escaped() {
        assert_eq!(escape(b"add"), "add");
        assert_eq!(escape(b"<init>"), "<init>");
        assert_eq!(escape(b"a\\b"), "a\\\\b");
        assert_eq!(escape(b"a\"b"), "a\\\"b");
        assert_eq!(escape(&[0x01, 0xff]), "\\x01\\xff");
    }

    #[test]
    fn trace_feature_is_toggled_in_the_manifest() {
        let dir = std::env::temp_dir().join(format!("dex2rs-scaffold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"stub\"\n\n[dependencies]\ndex2rs-runtime = { path = \"runtime\", default-features = false }\n",
        )
        .unwrap();
        set_trace_feature(&dir, true).unwrap();
        let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(text.contains("features = [\"trace\"]"), "{text}");
        set_trace_feature(&dir, false).unwrap();
        let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(!text.contains("features = [\"trace\"]"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_embedded_project_template_is_complete() {
        let dir = std::env::temp_dir().join(format!("dex2rs-embedded-{}", std::process::id()));
        create_project(&dir, "mylib").unwrap();
        for file in [
            "Cargo.toml",
            "src/lib.rs",
            "src/methods.rs",
            "src/dynamic_register.rs",
            "runtime/Cargo.toml",
            "runtime/src/lib.rs",
            "runtime/src/resolve.rs",
        ] {
            assert!(dir.join(file).exists(), "{file} was not written");
        }
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"mylib\""), "{manifest}");
        // The template's own placeholder must not survive the rewrite.
        assert!(!manifest.contains("name = \"stub\""), "{manifest}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
