//! `SOURCES` is what a driver writes out as a generated project's `runtime/`
//! directory, and nothing in the type system ties it to the files on disk: a
//! module added here and not listed there produces a project that does not
//! compile — and only after a `cargo install`, where nobody is looking.
//!
//! These tests only run from a checkout, which is exactly where the mistake would
//! be made:
//!
//! ```text
//! cargo test -p dex2rs-runtime --features vendor-table
//! ```

use std::path::Path;

#[test]
fn sources_lists_every_module() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut on_disk = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("reading src/") {
        let path = entry.expect("directory entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // `vendored.rs` holds this table, so it is not one of the files the table
        // lists -- a vendored project has nothing to vendor.
        if name == "vendored.rs" || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        on_disk.push(format!("src/{name}"));
    }
    on_disk.sort();

    // `src/vendored.rs` is the table itself: it is written by this crate, so a
    // vendored project has no use for it and `dex2rs` does not write it out.
    let mut listed: Vec<String> = dex2rs_runtime::vendored::SOURCES
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();
    listed.sort();

    assert_eq!(
        on_disk, listed,
        "SOURCES is out of sync with src/ — a generated project would be missing a module"
    );
}

#[test]
fn every_listed_source_is_a_real_module() {
    for (path, contents) in dex2rs_runtime::vendored::SOURCES {
        assert!(path.starts_with("src/"), "{path} is not under src/");
        assert!(path.ends_with(".rs"), "{path} is not a Rust file");
        assert!(
            contents.contains("//!") || contents.contains("///") || contents.contains("pub "),
            "{path} does not look like the module it claims to be"
        );
    }
}

#[test]
fn the_standalone_manifest_is_self_contained() {
    let manifest = dex2rs_runtime::vendored::STANDALONE_MANIFEST;
    assert!(manifest.contains("name = \"dex2rs-runtime\""), "{manifest}");
    // A generated project is its own workspace: anything inherited from *this*
    // workspace would not resolve there.
    assert!(
        !manifest.contains(".workspace = true"),
        "the vendored manifest inherits from the workspace it was copied out of"
    );
    // No examples are vendored, so a `[[example]]` here would break `cargo build`
    // in the generated project.
    assert!(!manifest.contains("[[example]]"), "{manifest}");
}
