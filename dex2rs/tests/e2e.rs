//! End-to-end tests: a real APK goes in, a patched APK comes out.
//!
//! The fixture APK is assembled with the `smali` crate itself (dex from the
//! shared smali fixture + a binary `AndroidManifest.xml`), so the whole
//! pipeline — dex parsing, filtering, compiling, dex patching, manifest
//! rewriting and repacking — runs without needing an external APK or the NDK.

use std::path::{Path, PathBuf};

use smali::android::binary_xml::AndroidManifest;
use smali::android::zip::ApkFile;
use smali::dex::DexFile;
use smali::types::{Modifier, SmaliClass};

const HELLO: &str = include_str!("../../dex2rs-core/tests/fixtures/Hello.smali");

/// An app class with an existing (empty) `<clinit>`, to exercise the
/// "application class found in the manifest" path.
const APP: &str = r#"
.class public Lcom/example/App;
.super Landroid/app/Application;

.method static final constructor <clinit>()V
    .locals 0

    return-void
.end method
"#;

/// Kotlin's shape, which is why this matters more than it looks: an interface
/// with a method that has a default argument. The `$default` bridge is declared
/// by the interface *and has bytecode*, so the converter's own "no bytecode"
/// check does not see it.
const SIGNER: &str = r#"
.class public interface abstract Lcom/example/Signer;
.super Ljava/lang/Object;

# The declaration: abstract, bodyless.
.method public abstract sign()V
.end method

# The bridge a default argument generates, with a body of its own.
.method public static sign$default()V
    .locals 0

    return-void
.end method
"#;

/// A class declaring the same method, which must still be converted -- the rule
/// is about interfaces, not about method names.
const SIGNER_IMPL: &str = r#"
.class public Lcom/example/SignerImpl;
.super Ljava/lang/Object;

.method public sign()V
    .locals 0

    return-void
.end method
"#;

fn manifest_xml(app_class: Option<&str>) -> String {
    let application = match app_class {
        Some(name) => format!("<application android:name=\"{name}\" />"),
        None => "<application />".to_string(),
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.example.fixture">
    <uses-sdk android:minSdkVersion="24" />
    {application}
</manifest>
"#
    )
}

/// Assemble `<dir>/fixture.apk` with the fixture dex and manifest.
fn build_apk(dir: &Path, app_class: Option<&str>) -> PathBuf {
    build_apk_with(dir, app_class, &[])
}

/// `build_apk`, plus extra smali class sources appended to the dex.
fn build_apk_with(dir: &Path, app_class: Option<&str>, extra: &[&str]) -> PathBuf {
    let mut classes = vec![SmaliClass::from_smali(HELLO).expect("fixture parses")];
    if app_class.is_some() {
        classes.push(SmaliClass::from_smali(APP).expect("app class parses"));
    }
    for source in extra {
        classes.push(SmaliClass::from_smali(source).expect("extra class parses"));
    }
    let dex = DexFile::from_smali(&classes)
        .expect("assemble")
        .to_bytes()
        .to_vec();

    let mut manifest = AndroidManifest::from_string(&manifest_xml(app_class)).expect("manifest");
    // A generated manifest starts with no resource ids; tell the encoder about
    // `android:name` so `<application>` keeps a usable app class.
    // Tell the encoder about the framework attributes used above so their
    // resource ids survive into the binary XML (Android's package parser
    // matches attributes by id, not by name).
    manifest.register_framework_attribute("name", 0x0101_0003); // android.R.attr.name
    manifest.register_framework_attribute("minSdkVersion", 0x0101_020c); // .minSdkVersion

    let mut apk = ApkFile::new();
    apk.replace_entry("classes.dex", dex).expect("dex entry");
    apk.replace_entry_with(
        "AndroidManifest.xml",
        manifest.to_apk_entry(None).expect("encoding manifest"),
    )
    .expect("manifest entry");

    let path = dir.join("fixture.apk");
    apk.write_to_file(&path).expect("writing fixture apk");
    path
}

fn filter_file(dir: &Path) -> PathBuf {
    let path = dir.join("filter.txt");
    std::fs::write(&path, "# compile everything in the fixture\n.*\n").unwrap();
    path
}

