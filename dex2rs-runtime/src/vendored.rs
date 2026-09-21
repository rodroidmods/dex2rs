//! The crate's own sources, for a driver that vendors the runtime into a project.
//!
//! `dex2rs` writes these files out as a generated project's `runtime/` directory —
//! the original shipped `jni/nc/` inside the project folder — and after
//! `cargo install` there is no checkout to copy them from. Keeping them here means
//! the copy in a project is byte for byte the runtime this crate was published as,
//! instead of whatever a directory lookup happened to find.
//!
//! Behind the `vendor-table` feature, which only `dex2rs` turns on: this module's
//! own file is not part of [`SOURCES`], so the vendored copy does not carry a
//! table of the sources inside the sources.

/// Every source file of this crate, as `(path, contents)`.
///
/// `src/vendored.rs` is deliberately absent: it is what writes the others out, and
/// a vendored project neither has it on disk nor can have it enabled (the
/// standalone manifest declares the feature but never turns it on).
pub const SOURCES: &[(&str, &str)] = &[
    ("src/invoke.rs", include_str!("invoke.rs")),
    ("src/jni.rs", include_str!("jni.rs")),
    ("src/lib.rs", include_str!("lib.rs")),
    ("src/ops.rs", include_str!("ops.rs")),
    ("src/resolve.rs", include_str!("resolve.rs")),
    ("src/watchdog.rs", include_str!("watchdog.rs")),
];

/// The manifest a generated project uses for its vendored `runtime/` directory.
///
/// Deliberately *not* this crate's own manifest: a generated project is a
/// workspace of its own, so it needs the standalone copy — no `workspace = true`
/// inheritance, no `[package.metadata.docs.rs]`, no examples, `publish = false`.
pub const STANDALONE_MANIFEST: &str = include_str!("../standalone-manifest.toml");
