//! Write `target/fixture/fixture.apk` (and its `filter.txt`) for manual runs:
//!
//! ```text
//! cargo run -p dex2rs --example make_fixture_apk
//! cargo run -p dex2rs -- -i target/fixture/fixture.apk \
//!     -f target/fixture/filter.txt -e target/fixture/project -o target/fixture/out.apk
//! ```
//!
//! The APK is assembled from the compiler's smali fixture plus a binary
//! `AndroidManifest.xml`, so it exercises the whole pipeline without needing a
//! real app.

use std::path::{Path, PathBuf};

use smali::android::binary_xml::AndroidManifest;
use smali::android::zip::ApkFile;
use smali::dex::DexFile;
use smali::types::SmaliClass;

const HELLO: &str = include_str!("../../dex2rs-core/tests/fixtures/Hello.smali");

const APP: &str = r#"
.class public Lcom/example/App;
.super Landroid/app/Application;

.method static final constructor <clinit>()V
    .locals 0

    return-void
.end method
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/fixture"));
    std::fs::create_dir_all(&dir)?;
    let app_class = std::env::args().nth(2);
    let apk = build(&dir, app_class.as_deref())?;
    std::fs::write(
        dir.join("filter.txt"),
        "# compile everything in the fixture\n.*\n",
    )?;
    println!("wrote {}", apk.display());
    Ok(())
}

fn build(dir: &Path, app_class: Option<&str>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut classes = vec![SmaliClass::from_smali(HELLO)?];
    if app_class.is_some() {
        classes.push(SmaliClass::from_smali(APP)?);
    }
    let dex = DexFile::from_smali(&classes)?.to_bytes().to_vec();

    let application = match app_class {
        Some(name) => format!("<application android:name=\"{name}\" />"),
        None => "<application />".to_string(),
    };
    let xml = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.example.fixture">
    <uses-sdk android:minSdkVersion="24" />
    {application}
</manifest>
"#
    );
    let mut manifest = AndroidManifest::from_string(&xml)?;
    // Tell the encoder about the framework attributes used above so their
    // resource ids survive into the binary XML (Android's package parser
    // matches attributes by id, not by name).
    manifest.register_framework_attribute("name", 0x0101_0003); // android.R.attr.name
    manifest.register_framework_attribute("minSdkVersion", 0x0101_020c); // .minSdkVersion

    let mut apk = ApkFile::new();
    apk.replace_entry("classes.dex", dex)?;
    apk.replace_entry_with("AndroidManifest.xml", manifest.to_apk_entry(None)?)?;
    let path = dir.join("fixture.apk");
    apk.write_to_file(&path)?;
    Ok(path)
}
