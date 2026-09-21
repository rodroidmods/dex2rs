//! `dex2rs` — Dalvik bytecode to Rust/JNI ahead-of-time compiler.
//!
//! The command line is the Rust port of `d2c/dcc.py`: same options, same config
//! file, same output layout. The pipeline is:
//!
//! ```text
//! apk/dex -> smali (smali crate) -> compile (dex2rs-core) -> generated crate
//!         -> cargo ndk build -> patched dex -> repacked apk/dex -> signed
//! ```

pub mod apk;
pub mod build;
pub mod config;
pub mod filter;
pub mod scaffold;
pub mod sign;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use rayon::prelude::*;
use smali::types::{SmaliClass, SmaliMethod};

use config::{DccConfig, SignatureConfig};
use filter::{MethodFilter, MethodTriple};
use scaffold::GeneratedMethod;

/// Shown under `--help`: the commands people actually run.
///
/// The flag list above it answers "what can I pass"; this answers "what do I
/// type", which is the question that costs a reader the most time.
const EXAMPLES: &str = "\
EXAMPLES

  Convert and sign an app, for one device, with names and strings hidden:
    dex2rs -i app-release.apk -o app-protected.apk -f filter.txt -p -d -C \
           -k --abis arm64-v8a --opt-level z

  One device ABI instead of four is the biggest single saving in build time.
  `--abis` on its own is ignored for an apk that ships libraries: the apk decides
  which ABIs are built, so `-k` is what makes `--abis` win.

  What the short flags mean:
    -i   input: an apk, or a bare .dex
    -o   output: a signed apk, or a .zip when the input is a dex
    -f   filter file: which methods to convert
    -p   obfuscate string constants and reflection names
    -d   register natives with RegisterNatives instead of Java_* symbols
    -C   deflate lib/*.so inside the apk (smaller download)
    -l   library name: lib<NAME>.so, default `stub`
    -b   do not build; pack a library already in libs/, or write the sources
    -e   where the generated Rust project goes (reused when it exists)
    -z   zip the generated project to this path (used when -b has no library)

  Sources only, no NDK build (finish later with the same -e):
    dex2rs -i app-release.apk -f filter.txt -b -e out/project -z out/project.zip

  Pack a library this project already built, without rebuilding it:
    dex2rs -i app-release.apk -o app-protected.apk -f filter.txt -b -e out/project

  Size against speed, on a 3 729-method app, arm64:
    --opt-level z        7.64 MB libstub.so, 10.24 MB apk   (the default)
    --outline-common     8.81 MB libstub.so, 10.85 MB apk   (-O3, outliner on)
    --opt-level 3       12.37 MB libstub.so, 12.60 MB apk   (fastest to run)

FILTERS (-f)
  One rule per line, matched against the concatenated `L<Package>/<Class>;<name>(<proto>)`
  -- for example `Lcom/example/Hello;add(II)I`. A method is converted when some plain
  rule matches and no `!` rule does (`!` is checked first, whatever the order); `=`
  matches that whole string exactly. A filter with no rule converts nothing.
    Lcom/example/.*;.*                every method of the package and below
    !Lcom/example/Secrets;.*          never these, whatever else matches
    =Lcom/example/Hello;add(II)I      exactly this one method
  Lines starting with # are comments.";

/// What a `--help` reader needs that a flag list cannot say: which commands to
/// run, and what each short flag is for.
const AFTER_HELP: &str = EXAMPLES;

#[derive(Parser, Debug)]
#[command(
    name = "dex2rs",
    about = "Convert Android dalvik bytecode into Rust/JNI native code",
    long_about = "Convert Android dalvik bytecode into Rust/JNI native code.\n\n\
                  The methods a filter selects are compiled to Rust, built into \
                  lib<name>.so with cargo ndk, and the dex is patched so those methods are \
                  `native` and the library loads with the app. Options and config file match \
                  d2c/dcc.py.",
    after_help = AFTER_HELP,
    version
)]
struct Args {
    /// Input apk/dex file path
    #[arg(short, long, help_heading = "Input and output")]
    input: PathBuf,

    /// Output file path, default: output.apk for APK, output.zip for DEX
    #[arg(short, long, help_heading = "Input and output")]
    output: Option<String>,

    /// Configuration file providing defaults for the options above
    #[arg(
        long,
        default_value = "dcc.cfg",
        help_heading = "Input and output",
        value_name = "FILE"
    )]
    config: PathBuf,

    /// Method filters configuration file
    #[arg(
        short,
        long,
        default_value = "filter.txt",
        help_heading = "Method selection",
        value_name = "FILE"
    )]
    filter: String,

    /// Skip synthetic methods in all classes
    #[arg(short, long, help_heading = "Method selection")]
    skip_synthetic: bool,

    /// Do not skip init methods (<init>/<clinit>), which normally stay in the dex
    #[arg(short, long, help_heading = "Method selection")]
    allow_init: bool,

    /// Emit the original writer's per-instruction LOGD traces
    #[arg(long, help_heading = "Method selection")]
    trace: bool,

    /// Encrypt string constants and the reflection names (classes, methods,
    /// fields, and the RegisterNatives table), decrypting them at the call
    #[arg(short = 'p', long, help_heading = "Protection")]
    obfuscate: bool,

    /// Export native methods using RegisterNatives instead of Java_* symbols,
    /// so the library lists no converted method in its exports
    #[arg(short, long, help_heading = "Protection")]
    dynamic_register: bool,

    /// Library name: the output is lib<NAME>.so, default stub
    #[arg(
        short,
        long,
        default_value = "stub",
        help_heading = "Loader",
        value_name = "NAME"
    )]
    lib_name: String,

    /// Loader class, default: amimo.dcc.DccApplication
    #[arg(
        short,
        long,
        default_value = "amimo.dcc.DccApplication",
        help_heading = "Loader",
        value_name = "CLASS"
    )]
    custom_loader: String,

    /// Force use of custom loader
    #[arg(short = 'r', long, help_heading = "Loader")]
    force_custom_loader: bool,

    /// Do not build the compiled code.
    ///
    /// Packs the libraries a previous build left in `libs/`, so a build can be
    /// repeated without recompiling. With no library there, it writes the
    /// generated project and zips it to `--project-archive` instead, and the
    /// output apk contains no library: an artifact to inspect, not to install.
    #[arg(short = 'b', long, help_heading = "Building")]
    no_build: bool,

    /// Where the generated Rust project is written; reused as-is when it
    /// already exists, which is how a `-b`/build/`-b` cycle stays on one tree
    #[arg(short = 'e', long, help_heading = "Building", value_name = "DIR")]
    source_dir: Option<PathBuf>,

    /// Zip the generated project to this file, written on a `--no-build` run
    /// that has no library to pack. `target/` and `libs/` are left out
    #[arg(
        short = 'z',
        long,
        default_value = "project-source.zip",
        help_heading = "Building",
        value_name = "FILE"
    )]
    project_archive: String,

    /// Build these ABIs, as a comma separated list: arm64-v8a, armeabi-v7a,
    /// x86, x86_64.
    ///
    /// An apk that already ships libraries decides which ABIs are built, so this
    /// only applies when the input has none -- or alongside `-k`, which is what
    /// makes it win. One ABI (the one your device uses) is by far the cheapest
    /// way to cut build time.
    #[arg(long, help_heading = "Building", value_name = "LIST")]
    abis: Option<String>,

    /// Use the ABIs from `--abis` (or the config) even when the input apk ships
    /// its own, and skip the per-abi check against the apk
    #[arg(short = 'k', long, help_heading = "Building")]
    force_keep_libs: bool,

    /// Android API level for the cargo-ndk build (default: the input's
    /// minSdkVersion)
    #[arg(long, help_heading = "Building", value_name = "LEVEL")]
    api: Option<u32>,

    /// Deflate `lib/**/*.so` in the output APK instead of storing it, and set
    /// `android:extractNativeLibs="true"` so the installer unpacks them. Saves
    /// several MiB of download (the converted code is stored uncompressed
    /// otherwise) at the cost of extracting the libraries at install time.
    #[arg(short = 'C', long, help_heading = "Size")]
    compress_libs: bool,

    /// `opt-level` for the generated library: `z` (default) or `3` (fastest to
    /// run, and about 60% larger).
    ///
    /// Not a taste setting. On the reference app (3 729 methods, arm64) `3` gives
    /// a 12.37 MB `libstub.so` and `z` 7.64 MB, and the split is worth knowing
    /// before reaching for either: disabling inlining at `3` saves just 1.32 MB,
    /// while LLVM's machine outliner -- which `z` runs and `3` does not -- is the
    /// other 4.46 MB. See `--outline-common` for keeping `3` and getting most of
    /// that back. `2` and `s` both measured *larger* than `3` here.
    #[arg(long, help_heading = "Size", value_name = "LEVEL")]
    opt_level: Option<String>,

    /// Force LLVM's machine outliner on at `opt-level 3` (it otherwise only runs
    /// at `z`).
    ///
    /// It undoes the duplication `-O3`'s inliner creates: 30 066 short runtime
    /// helper sequences that had been copied up to 8 291 times each, 447 761 call
    /// sites in all. On the reference app this takes `libstub.so` from 12.37 MB
    /// to **8.81 MB** with everything outside the outlined fragments still `-O3`:
    /// three quarters of the way to `--opt-level z`'s 7.64 MB without moving the
    /// optimizer down. The cost is a call where `-O3` had the sequence inline.
    #[arg(long, help_heading = "Size")]
    outline_common: bool,

    /// Keep the debug tables (line numbers, local variable names) in the
    /// output dex. Dropped by default: they are megabytes on a real app and
    /// are rebuilt in a much larger form by the smali assembler.
    #[arg(long, help_heading = "Size")]
    keep_debug_info: bool,
}

/// The CLI options after the `dcc.cfg` merge.
struct Options {
    input: PathBuf,
    output: Option<String>,
    obfuscate: bool,
    trace: bool,
    dynamic_register: bool,
    filter: String,
    custom_loader: String,
    force_custom_loader: bool,
    skip_synthetic: bool,
    allow_init: bool,
    lib_name: String,
    no_build: bool,
    force_keep_libs: bool,
    source_dir: Option<PathBuf>,
    project_archive: String,
    abis: Option<Vec<String>>,
    min_sdk: Option<u32>,
    ndk_dir: Option<String>,
    signature: SignatureConfig,
    keep_debug_info: bool,
    compress_libs: bool,
    opt_level: String,
    outline_common: bool,
}

/// dcc.py: the config value wins only while the CLI argument keeps its default.
fn pick(cli: &str, default: &str, config: &Option<String>) -> String {
    if cli == default {
        config
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| cli.to_string())
    } else {
        cli.to_string()
    }
}

impl Options {
    fn merge(args: Args, config: DccConfig, config_dir: &Path) -> Result<Self> {
        let mut signature = config.signature.clone();
        let keystore = Path::new(&signature.keystore_path);
        if !keystore.is_file() {
            // Configs are often run from a different working directory; try the
            // path relative to the config file before giving up.
            let candidate = config_dir.join(keystore);
            if candidate.is_file() {
                signature.keystore_path = candidate.to_string_lossy().to_string();
            }
        }
        Ok(Options {
            input: args.input,
            output: args
                .output
                .clone()
                .or_else(|| config.output.clone().filter(|value| !value.is_empty())),
            obfuscate: DccConfig::flag_or(args.obfuscate, config.obfuscate),
            trace: DccConfig::flag_or(args.trace, config.trace),
            dynamic_register: DccConfig::flag_or(args.dynamic_register, config.dynamic_register),
            filter: pick(&args.filter, "filter.txt", &config.filter),
            custom_loader: pick(
                &args.custom_loader,
                "amimo.dcc.DccApplication",
                &config.custom_loader,
            ),
            force_custom_loader: DccConfig::flag_or(
                args.force_custom_loader,
                config.force_custom_loader,
            ),
            skip_synthetic: DccConfig::flag_or(args.skip_synthetic, config.skip_synthetic),
            allow_init: DccConfig::flag_or(args.allow_init, config.allow_init),
            lib_name: pick(&args.lib_name, "stub", &config.lib_name),
            no_build: DccConfig::flag_or(args.no_build, config.no_build),
            force_keep_libs: DccConfig::flag_or(args.force_keep_libs, config.force_keep_libs),
            source_dir: args.source_dir.or_else(|| {
                config
                    .source_dir
                    .clone()
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            }),
            project_archive: pick(
                &args.project_archive,
                "project-source.zip",
                &config.project_archive,
            ),
            abis: args
                .abis
                .as_deref()
                .map(|list| {
                    list.split(',')
                        .map(|abi| abi.trim().to_string())
                        .filter(|abi| !abi.is_empty())
                        .collect::<Vec<_>>()
                })
                .or_else(|| config.abis.clone()),
            min_sdk: args.api.or(config.min_sdk),
            // An empty `ndk_dir` means "let cargo-ndk find the NDK" (it reads
            // ANDROID_NDK_HOME/ANDROID_NDK_ROOT itself).
            ndk_dir: config.ndk_dir.clone().filter(|dir| !dir.is_empty()),
            signature,
            keep_debug_info: DccConfig::flag_or(args.keep_debug_info, config.keep_debug_info),
            compress_libs: DccConfig::flag_or(args.compress_libs, config.compress_libs),
            opt_level: opt_level(args.opt_level, config.opt_level)?,
            outline_common: DccConfig::flag_or(args.outline_common, config.outline_common),
        })
    }
}

/// The default `opt-level` of the generated release profile.
///
/// `z`, not `3`: on the reference app it is both smaller (7.64 MB against 12.37 MB
/// of `libstub.so`) and *quicker to compile* (3m 01s against 3m 46s, one ABI), and
/// it is what this repository's `dcc.cfg` asks for -- so an installed `dex2rs`
/// with no config file beside it behaves the same way the documentation says it
/// does. `3` is still the fastest to run, and the one to pass when dispatch speed
/// matters more than the download.
const DEFAULT_OPT_LEVEL: &str = "z";

/// Which `opt-level` the generated `Cargo.toml` gets.
///
/// Validated here rather than passed through: a typo would otherwise reach cargo
/// as an unknown profile value and fail a three-minute build with a message about
/// the manifest, not about the flag.
fn opt_level(cli: Option<String>, config: Option<String>) -> Result<String> {
    let level = cli
        .or_else(|| config.filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_OPT_LEVEL.to_string());
    match level.as_str() {
        "0" | "1" | "2" | "3" | "s" | "z" => Ok(level),
        other => bail!(
            "--opt-level {other}: cargo accepts 0, 1, 2, 3, \"s\" or \"z\". Only z \
             (the default, about 40% smaller) and 3 (the fastest) are worth choosing between."
        ),
    }
}

/// Parse `args` (without the program name) and run a conversion.
pub fn run_from(args: &[String]) -> Result<()> {
    let mut argv = vec!["dex2rs".to_string()];
    argv.extend_from_slice(args);
    let args = Args::try_parse_from(argv)?;
    let config_dir = args
        .config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let config = DccConfig::load(&args.config)?;
    let options = Options::merge(args, config, &config_dir)?;
    convert(options)
}

fn convert(options: Options) -> Result<()> {
    let mut input = apk::load(&options.input)?;
    let min_sdk = options.min_sdk.unwrap_or_else(|| input.min_sdk());
    let out_file = options.output.clone().unwrap_or_else(|| {
        if input.is_apk() {
            "output.apk".to_string()
        } else {
            "output.zip".to_string()
        }
    });

    // ABIs: prefer the ones the APK already ships (Application.mk was rewritten
    // the same way).
    let abis = match &input {
        apk::Input::Apk { apk, .. } if !options.force_keep_libs => {
            let entries = apk.entry_names().map(str::to_string).collect::<Vec<_>>();
            let found = build::abis_from_apk(entries.into_iter())?;
            if found.is_empty() {
                eprintln!(" No lib abis found in the apk, using the default ones");
                default_abis(&options)
            } else {
                found
            }
        }
        _ => default_abis(&options),
    };

    let (methods, errors, skipped) = compile_all(&input, &options)?;
    if !errors.is_empty() {
        eprintln!(" ================================");
        for error in &errors {
            eprintln!(" {error}");
        }
        eprintln!(" ================================");
    }
    if methods.is_empty() {
        bail!(
            "no methods compiled; check the filter file ({})",
            options.filter
        );
    }
    eprintln!(" Compiled {} methods", methods.len());
    if skipped > 0 {
        // Abstract and interface methods have no bytecode to convert. Anything
        // else that went missing here would show up as a filter-selected method
        // with no compiled body, so say the number out loud rather than letting
        // a silently dropped method look like a successful conversion.
        eprintln!(" Skipped {skipped} selected methods that have no bytecode (abstract/interface)");
    }

    // Scaffold the generated project. A user supplied `--source-dir` that
    // already exists is reused as-is (dcc.py only copies `project/` when the
    // directory is missing); a temporary one is always freshly scaffolded.
    let temporary;
    let (project_dir, fresh_project) = match &options.source_dir {
        Some(dir) if dir.exists() => {
            eprintln!(" Reusing the existing project directory {}", dir.display());
            // The generated `methods/` are rewritten below either way, but the
            // vendored `runtime/` is the tool's own code and is only written by
            // `create_project`. Leaving it alone here means a reused tree builds
            // today's converted methods on top of whenever-it-was-scaffolded JNI
            // helpers, which is invisible in the output and has already cost a
            // round of "the fix is in the source but not in the APK".
            scaffold::vendor_runtime(dir)?;
            (dir.clone(), false)
        }
        Some(dir) => (dir.clone(), true),
        None => {
            std::fs::create_dir_all(".tmp").ok();
            temporary = tempfile::Builder::new()
                .prefix("dcc-project-")
                .tempdir_in(".tmp")
                .or_else(|_| tempfile::tempdir())
                .context("creating the project directory")?;
            (temporary.path().to_path_buf(), true)
        }
    };
    if fresh_project {
        scaffold::create_project(&project_dir, &options.lib_name)?;
    }
    scaffold::write_methods(&project_dir, &methods)?;
    scaffold::write_dynamic_register(
        &project_dir,
        &methods,
        options.dynamic_register,
        options.obfuscate,
    )?;
    scaffold::set_trace_feature(&project_dir, options.trace)?;
    scaffold::set_opt_level(&project_dir, &options.opt_level)?;
    scaffold::set_outline_common(&project_dir, options.outline_common)?;

    let libs = if options.no_build {
        // `--no-build` still packs a library a previous build left in `libs/`
        // (the C++ tool always wrote the APK with whatever `ndk-build` had
        // produced): only a run with nothing to pack archives the sources.
        let found = existing_libs(&project_dir, &options.lib_name, &abis);
        if found.is_empty() {
            if input.is_apk() {
                eprintln!(
                    " WARNING: --no-build and no libraries in {}; {out_file} will NOT contain \
                     lib{}.so and every converted method will fail to link on the device. Build the \
                     library first (`cargo ndk -o libs build --release` in the project directory) or \
                     drop --no-build.",
                    project_dir.join("libs").display(),
                    options.lib_name
                );
            }
            eprintln!(
                " --no-build: no library in {} to pack, so the sources are the output",
                project_dir.join("libs").display()
            );
            scaffold::archive_project(&project_dir, Path::new(&options.project_archive))?;
            eprintln!(
                "   sources: {}\n   archive: {}\n   finish: run `cargo ndk -o libs build \
                 --release` in it, then this command again with `-e {}` to pack the library",
                project_dir.display(),
                options.project_archive,
                project_dir.display()
            );
            Vec::new()
        } else {
            check_build_record(&project_dir)?;
            let packed_abis = found
                .iter()
                .map(|(abi, _)| abi.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                " --no-build: packing the {} already built in libs/ ({packed_abis})",
                found.len()
            );
            // One ABI is the normal case: a device build produces one, and the
            // input APK advertises four. Say which ones the output cannot serve
            // rather than letting the missing library show up on a phone.
            for abi in abis_without_libs(&abis, &found) {
                eprintln!(
                    " WARNING: no lib{}.so for {abi}: the {out_file} methods stay native with no \
                     implementation, so they fail on {abi} devices. Build that ABI too, or \
                     exclude it with --abis.",
                    options.lib_name
                );
            }
            found
        }
    } else {
        build::build_project(
            &project_dir,
            &options.lib_name,
            &abis,
            options.ndk_dir.as_deref(),
            min_sdk,
        )?
    };

    // Patch the dexes: compiled methods become `native`, the loader loads the lib.
    let compiled: BTreeSet<MethodTriple> = methods.keys().cloned().collect();
    apk::patch_compiled_methods(input.dexes_mut(), &compiled, !options.keep_debug_info);
    if input.is_apk() {
        apk::inject_loader(
            &mut input,
            &options.lib_name,
            &options.custom_loader,
            options.force_custom_loader,
        )?;
        // Sign into place through a temporary unsigned file.
        let unsigned = tempfile::Builder::new()
            .prefix("dcc-unsigned-")
            .suffix(".apk")
            .tempfile()
            .context("creating a temporary apk")?;
        let unsigned_path = unsigned.path().to_path_buf();
        apk::write_output(
            input,
            &unsigned_path,
            &libs,
            &options.lib_name,
            options.compress_libs,
        )?;
        sign::sign(
            &unsigned_path,
            Path::new(&out_file),
            min_sdk,
            &options.signature,
        )?;
        // A missing library entry means every converted method throws
        // UnsatisfiedLinkError on the device, which is worth shouting about.
        let missing = apk::missing_libs(Path::new(&out_file), &libs, &options.lib_name);
        for name in &missing {
            eprintln!(" WARNING: {out_file} does not contain {name}");
        }
        eprintln!(" Completed: {out_file}");
    } else {
        apk::write_output(
            input,
            Path::new(&out_file),
            &libs,
            &options.lib_name,
            options.compress_libs,
        )?;
        eprintln!(
            " Completed: {out_file} (a bare dex has no manifest: call \
             System.loadLibrary(\"{}\") from your own code)",
            options.lib_name
        );
    }
    Ok(())
}

/// The `libs/<abi>/lib<name>.so` of a previous build, when every requested ABI
/// has one.
/// The libraries already sitting in the project's `libs/`, one per ABI that
/// actually has one.
///
/// Deliberately **not** all-or-nothing. It used to return `None` unless every
/// ABI the input APK advertises had been built, which made `--no-build` report
/// "no libraries in libs/" and write an APK with no library at all for the
/// normal workflow of building one ABI for one device: the input advertises four
/// ABIs, `cargo ndk -t arm64-v8a` produces one, and the APK shipped without
/// `lib<name>.so` while a perfectly good one sat in `libs/`. An ABI with no
/// library is now the caller's warning to print, like the empty case was.
fn existing_libs(project_dir: &Path, lib_name: &str, abis: &[String]) -> Vec<(String, PathBuf)> {
    let mut found = Vec::with_capacity(abis.len());
    for abi in abis {
        let so = project_dir
            .join("libs")
            .join(abi)
            .join(format!("lib{lib_name}.so"));
        if so.is_file() {
            found.push((abi.clone(), so));
        }
    }
    found
}

/// ABIs in `abis` with no library built for them.
fn abis_without_libs(abis: &[String], libs: &[(String, PathBuf)]) -> Vec<String> {
    abis.iter()
        .filter(|abi| !libs.iter().any(|(have, _)| have == *abi))
        .cloned()
        .collect()
}

fn default_abis(options: &Options) -> Vec<String> {
    options.abis.clone().unwrap_or_else(|| {
        build::DEFAULT_ABIS
            .iter()
            .map(|abi| abi.to_string())
            .collect()
    })
}

/// Refuse to pack libraries that were built from different sources.
///
/// `--no-build` is the one path where the dex and the library can disagree: the
/// dex is patched from the methods compiled in this run, while the library is
/// whatever `libs/` happens to hold. When they disagree the build is clean and
/// the app fails on the device — the dex marks a method `native`, the library
/// does not implement it, and the call throws `UnsatisfiedLinkError` (a removed
/// method, or a stale implementation, depending on the direction).
///
/// A library this tool built carries a fingerprint of the sources it was built
/// from, so a mismatch is detectable exactly. A library built by hand carries
/// nothing, and that is reported rather than guessed at.
fn check_build_record(project_dir: &Path) -> Result<()> {
    let Some(recorded) = scaffold::recorded_build(project_dir) else {
        eprintln!(
            " --no-build: {} has no build record from this tool, so the libraries in it \
             are packed without checking that they match the generated sources",
            project_dir
                .join("libs")
                .join(scaffold::BUILD_RECORD)
                .display()
        );
        return Ok(());
    };
    let current = scaffold::sources_fingerprint(project_dir)?;
    if recorded == current {
        eprintln!(" --no-build: the libraries match the generated sources ({current})");
        return Ok(());
    }
    bail!(
        "the libraries in {} were built from different sources ({recorded}) than the ones \
         generated now ({current}): the output would patch the dex for one set of methods while \
         shipping a library for another, and the app would fail at the first converted call. \
         Re-run without --no-build to rebuild them, or delete {} to build from scratch.",
        project_dir.join("libs").display(),
        project_dir.join("libs").display()
    )
}

/// Compile every method the filter selects, per dex (dcc.py built one
/// `MethodFilter` and one `Dex2C` per dex, and so does this).
fn compile_all(
    input: &apk::Input,
    options: &Options,
) -> Result<(BTreeMap<MethodTriple, GeneratedMethod>, Vec<String>, usize)> {
    let mut methods: BTreeMap<MethodTriple, GeneratedMethod> = BTreeMap::new();
    let mut errors = Vec::new();
    let mut skipped = 0usize;
    // Every method the filter selects must end up in exactly one of: `methods`,
    // `errors`, or the no-bytecode `skipped` count. A gap means a method
    // vanished without a trace -- the old converter's 220-character JNI-name
    // limit did exactly that (silently, at debug level), and a conversion that
    // quietly drops methods is worse than one that fails.
    let mut selected = 0usize;

    for unit in input.dexes() {
        // Selected methods of this dex, compiled in parallel at the end of the
        // loop body.
        let mut jobs: Vec<(MethodTriple, String, &SmaliClass, &SmaliMethod)> = Vec::new();
        let filter = MethodFilter::new(
            Path::new(&options.filter),
            &unit.classes,
            options.skip_synthetic,
            options.allow_init,
        )?;
        for class in &unit.classes {
            for method in &class.methods {
                if !filter.should_compile(class, method) {
                    continue;
                }
                selected += 1;
                // Abstract/interface methods have no code: dcc.py's
                // `IrBuilder.process()` returns `None` for them and they are
                // skipped without an error, so do the same. Counted, so that
                // every selected method is accounted for below.
                if !method
                    .ops
                    .iter()
                    .any(|op| matches!(op, smali::types::SmaliOp::Op(_)))
                {
                    skipped += 1;
                    continue;
                }
                let triple: MethodTriple = (
                    class.name.as_jni_type(),
                    method.name.clone(),
                    method.signature.to_jni(),
                );
                let jni_name = dex2rs_core::util::jni_long_name(&triple.0, &triple.1, &triple.2);
                // Note: no length limit on `jni_name`.
                //
                // dcc.py dropped every method whose JNI long name exceeded 220
                // characters, because it wrote one `.cpp` per method with the
                // symbol *as the file name* (a filesystem limit) -- and it
                // logged that skip at debug level, so a normal run never showed
                // it. The methods that reach 220 are ordinary Kotlin ones:
                // `$default` bridges with wide signatures, lambdas, and
                // `access$...` accessors. In an app whose whole point is to
                // convert every selected method, each of those silently lost its
                // conversion. There is no such constraint here: all methods end
                // up in one `methods.rs`, and neither the linker nor ART cares
                // how long an exported symbol is.
                jobs.push((triple, jni_name, class, method));
            }
        }

        // The per-method compile is independent -- `compile_method` builds its
        // own IR and the core keeps no shared mutable state -- and it is the
        // long pole of a full conversion, so the work is spread over the pool.
        // `collect` on an indexed parallel iterator preserves the input order,
        // so the error list and the generated file stay byte-identical to the
        // sequential run whatever the scheduling does.
        let compiled: Vec<(MethodTriple, String, Result<String>)> = jobs
            .into_par_iter()
            .map(|(triple, jni_name, class, method)| {
                (triple, jni_name, compile_method(class, method, options))
            })
            .collect();

        for (triple, jni_name, source) in compiled {
            match source {
                Ok(source) => {
                    let prototype = triple.2.clone();
                    methods.insert(
                        triple.clone(),
                        GeneratedMethod {
                            triple,
                            jni_name,
                            prototype,
                            source,
                        },
                    );
                }
                Err(error) => {
                    errors.push(format!("{}->{}{}: {error}", triple.0, triple.1, triple.2))
                }
            }
        }
    }
    let accounted = methods.len() + errors.len() + skipped;
    if accounted != selected {
        eprintln!(
            " WARNING: {selected} methods were selected but only {accounted} are accounted for \
             ({} compiled, {} failed, {} without bytecode); {} vanished silently",
            methods.len(),
            errors.len(),
            skipped,
            selected.saturating_sub(accounted),
        );
    }
    Ok((methods, errors, skipped))
}

/// One method -> Rust source.
fn compile_method(class: &SmaliClass, method: &SmaliMethod, options: &Options) -> Result<String> {
    let mut input = dex2rs_core::dexinput::decode_method(
        class,
        method,
        options.obfuscate,
        options.dynamic_register,
    )
    .map_err(anyhow::Error::msg)?;
    input.trace = options.trace;
    let ir_method = dex2rs_core::ssa::IrBuilder::new(&input)
        .process()
        .map_err(anyhow::Error::msg)?;
    ir_method.get_source().map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--opt-level` is validated before it reaches cargo: a typo would otherwise
    /// fail a three-minute NDK build with an error about the manifest rather than
    /// about the flag that caused it.
    #[test]
    fn opt_level_is_validated_and_config_can_supply_it() {
        // The CLI wins, the config fills in, and the default is the small build.
        assert_eq!(opt_level(Some("3".into()), Some("z".into())).unwrap(), "3");
        assert_eq!(opt_level(Some("z".into()), Some("3".into())).unwrap(), "z");
        assert_eq!(opt_level(None, Some("3".into())).unwrap(), "3");
        assert_eq!(opt_level(None, None).unwrap(), DEFAULT_OPT_LEVEL);
        assert_eq!(opt_level(Some("s".into()), None).unwrap(), "s");
        // An empty config value is "unset", not a level.
        assert_eq!(
            opt_level(None, Some(String::new())).unwrap(),
            DEFAULT_OPT_LEVEL
        );

        assert!(opt_level(Some("fast".into()), None).is_err());
        assert!(opt_level(Some("".into()), None).is_err());
        assert!(opt_level(Some("4".into()), None).is_err());
    }
}