/// Read the converted APK back and return `(classes.dex smali, manifest)`.
fn read_output(path: &Path) -> (Vec<SmaliClass>, AndroidManifest) {
    let apk = ApkFile::from_file(path).expect("output apk opens");
    let dex = apk.entry("classes.dex").expect("classes.dex").data.clone();
    let classes = DexFile::from_bytes(&dex)
        .expect("dex parses")
        .to_smali()
        .expect("dex disassembles");
    let manifest =
        AndroidManifest::from_apk_entry(apk.entry("AndroidManifest.xml").expect("manifest"))
            .expect("manifest decodes");
    (classes, manifest)
}

fn method_is_native<'a>(classes: &'a [SmaliClass], name: &str) -> &'a smali::types::SmaliMethod {
    classes
        .iter()
        .flat_map(|class| class.methods.iter())
        .find(|method| method.name == name)
        .unwrap_or_else(|| panic!("converted dex has a method named {name}"))
}

#[test]
fn converts_a_dex_into_native_methods_and_a_loader() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), None);
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
    ])
    .expect("conversion succeeds");

    assert!(out.exists(), "the output apk exists");
    let (classes, manifest) = read_output(&out);

    // Compiled methods become `native` with no body.
    for name in ["add", "max", "sum", "divZero", "makeArray"] {
        let method = method_is_native(&classes, name);
        assert!(
            method.modifiers.contains(&Modifier::Native),
            "{name} is native: {:?}",
            method.modifiers
        );
        assert!(method.ops.is_empty(), "{name} has no body");
    }
    // `<clinit>`/`<init>` are skipped by the filter, so they keep their bodies.
    let clinit = method_is_native(&classes, "<clinit>");
    assert!(!clinit.modifiers.contains(&Modifier::Native));

    // No application class in the manifest: the loader class is injected and
    // becomes the application class.
    let loader_class = classes
        .iter()
        .find(|class| class.name.as_java_type() == "amimo.dcc.DccApplication")
        .expect("loader class was added");
    let clinit = loader_class
        .methods
        .iter()
        .find(|method| method.name == "<clinit>")
        .expect("loader has a <clinit>");
    let loads_library = clinit.ops.iter().any(|op| {
        matches!(
            op,
            smali::types::SmaliOp::Op(smali::smali_ops::DexOp::ConstString { value, .. })
                if value.contains("stub")
        )
    });
    assert!(loads_library, "loader loads libstub: {clinit:?}");
    assert_eq!(
        manifest
            .application()
            .and_then(|app| app.attribute_value("android:name"))
            .and_then(|value| value.as_str()),
        Some("amimo.dcc.DccApplication")
    );
}

#[test]
fn reuses_the_application_class_clinit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), Some("com.example.App"));
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
        "-l".into(),
        "mylib".into(),
    ])
    .expect("conversion succeeds");

    let (classes, manifest) = read_output(&out);

    // The app class kept its `<clinit>`, which now loads the library.
    let app_class = classes
        .iter()
        .find(|class| class.name.as_java_type() == "com.example.App")
        .expect("application class still present");
    assert_eq!(
        app_class
            .methods
            .iter()
            .filter(|method| method.name == "<clinit>")
            .count(),
        1,
        "still exactly one <clinit>"
    );
    let clinit = app_class
        .methods
        .iter()
        .find(|method| method.name == "<clinit>")
        .unwrap();
    let loads_library = clinit.ops.iter().any(|op| {
        matches!(
            op,
            smali::types::SmaliOp::Op(smali::smali_ops::DexOp::ConstString { value, .. })
                if value.contains("mylib")
        )
    });
    assert!(loads_library, "app class loads libmylib: {clinit:?}");
    // No loader class was added.
    assert!(
        classes
            .iter()
            .all(|class| class.name.as_java_type() != "amimo.dcc.DccApplication")
    );
    // The manifest still points at the real application class.
    assert_eq!(
        manifest
            .application()
            .and_then(|app| app.attribute_value("android:name"))
            .and_then(|value| value.as_str()),
        Some("com.example.App")
    );
}

#[test]
fn keeps_unselected_methods_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), None);
    let filter = tmp.path().join("filter.txt");
    // Only `add` is compiled: a full-name rule is the class descriptor, the
    // method name and the prototype concatenated (dcc.py's `"".join(triple)`).
    std::fs::write(&filter, "=Lcom/example/Hello;add(II)I\n").unwrap();
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
    ])
    .expect("conversion succeeds");

    let (classes, _) = read_output(&out);
    assert!(
        method_is_native(&classes, "add")
            .modifiers
            .contains(&Modifier::Native)
    );
    assert!(
        !method_is_native(&classes, "max")
            .modifiers
            .contains(&Modifier::Native)
    );
    assert!(!method_is_native(&classes, "max").ops.is_empty());
}

