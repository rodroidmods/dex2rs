//! `dcc.cfg` handling — the same file, keys and defaults as the Python tool.
//!
//! dcc.py reads the config with `json.load` and then, for every CLI argument left
//! at its default, lets a truthy config value win. [`DccConfig::merge_*`] helpers
//! reproduce that rule.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The keystore d2c signs with by default, relative to the working directory.
///
/// Named here rather than inline because signing treats it specially: it is the
/// one path that falls back to the copy embedded in the binary when the file is
/// not there (see `sign::keystore`).
pub const DEFAULT_KEYSTORE: &str = "keystore/debug.keystore";

/// `"signature"` block of `dcc.cfg`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SignatureConfig {
    pub keystore_path: String,
    pub alias: String,
    pub keystore_pass: String,
    pub store_pass: String,
    pub v1_enabled: bool,
    pub v2_enabled: bool,
    pub v3_enabled: bool,
}

impl Default for SignatureConfig {
    fn default() -> Self {
        SignatureConfig {
            keystore_path: DEFAULT_KEYSTORE.to_string(),
            alias: "androiddebugkey".to_string(),
            keystore_pass: "android".to_string(),
            store_pass: "android".to_string(),
            v1_enabled: true,
            v2_enabled: true,
            v3_enabled: true,
        }
    }
}

/// A parsed `dcc.cfg`. Every key is optional so a short config file is valid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DccConfig {
    /// Path to the Android NDK; exported as `ANDROID_NDK_HOME` for `cargo ndk`.
    pub ndk_dir: Option<String>,
    pub signature: SignatureConfig,
    pub output: Option<String>,
    pub obfuscate: bool,
    /// Emit `LOGD`-style traces for every instruction (large output).
    pub trace: bool,
    pub dynamic_register: bool,
    pub skip_synthetic: bool,
    pub allow_init: bool,
    pub force_keep_libs: bool,
    pub no_build: bool,
    /// Keep the debug tables in the output dex (dropped by default).
    pub keep_debug_info: bool,
    /// Deflate `lib/**/*.so` in the output APK instead of storing it. Costs
    /// nothing at run time but moves the native libraries from "mapped out of
    /// the APK" to "extracted at install", so the manifest has to agree
    /// (see `apk::set_extract_native_libs`). Off by default because the stored
    /// layout is what the generated APK has always shipped.
    pub compress_libs: bool,
    pub filter: Option<String>,
    pub custom_loader: Option<String>,
    pub force_custom_loader: bool,
    pub lib_name: Option<String>,
    pub source_dir: Option<String>,
    pub project_archive: Option<String>,
    /// ABIs to build when the input carries none (default: `arm64-v8a`,
    /// `armeabi-v7a` — the two `APP_ABI` values of the original `Application.mk`).
    pub abis: Option<Vec<String>>,
    /// API level for `cargo ndk`; defaults to the input's `minSdkVersion`.
    pub min_sdk: Option<u32>,
    /// `opt-level` for the generated library's release profile: `"z"` (the
    /// default) or `"3"`.
    ///
    /// Not a taste setting. On the reference app `"3"` gives a 12.37 MB library
    /// and `"z"` 7.64 MB, and the difference is mostly **not** inlining: building
    /// `"3"` with the inliner off saves 1.32 MB, while the machine outliner that
    /// `"z"` runs and `"3"` does not is the other 4.19 MB. `"2"` and `"s"`
    /// measured *larger* than `"3"`, so there are two useful values rather than a
    /// spectrum. See [`Self::outline_common`].
    pub opt_level: Option<String>,
    /// Ask for LLVM's machine outliner at any `opt-level` (`--outline-common`).
    ///
    /// The generated code repeats the same instruction sequences once per JNI
    /// entry point; the outliner folds them into shared copies. On the reference
    /// app that is 12.37 MB → 8.81 MB of `libstub.so` at `opt-level 3`, against
    /// 7.64 MB for `"z"`, and everything outside the outlined fragments stays
    /// `-O3`. Off by default: it trades a call for code that was inline.
    pub outline_common: bool,
}

impl DccConfig {
    /// Read `path`; a missing file is not an error (dcc.py behaves the same way:
    /// it simply leaves the CLI defaults in place).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(DccConfig::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(DccConfig::default());
        }
        let config: DccConfig = serde_json::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(config)
    }

    /// dcc.py's rule for flags: either side can enable, `false` never disables.
    pub fn flag_or(cli: bool, config: bool) -> bool {
        cli || config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_merges_like_dcc_py() {
        assert!(DccConfig::flag_or(false, true));
        assert!(DccConfig::flag_or(true, false));
        assert!(!DccConfig::flag_or(false, false));
    }

    #[test]
    fn missing_keys_are_defaulted() {
        let config: DccConfig = serde_json::from_str(r#"{"lib_name":"mylib"}"#).unwrap();
        assert_eq!(config.lib_name.as_deref(), Some("mylib"));
        assert_eq!(config.signature.alias, "androiddebugkey");
        assert!(config.signature.v2_enabled);
    }
}
