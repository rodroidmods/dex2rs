//! Cross-compilation: `cargo ndk` replaces `ndk-build` + `Android.mk` /
//! `Application.mk`.
//!
//! The original build produced one `lib<name>.so` per `APP_ABI`; here
//! `cargo ndk -t <abi> -o libs build --release` does the same and writes the
//! libraries under `libs/<abi>/`, which is where `copy_compiled_libs` expects
//! them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The ABIs of the original `Application.mk`, in case the input has no libs.
pub const DEFAULT_ABIS: [&str; 2] = ["arm64-v8a", "armeabi-v7a"];

/// Map an Android ABI to the rust target triple (`cargo ndk -t` accepts both).
pub fn rust_target(abi: &str) -> Option<&'static str> {
    Some(match abi {
        "arm64-v8a" => "aarch64-linux-android",
        "armeabi-v7a" => "armv7-linux-androideabi",
        "x86" => "i686-linux-android",
        "x86_64" => "x86_64-linux-android",
        _ => return None,
    })
}

/// Is this an ABI the tool knows how to build?
pub fn is_supported(abi: &str) -> bool {
    rust_target(abi).is_some()
}

/// Build `project_dir` for every ABI and return `(abi, path to lib<name>.so)`.
pub fn build_project(
    project_dir: &Path,
    lib_name: &str,
    abis: &[String],
    ndk_dir: Option<&str>,
    min_sdk: u32,
) -> Result<Vec<(String, PathBuf)>> {
    // `cargo ndk -o` resolves a relative path against the crate directory, so
    // work with absolute paths from here on.
    let project_dir = project_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", project_dir.display()))?;
    let out_dir = project_dir.join("libs");
    let mut command = Command::new("cargo");
    command.arg("ndk");
    for abi in abis {
        let target = rust_target(abi)
            .with_context(|| format!("ABI '{abi}' is not supported (use --force-keep-libs?): supported ABIs are arm64-v8a, armeabi-v7a, x86, x86_64"))?;
        command.arg("-t").arg(target);
    }
    command
        .arg("-o")
        .arg(&out_dir)
        .arg("--platform")
        .arg(min_sdk.to_string())
        .arg("build")
        .arg("--release")
        .current_dir(&project_dir);
    if let Some(ndk) = ndk_dir {
        command
            .env("ANDROID_NDK_HOME", ndk)
            .env("ANDROID_NDK_ROOT", ndk);
    }

    eprintln!(
        " Building {} for {} ...",
        project_dir.display(),
        abis.join(", ")
    );
    let _ = &project_dir;
    let status = command
        .status()
        .context("running `cargo ndk` (is cargo-ndk installed?)")?;
    if !status.success() {
        bail!("cargo ndk build failed with {status}");
    }

    let file_name = format!("lib{lib_name}.so");
    let mut built = Vec::new();
    for abi in abis {
        let so = out_dir.join(abi).join(&file_name);
        if !so.exists() {
            bail!("the build did not produce {}", so.display());
        }
        built.push((abi.clone(), so));
    }
    // Which sources these libraries came from, so a later `--no-build` can tell
    // whether they still match the dex it is about to patch. Failure to write
    // the record is not a build failure: it only downgrades the next
    // `--no-build` run to "cannot check" instead of "matches".
    if let Err(error) = crate::scaffold::record_build(&project_dir) {
        eprintln!(" Could not record the build fingerprint: {error:#}");
    }
    Ok(built)
}

/// ABIs found in the APK's `lib/` entries (dcc.py's
/// `get_dex_files_and_adjust_mk_files`). `armeabi` is mapped to `armeabi-v7a`.
pub fn abis_from_apk(entries: impl Iterator<Item = String>) -> Result<Vec<String>> {
    let mut abis = Vec::new();
    for name in entries {
        let Some(rest) = name.strip_prefix("lib/") else {
            continue;
        };
        let mut parts = rest.split('/');
        let (Some(abi), Some(_file)) = (parts.next(), parts.next()) else {
            continue;
        };
        if abi == "armeabi" {
            eprintln!(" ABI 'armeabi' is deprecated, using 'armeabi-v7a' instead");
            if !abis.iter().any(|a| a == "armeabi-v7a") {
                abis.push("armeabi-v7a".to_string());
            }
            continue;
        }
        if !is_supported(abi) {
            bail!(
                "ABI '{abi}' is unsupported, please remove it from the apk or use --force-keep-libs"
            );
        }
        if !abis.iter().any(|a| a == abi) {
            abis.push(abi.to_string());
        }
    }
    Ok(abis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_mapping() {
        assert_eq!(rust_target("arm64-v8a"), Some("aarch64-linux-android"));
        assert_eq!(rust_target("armeabi-v7a"), Some("armv7-linux-androideabi"));
        assert!(!is_supported("riscv64"));
    }

    #[test]
    fn apk_abis_are_collected_in_order() {
        let names = [
            "lib/arm64-v8a/libfoo.so".to_string(),
            "lib/arm64-v8a/libbar.so".to_string(),
            "lib/x86_64/libfoo.so".to_string(),
            "classes.dex".to_string(),
            "lib/armeabi/libfoo.so".to_string(),
        ];
        assert_eq!(
            abis_from_apk(names.into_iter()).unwrap(),
            vec!["arm64-v8a", "x86_64", "armeabi-v7a"]
        );
    }

    #[test]
    fn unknown_abi_is_rejected() {
        let names = ["lib/riscv64/libfoo.so".to_string()];
        assert!(abis_from_apk(names.into_iter()).is_err());
    }
}