/// `--force-custom-loader` with an application class present: the loader class
/// is added *and* the application class instantiates it from `<clinit>`.
#[test]
fn force_custom_loader_instantiates_the_loader() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), Some("com.example.App"));
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
        "-r".into(),
        "-c".into(),
        "my.loader.App".into(),
    ])
    .expect("conversion succeeds");

    let (classes, manifest) = read_output(&out);
    let app_class = classes
        .iter()
        .find(|class| class.name.as_java_type() == "com.example.App")
        .expect("application class still present");
    let clinit = app_class
        .methods
        .iter()
        .find(|method| method.name == "<clinit>")
        .expect("<clinit> exists");
    let news_the_loader = clinit.ops.iter().any(|op| {
        matches!(
            op,
            smali::types::SmaliOp::Op(smali::smali_ops::DexOp::NewInstance { .. })
        )
    });
    assert!(
        news_the_loader,
        "the app class instantiates the loader: {clinit:?}"
    );
    assert!(
        classes
            .iter()
            .any(|class| class.name.as_java_type() == "my.loader.App"),
        "the requested loader class was added"
    );
    // The manifest still points at the real application class.
    assert_eq!(
        manifest
            .application()
            .and_then(|app| app.attribute_value("android:name"))
            .and_then(|value| value.as_str()),
        Some("com.example.App")
    );
}

/// The full pipeline: a real NDK build, packaging and signing (needs the NDK,
/// `cargo-ndk`, `java` and the repository `dcc.cfg`). Run with
/// `cargo test -p dex2rs --test e2e -- --ignored --nocapture`.
#[test]
#[ignore]
fn builds_a_shared_library_and_signs_with_the_repository_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), None);
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");
    // The repository config points `keystore_path` at the working-directory
    // default, which is not shipped, so this also exercises the resolution order:
    // nothing on disk, then the keystore embedded in the binary.
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dcc.cfg");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--config".into(),
        config.display().to_string(),
        "--abis".into(),
        "arm64-v8a".into(),
        "--api".into(),
        "24".into(),
        "-l".into(),
        "stub".into(),
    ])
    .expect("conversion succeeds");

    let apk = ApkFile::from_file(&out).expect("output apk opens");
    let so = apk
        .entry("lib/arm64-v8a/libstub.so")
        .expect("compiled library is packaged");
    assert!(so.data.starts_with(b"\x7fELF"), "packaged file is an ELF");

    // A v2/v3 signature lives in the APK Signing Block, not in META-INF.
    let bytes = std::fs::read(&out).expect("output apk bytes");
    assert!(
        bytes.windows(16).any(|w| w == b"APK Sig Block 42"),
        "the output apk carries an APK Signing Block"
    );
}

/// A method whose JNI long name exceeds 220 characters must still be
/// converted.
///
/// dcc.py named every generated `.cpp` after the JNI symbol, which is capped by
/// the filesystem at 255 bytes, so it silently dropped each method whose
/// mangled name passed 220 characters. The methods that reach that length are
/// ordinary Kotlin ones -- `$default` bridges with wide signatures, big
/// lambdas, `access$...` accessors -- and the skip was only logged at debug
/// level, so a normal run never showed it: those methods kept their bytecode
/// and the conversion looked complete. The port writes all methods into one
/// `methods.rs`, so nothing needs a length cap; this pins that.
#[test]
fn converts_methods_with_jni_names_longer_than_220_chars() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // `Java_com_example_LongName_m__` plus 40 x `Ljava_lang_String_2` is well
    // past 220 characters, and is exactly the shape a `$default` bridge takes.
    let params = "Ljava/lang/String;".repeat(40);
    let long_class = format!(
        ".class public Lcom/example/LongName;\n\
         .super Ljava/lang/Object;\n\n\
         .method public static m({params})V\n\
         .registers 41\n\n\
         return-void\n\
         .end method\n"
    );
    let jni_name =
        dex2rs_core::util::jni_long_name("Lcom/example/LongName;", "m", &format!("({params})V"));
    assert!(jni_name.len() > 220, "fixture name is {}", jni_name.len());

    let apk_path = build_apk_with(tmp.path(), None, &[long_class.as_str()]);
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
    ])
    .expect("conversion succeeds");

    let (classes, _) = read_output(&out);
    let method = method_is_native(&classes, "m");
    assert!(
        method.modifiers.contains(&Modifier::Native),
        "the long-named method was converted, not dropped: {:?}",
        method.modifiers
    );
    assert!(method.ops.is_empty(), "the long-named method has no body");
}

