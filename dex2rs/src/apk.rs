//! APK / DEX input handling: read, patch, inject the loader, write back out.
//!
//! The Python tool drove `APKEditor.jar` (baksmali/smali/APKEditor) for all of
//! this. The `smali` crate replaces all three: `DexFile::from_bytes` +
//! `to_smali()` disassemble, editing happens on typed `SmaliClass`es, and
//! `DexFile::from_smali` reassembles. `ApkFile` reads and writes the archive and
//! decodes/encodes the binary `AndroidManifest.xml`.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use smali::android::binary_xml::{AndroidManifest, ManifestAttribute, ManifestValue};
use smali::android::zip::{ApkCompression, ApkFile, is_top_level_dex_name};
use smali::dex::DexFile;
use smali::types::{Modifier, SmaliClass};

use dex2rs_core::dexedit;

use crate::filter::MethodTriple;

/// The Android namespace URI, needed when adding a new `android:*` attribute.
const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
/// `android.R.attr.name`
const ATTR_ANDROID_NAME: u32 = 0x0101_0003;
/// `android.R.attr.extractNativeLibs`
const ATTR_ANDROID_EXTRACT_NATIVE_LIBS: u32 = 0x0101_04ea;

/// One dex with its entry name.
pub struct DexUnit {
    /// `classes.dex`, `classes2.dex`, ... inside the APK, or the file name of a
    /// standalone dex.
    pub name: String,
    pub classes: Vec<SmaliClass>,
    /// DEX version the input declared (35, 37, 38, 39…). The assembler always
    /// emits 039; this is what the output header is rewritten back to, because
    /// ART rejects a version newer than the one the device supports.
    pub version: u32,
}

/// A loaded input file.
pub enum Input {
    Apk {
        apk: ApkFile,
        dexes: Vec<DexUnit>,
        manifest: AndroidManifest,
        /// Original bytes of `AndroidManifest.xml`, used as the encoding template.
        manifest_entry: smali::android::zip::ApkEntry,
        min_sdk: u32,
        app_class: Option<String>,
    },
    Dex {
        /// Name to store the rebuilt dex under inside the output zip.
        file_name: String,
        dexes: Vec<DexUnit>,
        min_sdk: u32,
    },
}

impl Input {
    pub fn is_apk(&self) -> bool {
        matches!(self, Input::Apk { .. })
    }

    pub fn min_sdk(&self) -> u32 {
        match self {
            Input::Apk { min_sdk, .. } | Input::Dex { min_sdk, .. } => *min_sdk,
        }
    }

    pub fn dexes(&self) -> &[DexUnit] {
        match self {
            Input::Apk { dexes, .. } | Input::Dex { dexes, .. } => dexes,
        }
    }

    pub fn dexes_mut(&mut self) -> &mut Vec<DexUnit> {
        match self {
            Input::Apk { dexes, .. } | Input::Dex { dexes, .. } => dexes,
        }
    }
}

/// Detect and load `path` as an APK or a standalone dex.
pub fn load(path: &Path) -> Result<Input> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.starts_with(b"dex\n") {
        return load_dex(path, bytes);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return load_apk(path);
    }
    bail!(
        "{} is not supported: expected an '.apk' (zip) or a '.dex' file",
        path.display()
    )
}

fn load_dex(path: &Path, bytes: Vec<u8>) -> Result<Input> {
    let version = dex_version(&bytes).unwrap_or(35);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "classes.dex".to_string());
    let classes = DexFile::from_bytes(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?
        .to_smali()
        .context("disassembling dex")?;
    Ok(Input::Dex {
        file_name,
        dexes: vec![DexUnit {
            name: "classes.dex".to_string(),
            classes,
            version,
        }],
        min_sdk: min_sdk_from_dex_version(version),
    })
}

fn load_apk(path: &Path) -> Result<Input> {
    let apk = ApkFile::from_file(path).with_context(|| format!("opening {}", path.display()))?;
    let manifest_entry = apk
        .entry("AndroidManifest.xml")
        .cloned()
        .context("APK has no AndroidManifest.xml")?;
    let manifest =
        AndroidManifest::from_apk_entry(&manifest_entry).context("decoding AndroidManifest.xml")?;
    let min_sdk = manifest_min_sdk(&manifest).unwrap_or(21);
    let app_class = manifest_application_name(&manifest);

    let mut dexes = Vec::new();
    for name in apk.entry_names() {
        if !is_top_level_dex_name(name) {
            continue;
        }
        let data = &apk.entry(name).expect("entry just listed").data;
        let version = dex_version(data).unwrap_or(39);
        let classes = DexFile::from_bytes(data)
            .with_context(|| format!("parsing {name}"))?
            .to_smali()
            .with_context(|| format!("disassembling {name}"))?;
        dexes.push(DexUnit {
            name: name.to_string(),
            classes,
            version,
        });
    }
    if dexes.is_empty() {
        bail!("{} is not supported: no classes.dex found", path.display());
    }

    Ok(Input::Apk {
        apk,
        dexes,
        manifest,
        manifest_entry,
        min_sdk,
        app_class,
    })
}