/// A standalone dex in, a zip (dex + lib/) out.
#[test]
fn converts_a_standalone_dex() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk(tmp.path(), None);
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.zip");

    // Take the dex back out of the fixture apk and convert it on its own.
    let fixture = ApkFile::from_file(&apk_path).expect("fixture opens");
    let dex_path = tmp.path().join("input.dex");
    std::fs::write(
        &dex_path,
        &fixture.entry("classes.dex").expect("dex entry").data,
    )
    .expect("writing the dex");

    dex2rs::run_from(&[
        "-i".into(),
        dex_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
    ])
    .expect("conversion succeeds");

    // dcc.py stores the dex under its own file name and adds `lib/<abi>/`.
    let zip = ApkFile::from_file(&out).expect("output zip opens");
    assert!(zip.entry("input.dex").is_some(), "the dex keeps its name");
    let classes = DexFile::from_bytes(&zip.entry("input.dex").unwrap().data)
        .expect("dex parses")
        .to_smali()
        .expect("dex disassembles");
    assert!(
        classes
            .iter()
            .flat_map(|class| class.methods.iter())
            .find(|method| method.name == "add")
            .expect("add is present")
            .modifiers
            .contains(&Modifier::Native)
    );
    assert!(zip.entry("classes.dex").is_none());
}

/// A method an interface declares must stay abstract or keep its body: marking
/// it `native` makes the verifier reject the *whole* dex file, so the APK cannot
/// load at all -- it fails as "ClassNotFoundException: <application class>",
/// which points nowhere near the cause.
#[test]
fn an_interfaces_methods_are_never_converted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let apk_path = build_apk_with(tmp.path(), None, &[SIGNER, SIGNER_IMPL]);
    let filter = filter_file(tmp.path());
    let out = tmp.path().join("out.apk");

    dex2rs::run_from(&[
        "-i".into(),
        apk_path.display().to_string(),
        "-o".into(),
        out.display().to_string(),
        "-f".into(),
        filter.display().to_string(),
        "--no-build".into(),
    ])
    .expect("conversion succeeds");

    let (classes, _) = read_output(&out);

    // The rule, stated over the whole dex: this is the property that failed to
    // hold when a converted app could not start.
    for class in classes
        .iter()
        .filter(|class| class.modifiers.contains(&Modifier::Interface))
    {
        for method in &class.methods {
            assert!(
                !method.modifiers.contains(&Modifier::Native),
                "{} is native in interface {}, which no dex can carry",
                method.name,
                class.name.as_java_type()
            );
        }
    }

    // Both shapes named: nothing in the interface changed, body included.
    let signer = classes
        .iter()
        .find(|class| class.name.as_java_type() == "com.example.Signer")
        .expect("the interface survives");
    for name in ["sign", "sign$default"] {
        let method = signer
            .methods
            .iter()
            .find(|method| method.name == name)
            .unwrap_or_else(|| panic!("the interface still declares {name}"));
        assert!(
            !method.modifiers.contains(&Modifier::Native),
            "{name} was left as it was"
        );
    }
    let bridge = signer
        .methods
        .iter()
        .find(|method| method.name == "sign$default")
        .expect("the $default bridge is present");
    assert!(!bridge.ops.is_empty(), "and it kept its body");

    // The class beside it is converted as usual: the rule is about where a
    // method is declared, not about its name.
    let impl_class = classes
        .iter()
        .find(|class| class.name.as_java_type() == "com.example.SignerImpl")
        .expect("the implementation survives");
    let sign = impl_class
        .methods
        .iter()
        .find(|method| method.name == "sign")
        .expect("the implementation declares sign");
    assert!(sign.modifiers.contains(&Modifier::Native));
    assert!(sign.ops.is_empty());
}