/// `dex\n035\0` -> 35.
fn dex_version(bytes: &[u8]) -> Option<u32> {
    let digits = bytes.get(4..7)?;
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// dcc.py's `get_api_from_dex` / `get_min_sdk_from_dex` mapping.
pub fn min_sdk_from_dex_version(version: u32) -> u32 {
    match version {
        35 => 21,
        37 => 24,
        38 | 39 | 40 => 26,
        _ => 28,
    }
}

/// `minSdkVersion` of `<uses-sdk>`, whether stored as an integer or a reference.
pub fn manifest_min_sdk(manifest: &AndroidManifest) -> Option<u32> {
    let uses_sdk = manifest.root().find_child("uses-sdk")?;
    let value = uses_sdk.attribute_value("android:minSdkVersion")?;
    match value {
        ManifestValue::Integer(v) => u32::try_from(*v).ok(),
        ManifestValue::Reference(id) => Some(*id),
        ManifestValue::Hex(v) => Some(*v),
        _ => value.as_str()?.parse().ok(),
    }
}

/// `android:name` of `<application>`, i.e. the app class (Python's
/// `APKEditor.get_info -app-class`).
pub fn manifest_application_name(manifest: &AndroidManifest) -> Option<String> {
    let app = manifest.application()?;
    app.attribute_value("android:name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Rewrite every compiled method as `native` with no body, keeping its
/// annotations (minus the Dex2C marker) — the typed equivalent of dcc.py's
/// `native_class_methods`.
///
/// When `strip_debug` is set, every dex also loses its debug tables. The smali
/// assembler rebuilds a dense line table for each method that has one, and on a
/// real app that is several megabytes of `debug_info_item` that the input never
/// carried in that form; nothing but stack traces depend on it.
pub fn patch_compiled_methods(
    dexes: &mut [DexUnit],
    compiled: &BTreeSet<MethodTriple>,
    strip_debug: bool,
) {
    // Methods an interface declares that reached us anyway. The filter refuses
    // them, so this should stay at zero; it is counted rather than trusted,
    // because the failure it guards against is invisible until the APK is
    // installed and then looks like the converter broke the whole app.
    let mut refused_interface = 0usize;
    for unit in dexes.iter_mut() {
        for class in &mut unit.classes {
            // DEX requires everything an interface declares to be abstract or
            // to carry a body; `native` is neither, and the verifier rejects
            // the entire dex file over it -- every class in the APK stops
            // loading, starting with the application class.
            let in_interface = class.modifiers.contains(&Modifier::Interface);
            let cls = class.name.as_jni_type();
            for method in &mut class.methods {
                let proto = method.signature.to_jni();
                let triple = (cls.clone(), method.name.clone(), proto);
                if !compiled.contains(&triple) {
                    continue;
                }
                if in_interface {
                    refused_interface += 1;
                    continue;
                }
                if !method.modifiers.contains(&Modifier::Native) {
                    // Inserted before the name, like the Python tool's textual
                    // `"native " + name` replacement.
                    method.modifiers.push(Modifier::Native);
                }
                method.ops.clear();
                method.registers = None;
                method.locals = 0;
                method.params.clear();
                method
                    .annotations
                    .retain(|a| !a.annotation_type.to_jni().ends_with("Dex2C;"));
            }
        }
        if strip_debug {
            dexedit::strip_debug_info(&mut unit.classes);
        }
    }
    if refused_interface > 0 {
        eprintln!(
            " Left {} method(s) an interface declares alone: DEX cannot carry those as native",
            refused_interface
        );
    }
    // The check the refusal is for. It has to hold for the dex as written, not
    // just for what this function was handed: one native method in one
    // interface makes the whole file uninstallable, and the failure surfaces as
    // `ClassNotFoundException` for the application class.
    let broken = native_interface_methods(dexes);
    if broken > 0 {
        eprintln!(
            " WARNING: {broken} method(s) an interface declares are native; this dex will not verify"
        );
    }
}

/// How many methods an interface declares are marked `native` in these dexes.
///
/// Should always be zero. DEX requires an interface's methods to be abstract or
/// to carry a body, and a file that breaks that rule fails verification as a
/// whole, taking every class in the APK with it.
fn native_interface_methods(dexes: &[DexUnit]) -> usize {
    dexes
        .iter()
        .flat_map(|unit| unit.classes.iter())
        .filter(|class| class.modifiers.contains(&Modifier::Interface))
        .flat_map(|class| class.methods.iter())
        .filter(|method| method.modifiers.contains(&Modifier::Native))
        .count()
}

/// The archive writer's own "keep this entry uncompressed" rule (`classify_entry`
/// in the `smali` crate): only those entries get alignment padding, so an entry
/// carrying an inherited `Stored` hint that falls outside this set must be
/// deflated instead.
fn writer_stores_uncompressed(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "resources.arsc" {
        return true;
    }
    lower.ends_with(".arsc")
        || lower.ends_with(".dex")
        || lower.ends_with(".so")
        || matches!(
            lower.rsplit('.').next(),
            Some(ext)
                if matches!(
                    ext,
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "avif" | "mp3" | "ogg"
                        | "wav" | "aac" | "flac" | "m4a" | "mp4" | "webm" | "mkv" | "ico"
                )
        )
}

/// The compression AGP gives an entry, for the names where it matters.
///
/// `classes*.dex` is deflated (the `smali` writer would store every `*.dex`,
/// which cost 2 MiB on the reference app) and `resources.arsc` is stored,
/// because the resource table is mapped straight out of the APK. Everything
/// else returns `None` and keeps the input's choice.
///
/// Native libraries are the one case that depends on a switch. Stored (the
/// default) is what `extractNativeLibs="false"` requires: the loader maps the
/// `.so` out of the APK, which needs it uncompressed and page aligned, and it
/// costs the full size of the converted code in the download -- 7.7 MiB on the
/// reference app, more than half the APK. Deflated trades that for extraction at
/// install time: the installer unpacks the `.so` into `/data/app/.../lib/<abi>/`
/// and it is loaded from there, which is what a conventionally packaged app
/// does and what `--compress-libs` selects. The two settings are not
/// independent, so `set_extract_native_libs` has to agree with this.
fn agp_compression(name: &str, compress_libs: bool) -> Option<ApkCompression> {
    let lower = name.to_ascii_lowercase();
    if is_top_level_dex_name(name) {
        return Some(ApkCompression::Deflated);
    }
    if lower.starts_with("lib/") && lower.ends_with(".so") {
        return Some(if compress_libs {
            ApkCompression::Deflated
        } else {
            ApkCompression::Stored
        });
    }
    if lower == "resources.arsc" {
        return Some(ApkCompression::Stored);
    }
    None
}

/// Repack-time compression/alignment fix, applied after every entry has been
/// replaced (a fresh `ApkEntry` carries no compression hint, so this has to run
/// last).
///
/// Two things are corrected:
///
/// * the entries of [`agp_compression`] get the compression the platform
///   expects, whatever the input or the freshly built bytes arrived with;
/// * an entry that would be *stored* by the archive writer's name rule but is
///   **not** classified as storable by it loses the `Stored` flag. The writer
///   only pads an entry it classified itself (4 bytes generally, 16 KiB for
///   `lib/**/*.so`), so an inherited `Stored` hint on anything else lands
///   unaligned: for this app that was `assets/dexopt/baseline.prof`, and
///   `zipalign -c` refused the result.
fn normalize_compression(apk: &mut ApkFile, compress_libs: bool) -> Result<()> {
    let names: Vec<String> = apk.entry_names().map(str::to_string).collect();
    for name in names {
        let Some(mut entry) = apk.entry(&name).cloned() else {
            continue;
        };
        let wanted = match agp_compression(&name, compress_libs) {
            Some(preferred) => preferred,
            None => {
                let stored = match entry.compression {
                    Some(preferred) => preferred == ApkCompression::Stored,
                    // No hint: the writer decides from the name.
                    None => writer_stores_uncompressed(&name),
                };
                if !stored || writer_stores_uncompressed(&name) {
                    continue;
                }
                ApkCompression::Deflated
            }
        };
        if entry.compression == Some(wanted) {
            continue;
        }
        entry.compression = Some(wanted);
        apk.replace_entry_with(&name, entry)
            .with_context(|| format!("normalising the compression of {name}"))?;
    }
    Ok(())
}

/// Serialise every dex back to bytes and store the result in the APK.
///
/// The header is rewritten back to the input's DEX version: `from_smali` always
/// emits `039`, which a device below Android 9 / API 28 refuses to load.
fn rebuild_dex_bytes(dexes: &[DexUnit]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::with_capacity(dexes.len());
    for unit in dexes {
        let dex = DexFile::from_smali(&unit.classes)
            .with_context(|| format!("assembling {}", unit.name))?;
        let mut bytes = dex.to_bytes().to_vec();
        if dexedit::set_dex_version(&mut bytes, unit.version)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("restoring the dex version of {}", unit.name))?
        {
            eprintln!(" Restored {} to DEX version {:03}", unit.name, unit.version);
        }
        out.push((unit.name.clone(), bytes));
    }
    Ok(out)
}

/// The loader class dcc.py installed when the app class could not be found. The
/// original `loader/DccApplication.smali` with the library name substituted.
fn loader_smali(loader: &str, lib_name: &str) -> String {
    let loader = loader.replace('.', "/");
    format!(
        r#".class public L{loader};
.super Landroid/app/Application;


# direct methods
.method static final constructor <clinit>()V
    .locals 1

    const-string v0, "{lib_name}"

    invoke-static {{v0}}, Ljava/lang/System;->loadLibrary(Ljava/lang/String;)V

    return-void
.end method

.method public constructor <init>()V
    .locals 0

    invoke-direct {{p0}}, Landroid/app/Application;-><init>()V

    return-void
.end method
"#
    )
}

/// The `<clinit>` body dcc.py appends when the app class already exists.
fn clinit_body(loader: &str, lib_name: &str, force_custom_loader: bool) -> String {
    if force_custom_loader {
        format!("    new-instance v0, L{};\n", loader.replace('.', "/"))
    } else {
        format!(
            "    const-string v0, \"{lib_name}\"\n    invoke-static {{v0}}, Ljava/lang/System;->loadLibrary(Ljava/lang/String;)V\n"
        )
    }
}

/// Inject the `System.loadLibrary` call: into the existing app class' `<clinit>`
/// when there is one, otherwise through the generated loader class (which also
/// replaces `android:name` of `<application>`), exactly like dcc.py.
pub fn inject_loader(
    input: &mut Input,
    lib_name: &str,
    loader: &str,
    force_custom_loader: bool,
) -> Result<()> {
    let Input::Apk {
        dexes,
        manifest,
        app_class,
        ..
    } = input
    else {
        // A bare dex has no manifest to hook into; dcc.py leaves the caller to
        // call `System.loadLibrary` itself.
        return Ok(());
    };

    // dcc.py needs the application class to actually exist in the dex, not just
    // in the manifest.
    let mut patched_app_class = false;
    if let Some(app_class) = app_class.clone() {
        if let Some(class) = dexes
            .iter_mut()
            .flat_map(|unit| unit.classes.iter_mut())
            .find(|c| c.name.as_java_type() == app_class)
        {
            let text =
                append_load_library(format!("{class}"), lib_name, loader, force_custom_loader);
            *class = SmaliClass::from_smali(&text)
                .with_context(|| format!("re-parsing application class {app_class}"))?;
            patched_app_class = true;
        }
    }

    // The loader class is installed when there is no application class to hook
    // into, and always with `--force-custom-loader` (dcc.py then only creates an
    // instance of it from the application class).
    if !patched_app_class || force_custom_loader {
        let loader_class = SmaliClass::from_smali(&loader_smali(loader, lib_name))
            .context("parsing the generated loader class")?;
        if let Some(unit) = dexes.last_mut() {
            unit.classes.push(loader_class);
        }
    }
    if !patched_app_class {
        set_manifest_application_name(manifest, loader);
    }
    Ok(())
}

/// Append (or extend) `<clinit>` in a rendered smali class document.
fn append_load_library(
    mut text: String,
    lib_name: &str,
    loader: &str,
    force_custom_loader: bool,
) -> String {
    let body = clinit_body(loader, lib_name, force_custom_loader);
    match text.find(".method static final constructor <clinit>()V") {
        Some(index) => {
            // Insert right after the first `.locals`/`.registers` line of the
            // method, growing the register count when it is zero.
            let rest = &text[index..];
            if let Some(offset) = rest
                .lines()
                .scan(0usize, |pos, line| {
                    let start = *pos;
                    *pos += line.len() + 1;
                    Some((start, line))
                })
                .find(|(_, line)| {
                    let line = line.trim_start();
                    line.starts_with(".locals ") || line.starts_with(".registers ")
                })
                .map(|(start, line)| {
                    let mut replacement = line.to_string();
                    if let Some(count) = replacement.trim_start().split_whitespace().nth(1) {
                        if count == "0" {
                            replacement = replacement.replace(".locals 0", ".locals 1");
                            replacement = replacement.replace(".registers 0", ".registers 1");
                        }
                    }
                    (start, replacement)
                })
            {
                let (line_start, replacement) = offset;
                let line_end = rest[line_start..]
                    .find('\n')
                    .map(|i| line_start + i + 1)
                    .unwrap_or(rest.len());
                let mut result = String::with_capacity(text.len() + body.len());
                result.push_str(&text[..index + line_start]);
                result.push_str(&replacement);
                result.push('\n');
                result.push_str(&body);
                result.push_str(&rest[line_end..]);
                text = result;
            }
        }
        None => {
            text.push_str(&format!(
                "\n.method static final constructor <clinit>()V\n    .locals 1\n{body}    return-void\n.end method\n"
            ));
        }
    }
    text
}

/// `android:extractNativeLibs` as the *input* manifest declares it, if it does.
///
/// Read before [`set_extract_native_libs`] overwrites it. An input that asked for
/// it is making a statement about *its own* libraries, and repacking is not
/// entitled to invalidate it: `true` is how an app gets its native libraries
/// unpacked into `ApplicationInfo.nativeLibraryDir`, and an app that runs one of
/// them as a program -- rather than `dlopen`-ing it -- has nothing to run if that
/// directory comes out empty. Only the app knows whether it does that, so its
/// answer is the one to keep.
fn declared_extract_native_libs(manifest: &AndroidManifest) -> Option<bool> {
    let application = manifest.application()?;
    let attribute = application
        .attributes
        .iter()
        .find(|a| a.name == "extractNativeLibs")?;
    match attribute.value {
        ManifestValue::Boolean(value) => Some(value),
        // Anything else is not a value the platform will honour here; treat it as
        // unspecified rather than guessing at what it meant.
        _ => None,
    }
}

/// What the output manifest gets: `--compress-libs` can turn extraction on, and
/// an input that already asked for it keeps it, but nothing here turns it off.
///
/// The two directions are not symmetric. On means the installer unpacks
/// `lib/**/*.so` to disk and the app is handed paths it can execute; off means
/// the loader `mmap`s them out of the APK and the native library directory is
/// left empty, which is silently wrong for the app described above and invisible
/// until it tries to start its first program.
fn effective_extract_native_libs(manifest: &AndroidManifest, compress_libs: bool) -> bool {
    compress_libs || declared_extract_native_libs(manifest).unwrap_or(false)
}

/// Set `android:extractNativeLibs` on `<application>`.
///
/// The flag has to match how `lib/**/*.so` is stored, and getting it wrong is
/// silent until load time: with the libraries deflated (`--compress-libs`) the
/// framework must unpack them at install, which is what `true` asks for. Leaving
/// it absent and still deflating the libraries is the state that fails on a
/// device, because the loader would try to map a compressed entry out of the
/// APK. Setting it explicitly also removes the dependence on the platform's
/// "all libraries happen to be uncompressed" inference.
///
/// The value comes from [`effective_extract_native_libs`], not straight from
/// `compress_libs`: see there for why an input's `true` has to survive a repack.
fn set_extract_native_libs(manifest: &mut AndroidManifest, extract: bool) {
    let app = manifest.ensure_application();
    let value = ManifestValue::Boolean(extract);
    match app
        .attributes
        .iter_mut()
        .find(|a| a.name == "extractNativeLibs")
    {
        Some(attribute) => set_attribute_value(attribute, value),
        None => app.attributes.push(ManifestAttribute {
            namespace_prefix: Some("android".to_string()),
            namespace_uri: Some(ANDROID_NS.to_string()),
            stored_type: None,
            stored_data: None,
            raw_value: None,
            resource_id: Some(ATTR_ANDROID_EXTRACT_NATIVE_LIBS),
            proto_source: None,
            proto_compiled_item: None,
            name: "extractNativeLibs".to_string(),
            value,
        }),
    }
}

/// Replace an existing attribute's value so that the *encoder* actually uses it.
///
/// An attribute parsed out of a binary manifest still carries the type tag and
/// raw data word it was read with, and `ManifestAttribute::encoded_type` returns
/// those in preference to re-encoding `value`. Writing only `value` therefore
/// produces an attribute whose typed data is the old one and whose raw text is
/// the new one: `aapt2 dump xmltree` shows `extractNativeLibs(...)=false (Raw:
/// "true")`, and the platform reads the typed half, so the change does not take
/// effect at all. Clearing the pair forces the encode from `value`, which is what
/// a freshly pushed attribute gets.
fn set_attribute_value(attribute: &mut ManifestAttribute, value: ManifestValue) {
    attribute.value = value;
    attribute.stored_type = None;
    attribute.stored_data = None;
    attribute.raw_value = None;
    attribute.proto_compiled_item = None;
}

fn set_manifest_application_name(manifest: &mut AndroidManifest, loader: &str) {
    let app = manifest.ensure_application();
    match app.attributes.iter_mut().find(|a| a.name == "name") {
        Some(attribute) => {
            set_attribute_value(attribute, ManifestValue::String(loader.to_string()))
        }
        None => app.attributes.push(ManifestAttribute {
            namespace_prefix: Some("android".to_string()),
            namespace_uri: Some(ANDROID_NS.to_string()),
            stored_type: None,
            stored_data: None,
            raw_value: None,
            resource_id: Some(ATTR_ANDROID_NAME),
            proto_source: None,
            proto_compiled_item: None,
            name: "name".to_string(),
            value: ManifestValue::String(loader.to_string()),
        }),
    }
}

/// Write the converted output: an APK back into an APK, a dex into a zip that
/// also carries `lib/<abi>/lib<name>.so`.
pub fn write_output(
    input: Input,
    out_path: &Path,
    libs: &[(String, std::path::PathBuf)],
    lib_name: &str,
    compress_libs: bool,
) -> Result<()> {
    let name_for_so = format!("lib{lib_name}.so");
    match input {
        Input::Apk {
            mut apk,
            dexes,
            mut manifest,
            manifest_entry,
            ..
        } => {
            for (name, bytes) in rebuild_dex_bytes(&dexes)? {
                apk.replace_entry(name, bytes)
                    .context("replacing dex in the APK")?;
            }
            // Keep the manifest's promise about how the libraries are stored in
            // step with `normalize_compression` below. An input that already asks
            // for extraction keeps asking for it: the flag is not ours to remove
            // (see `effective_extract_native_libs`).
            let extract_native_libs = effective_extract_native_libs(&manifest, compress_libs);
            set_extract_native_libs(&mut manifest, extract_native_libs);
            let encoded = manifest
                .to_apk_entry(Some(&manifest_entry))
                .context("encoding AndroidManifest.xml")?;
            apk.replace_entry_with("AndroidManifest.xml", encoded)
                .context("replacing AndroidManifest.xml")?;
            for (abi, so) in libs {
                let data =
                    std::fs::read(so).with_context(|| format!("reading {}", so.display()))?;
                apk.replace_entry(format!("lib/{abi}/{name_for_so}"), data)
                    .with_context(|| format!("packing {so:?}"))?;
            }
            // Last: every entry above was just replaced with a fresh one that
            // carries no compression hint. Compression follows the same decision
            // the manifest was just given, so the pair cannot disagree.
            normalize_compression(&mut apk, extract_native_libs)?;
            apk.write_to_file(out_path)
                .with_context(|| format!("writing {}", out_path.display()))?;
        }
        Input::Dex {
            file_name, dexes, ..
        } => {
            let dir = tempfile::tempdir().context("creating a temporary output directory")?;
            let root = dir.path();
            for (name, bytes) in rebuild_dex_bytes(&dexes)? {
                // A single dex input keeps its own file name in the archive.
                let target = if name == "classes.dex" {
                    file_name.clone()
                } else {
                    name
                };
                std::fs::write(root.join(target), bytes)?;
            }
            for (abi, so) in libs {
                let target = root.join("lib").join(abi).join(&name_for_so);
                std::fs::create_dir_all(target.parent().expect("lib dir"))?;
                std::fs::copy(so, &target)
                    .with_context(|| format!("copying {} to {}", so.display(), target.display()))?;
            }
            let packed = ApkFile::from_directory(root).context("packing the output zip")?;
            packed
                .write_to_file(out_path)
                .with_context(|| format!("writing {}", out_path.display()))?;
        }
    }
    Ok(())
}

/// Library entries the output is missing, so the caller can complain loudly
/// instead of producing an APK whose native methods cannot link.
///
/// A `--no-build` run legitimately passes an empty `libs`, and that is the
/// caller's own warning to print; here only the expected-but-absent case is
/// reported.
pub fn missing_libs(
    output: &Path,
    libs: &[(String, std::path::PathBuf)],
    lib_name: &str,
) -> Vec<String> {
    if libs.is_empty() {
        return Vec::new();
    }
    let Ok(apk) = ApkFile::from_file(output) else {
        return Vec::new();
    };
    libs.iter()
        .map(|(abi, _)| format!("lib/{abi}/lib{lib_name}.so"))
        .filter(|entry| apk.entry(entry).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--compress-libs` and `extractNativeLibs` describe the same decision, so
    /// the two functions have to agree; the failure mode of a mismatch is a
    /// load-time error on a device, not a build error here.
    #[test]
    fn lib_compression_matches_extract_native_libs() {
        assert_eq!(
            agp_compression("lib/arm64-v8a/libstub.so", true),
            Some(ApkCompression::Deflated)
        );
        assert_eq!(
            agp_compression("lib/arm64-v8a/libstub.so", false),
            Some(ApkCompression::Stored)
        );
        // `resources.arsc` and the dex files keep their own rule either way.
        assert_eq!(
            agp_compression("resources.arsc", true),
            Some(ApkCompression::Stored)
        );
        assert_eq!(
            agp_compression("classes.dex", true),
            Some(ApkCompression::Deflated)
        );
    }

    #[test]
    fn extract_native_libs_is_set_and_updated_in_place() {
        let mut manifest = AndroidManifest::default();
        set_extract_native_libs(&mut manifest, true);
        let app = manifest.ensure_application();
        let attribute = app
            .attributes
            .iter()
            .find(|a| a.name == "extractNativeLibs")
            .expect("the attribute is added");
        assert_eq!(
            attribute.resource_id,
            Some(ATTR_ANDROID_EXTRACT_NATIVE_LIBS)
        );
        assert_eq!(attribute.value, ManifestValue::Boolean(true));
        assert_eq!(app.attributes.len(), 1, "only one copy may exist");

        // Setting it twice must not append a duplicate: the encoder would emit
        // both and the platform rejects an attribute repeated in one element.
        set_extract_native_libs(&mut manifest, false);
        let app = manifest.ensure_application();
        assert_eq!(app.attributes.len(), 1);
        assert_eq!(app.attributes[0].value, ManifestValue::Boolean(false));
    }

    /// An input that asks for its libraries to be extracted keeps asking for it.
    ///
    /// This is the rule that was missing, and the failure it caused is worth
    /// recording: the app that found it ships PRoot as `lib/<abi>/librodroid-
    /// proot.so`, marked executable by `useLegacyPackaging = true`, precisely
    /// because running a program means running a file the platform unpacked into
    /// `nativeLibraryDir`. A repack that rewrote the flag to `false` -- which is
    /// what tying it to `--compress-libs` did, since that defaults off -- left
    /// that directory empty, and the app stopped with "this build of the app
    /// carries no environment runner", a message about a missing file that had
    /// nothing to do with the conversion.
    #[test]
    fn extract_native_libs_from_the_input_is_never_turned_off() {
        // Absent: only `--compress-libs` can ask for extraction.
        assert!(!effective_extract_native_libs(
            &AndroidManifest::default(),
            false
        ));
        assert!(effective_extract_native_libs(
            &AndroidManifest::default(),
            true
        ));

        // The input asked for it, and the repack passes no flag: it survives.
        let mut wanted = AndroidManifest::default();
        set_extract_native_libs(&mut wanted, true);
        assert!(declared_extract_native_libs(&wanted) == Some(true));
        assert!(effective_extract_native_libs(&wanted, false));
        assert!(effective_extract_native_libs(&wanted, true));

        // An input that asked for the modern layout keeps it too, and
        // `--compress-libs` can still turn extraction on for it.
        let mut modern = AndroidManifest::default();
        set_extract_native_libs(&mut modern, false);
        assert!(!effective_extract_native_libs(&modern, false));
        assert!(effective_extract_native_libs(&modern, true));
    }

    /// The value a *device* sees for an attribute that already existed in the
    /// input manifest.
    ///
    /// A parsed attribute carries its original type tag and data word, and the
    /// binary XML encoder prefers those over `value`. Writing the value alone
    /// leaves the platform reading the old data -- `aapt2 dump xmltree` showed
    /// `extractNativeLibs(...)=false (Raw: "true")` -- so the setter has to clear
    /// the stored encoding. This is the bug the `-C` packaging check caught; it
    /// applies equally to the custom-loader class name.
    #[test]
    fn updating_a_parsed_attribute_clears_its_stored_encoding() {
        let mut manifest = AndroidManifest::default();
        // A stand-in for an attribute read out of a binary manifest: typed data
        // present, `Boolean(false)` originally.
        manifest
            .ensure_application()
            .attributes
            .push(ManifestAttribute {
                namespace_prefix: Some("android".to_string()),
                namespace_uri: Some(ANDROID_NS.to_string()),
                stored_type: Some(0x12),
                stored_data: Some(0),
                raw_value: Some("false".to_string()),
                resource_id: Some(ATTR_ANDROID_EXTRACT_NATIVE_LIBS),
                proto_source: None,
                proto_compiled_item: None,
                name: "extractNativeLibs".to_string(),
                value: ManifestValue::Boolean(false),
            });

        set_extract_native_libs(&mut manifest, true);
        let attribute = &manifest.ensure_application().attributes[0];
        assert_eq!(attribute.value, ManifestValue::Boolean(true));
        assert_eq!(
            attribute.stored_type, None,
            "the old type tag would win over the new value"
        );
        assert_eq!(attribute.stored_data, None);
        assert_eq!(attribute.raw_value, None, "re-derived from the new value");
    }

    #[test]
    fn dex_versions_map_to_api_levels() {
        // dcc.py: 035 -> 23 -> minSdk 21, 037 -> 25 -> 24, 038.. -> 28.
        assert_eq!(min_sdk_from_dex_version(35), 21);
        assert_eq!(min_sdk_from_dex_version(37), 24);
        assert_eq!(min_sdk_from_dex_version(38), 26);
        assert_eq!(min_sdk_from_dex_version(39), 26);
        assert_eq!(min_sdk_from_dex_version(40), 26);
    }

    #[test]
    fn clinit_is_appended_when_missing() {
        let text =
            ".class public Lcom/example/App;\n.super Landroid/app/Application;\n".to_string();
        let patched = append_load_library(text, "stub", "amimo.dcc.DccApplication", false);
        assert!(patched.contains(".method static final constructor <clinit>()V"));
        assert!(patched.contains("const-string v0, \"stub\""));
        assert!(patched.contains("System;->loadLibrary"));
    }

    #[test]
    fn clinit_register_count_grows_from_zero() {
        let text = ".class public Lcom/example/App;\n.super Landroid/app/Application;\n\n.method static final constructor <clinit>()V\n    .locals 0\n\n    return-void\n.end method\n".to_string();
        let patched = append_load_library(text, "mylib", "Loader", false);
        assert!(patched.contains(".locals 1"), "{patched}");
        assert!(patched.contains("const-string v0, \"mylib\""), "{patched}");
        // Still exactly one <clinit>.
        assert_eq!(patched.matches("constructor <clinit>").count(), 1);
    }

    #[test]
    fn forced_custom_loader_news_the_loader_class() {
        let body = clinit_body("a.b.Loader", "stub", true);
        assert_eq!(body, "    new-instance v0, La/b/Loader;\n");
    }

    #[test]
    fn dex_is_deflated_and_libraries_default_to_stored() {
        assert_eq!(
            agp_compression("classes.dex", false),
            Some(ApkCompression::Deflated)
        );
        assert_eq!(
            agp_compression("classes3.dex", false),
            Some(ApkCompression::Deflated)
        );
        assert_eq!(
            agp_compression("lib/arm64-v8a/libstub.so", false),
            Some(ApkCompression::Stored)
        );
        assert_eq!(
            agp_compression("resources.arsc", false),
            Some(ApkCompression::Stored)
        );
        // Keeps whatever the input had.
        assert_eq!(agp_compression("res/drawable/icon.png", false), None);
        assert_eq!(agp_compression("assets/dexopt/baseline.prof", false), None);
    }

    #[test]
    fn stored_entries_the_writer_would_not_store_are_deflated() {
        // `*.dex` and `*.so` are stored by the writer and aligned; the writer
        // treats an entry it did not classify as stored as unpadded, so an
        // inherited `Stored` flag there has to become `Deflated`.
        assert!(writer_stores_uncompressed("classes.dex"));
        assert!(writer_stores_uncompressed("lib/arm64-v8a/libstub.so"));
        assert!(writer_stores_uncompressed("resources.arsc"));
        assert!(!writer_stores_uncompressed("assets/dexopt/baseline.prof"));
        assert!(!writer_stores_uncompressed("assets/dexopt/baseline.profm"));
    }
}
